//! `vibeup tui` —— 三个标签:**键盘** / **手表**(BLE 设备配置)+ **管理**
//! (vibetty / vibekeys 的安装更新)。
//!
//! Tab 或 1/2/3 切标签。设备页是连接流程:进入 tab 自动后台扫描(界面转圈不
//! 冻结),扫到后列出候选设备让用户挑,选中连接成功才进入配置界面(见 `Conn`)。
//! 配置界面按功能分节(server / wifi / asr / …),每节只有一条上边框带节名;每行
//! 是「动作 + 当前值」,选中后回车:可编辑项原地变成编辑框,复杂输入(加 WiFi)
//! 弹居中的 pop 框分字段填写,立即执行的项直接跑。
//! wifi 节是动态列表:现有网络每条一行(回车改密码,`d` 删除),末尾固定一行
//! `add +` 弹框新增。连接成功后持有常驻 BLE 会话(`ble::Session`,后台线程),
//! 设备动作瞬时完成、断连自动弹窗(vibekeys_app 式检测);扫描/安装这类要打
//! stdout 或耗时的动作先退出 TUI 模式,结束后再回来(见 `suspend`)。demo
//! 模式用内存假数据和假设备名走完同样的流程,不碰蓝牙。

use crate::config_cmd;
use crate::install;
use crate::pkg::{self, PKGS, Pkg};
use anyhow::{Result, bail};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row as TRow, Table};
use ratatui::{DefaultTerminal, Frame};
use std::path::PathBuf;

// ===== 管理标签(包)=====

struct Entry {
    pkg: &'static Pkg,
    installed: Option<String>,
    latest: Option<String>, // 最新稳定版
    any: Option<String>,    // 最新 release(含 prerelease)
    note: Option<String>,   // 查询失败原因
}

impl Entry {
    fn new(pkg: &'static Pkg) -> Self {
        Self {
            pkg,
            installed: None,
            latest: None,
            any: None,
            note: None,
        }
    }

    fn status(&self, allow_pre: bool) -> &'static str {
        if self.note.is_some() {
            return "lookup failed";
        }
        let target = self.target(allow_pre);
        match (&self.installed, target) {
            (Some(_), None) => "up to date",
            (None, _) => "not installed",
            (Some(v), Some(t)) if pkg::normalize(v) == pkg::normalize(&t) => "up to date",
            (Some(_), Some(_)) => "update available",
        }
    }

    /// 本次会更新到的 tag:pre 开 → 最新 release,否则最新稳定版。
    fn target(&self, allow_pre: bool) -> Option<String> {
        match allow_pre {
            true => self.any.clone(),
            false => self.latest.clone(),
        }
    }
}

fn fetch(entries: &mut [Entry], dir: &std::path::Path) {
    for e in entries.iter_mut() {
        e.installed = pkg::installed_version(e.pkg, dir);
        match pkg::releases(e.pkg.repo) {
            Ok((latest, any)) => {
                e.latest = Some(latest);
                e.any = any;
                e.note = None;
            }
            Err(err) => e.note = Some(format!("{err:#}")),
        }
    }
}

// ===== 设备标签 =====

fn sample_config(kind: crate::ble::DeviceKind) -> serde_json::Value {
    use crate::ble::DeviceKind;
    let base = serde_json::json!({
        "wifi_list": [
            {"ssid": "Home", "pass": "horse-battery"},
            {"ssid": "Office", "pass": "correct-horse"}
        ],
        "server_url": "mqtt://127.0.0.1:1883",
        "asr_config": {
            "platform": "whisper",
            "uri": "https://api.openai.com/v1/audio/transcriptions",
            "api_key": "sk-demo",
            "model": "whisper-1"
        },
    });
    if kind == DeviceKind::VibeKeys {
        let mut v = base;
        v["mic_model"] = serde_json::json!(1);
        v["prefer_builtin_asr"] = serde_json::json!(true);
        v
    } else {
        base
    }
}

// ===== demo 动作:把对应操作直接落在假配置上 =====

fn mock_wifi_add(mock: &mut serde_json::Value, ssid: &str, pass: &str) {
    let list = mock["wifi_list"].as_array_mut().unwrap();
    match list.iter_mut().find(|e| e["ssid"] == ssid) {
        Some(e) => e["pass"] = pass.into(),
        None => list.push(serde_json::json!({"ssid": ssid, "pass": pass})),
    }
}

fn mock_wifi_remove(mock: &mut serde_json::Value, ssid: &str) {
    mock["wifi_list"]
        .as_array_mut()
        .unwrap()
        .retain(|e| e["ssid"] != ssid);
}

fn mock_asr_set(mock: &mut serde_json::Value, uri: Option<&str>, api_key: Option<&str>, model: Option<&str>) {
    let asr = mock.get_mut("asr_config").unwrap();
    if let Some(v) = uri {
        asr["uri"] = v.into();
    }
    if let Some(v) = api_key {
        asr["api_key"] = v.into();
    }
    if let Some(v) = model {
        asr["model"] = v.into();
    }
}

fn mock_keymap(mock: &mut serde_json::Value, key: &str, binding: &str) {
    let physical = config_cmd::physical_key_name(key);
    let parsed = config_cmd::parse_key_binding(binding);
    mock["keymap"][&physical] = parsed;
}

// ===== 连接流程:进入 tab → 后台扫描 → 列表选设备 → 连接 → 配置界面 =====

/// 设备页连接状态。
#[derive(Clone, PartialEq, Debug)]
enum Conn {
    /// 还没进入过(进入 tab 时弹入口菜单)。
    Fresh,
    /// 入口菜单:1 扫描设备 / 2 从网页配置。
    Menu,
    /// 网页配置:显示 setup 页地址(`opened` = 浏览器是否已尝试打开)。
    WebSetup { url: &'static str },
    /// 后台扫描中。
    Scanning,
    /// 扫到候选设备,等用户挑(列表在 `DeviceTab::pick`)。
    Pick,
    /// 正在连接(后台),存设备名供提示。
    Connecting(String),
    /// 已连上,配置界面就绪。
    Ready,
    /// 扫描/连接失败,可重试。
    Failed(String),
    /// 连上之后设备断开了(动作/重读报断连类错误):弹窗等回车,回扫描页。
    Disconnected,
}

/// 后台 BLE 任务:扫描 2 秒上下、连接也要几秒,放线程里跑,结果经 channel
/// 回来,TUI 事件循环轮询——界面不冻结,还能画转圈动画。
struct Job {
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    rx: std::sync::mpsc::Receiver<JobOut>,
}

enum JobOut {
    Scan(anyhow::Result<Vec<crate::ble::FoundDevice>>),
    Config(anyhow::Result<serde_json::Value>),
}

fn spawn_job(f: impl FnOnce() -> JobOut + Send + 'static) -> Job {
    use std::sync::atomic::AtomicBool;
    let (tx, rx) = std::sync::mpsc::channel();
    let done = std::sync::Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    std::thread::spawn(move || {
        let _ = tx.send(f());
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    Job { done, rx }
}

/// demo 假设备广播名:仿真机形如 `VibeKeys-XXXX`。
fn demo_devices(kind: crate::ble::DeviceKind) -> Vec<crate::ble::FoundDevice> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let suffixes = [(nanos & 0xffff) as u16, ((nanos >> 8) & 0xffff) as u16];
    suffixes
        .into_iter()
        .map(|s| crate::ble::FoundDevice {
            name: format!("{}-{s:04X}", kind.name_prefix()),
            id: format!("demo-{s:04X}"),
        })
        .collect()
}

/// demo 扫描:假装扫了 0.9s,回两台假设备。
fn demo_scan_job(kind: crate::ble::DeviceKind) -> Job {
    spawn_job(move || {
        std::thread::sleep(std::time::Duration::from_millis(900));
        JobOut::Scan(Ok(demo_devices(kind)))
    })
}

/// demo 连接:假装连了 0.4s,回样例配置。
fn demo_connect_job(kind: crate::ble::DeviceKind) -> Job {
    spawn_job(move || {
        std::thread::sleep(std::time::Duration::from_millis(400));
        JobOut::Config(Ok(sample_config(kind)))
    })
}

/// 可编辑字段:决定行内编辑提交时调用哪个动作。
#[derive(Clone, Copy, PartialEq)]
enum Field {
    ServerUrl,
    AsrUri,
    AsrApiKey,
    AsrModel,
    MicModel,
    PreferAsr,
    Keymap,
    Tailscale,
}

/// 编辑框的格式提示。
fn field_hint(field: Field) -> &'static str {
    match field {
        Field::ServerUrl => "mqtt://user:pass@host:1883",
        Field::AsrUri => "https://…/audio/transcriptions",
        Field::AsrApiKey => "api key",
        Field::AsrModel => "whisper-1",
        Field::MicModel => "ptt | toggle",
        Field::PreferAsr => "on | off",
        Field::Keymap => "KEY=BINDING, e.g. CUSTOM=\"/compact\\n\"",
        Field::Tailscale => "tskey-auth-…",
    }
}

/// 枚举选项的下一个值:当前值在列表里则取其后一个(循环),不在/为空则取第一个。
fn next_in_cycle<'a>(values: &'a [&'a str], current: &str) -> &'a str {
    match values.iter().position(|v| v.eq_ignore_ascii_case(current)) {
        Some(i) => values[(i + 1) % values.len()],
        None => values[0],
    }
}

/// 从 CONFIG 快照取一个字段的展示值(走点路径,缺失 = "")。
fn cfg_get(v: &serde_json::Value, path: &[&str]) -> String {
    let mut cur = v;
    for p in path {
        match cur.get(p) {
            Some(next) => cur = next,
            None => return String::new(),
        }
    }
    match cur {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn asr_cfg(v: &serde_json::Value, key: &str) -> String {
    cfg_get(v, &["asr_config", key])
}

/// 带光标的单行输入:支持左右/Home/End 移动、任意位置插入、前后删除。
#[derive(Default)]
struct Input {
    buf: String,
    /// 光标位置(字符下标,不是字节)。
    pos: usize,
}

impl Input {
    fn with_str(s: &str) -> Self {
        Self {
            buf: s.to_string(),
            pos: s.chars().count(),
        }
    }

    fn chars(&self) -> usize {
        self.buf.chars().count()
    }

    /// 光标的字节偏移。
    fn byte_pos(&self) -> usize {
        self.buf
            .char_indices()
            .nth(self.pos)
            .map(|(i, _)| i)
            .unwrap_or(self.buf.len())
    }

    fn left(&mut self) {
        self.pos = self.pos.saturating_sub(1);
    }

    fn right(&mut self) {
        if self.pos < self.chars() {
            self.pos += 1;
        }
    }

    fn home(&mut self) {
        self.pos = 0;
    }

    fn end(&mut self) {
        self.pos = self.chars();
    }

    fn insert(&mut self, c: char) {
        let b = self.byte_pos();
        self.buf.insert(b, c);
        self.pos += 1;
    }

    /// 删光标前的字符(Backspace)。
    fn backspace(&mut self) {
        if self.pos == 0 {
            return;
        }
        let b = self.byte_pos();
        let prev = self.buf[..b]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.buf.replace_range(prev..b, "");
        self.pos -= 1;
    }

    /// 删光标处的字符(Delete)。
    fn delete(&mut self) {
        let b = self.byte_pos();
        let Some(c) = self.buf[b..].chars().next() else {
            return;
        };
        self.buf.replace_range(b..b + c.len_utf8(), "");
    }

    /// 渲染为 spans。`cursor` 时光标处字符用反显(粗块光标)包住;光标在末尾时
    /// 画一个反显空格块。
    fn spans(&self, cursor: bool, base: Style) -> Vec<Span<'static>> {
        if !cursor {
            return vec![Span::styled(self.buf.clone(), base)];
        }
        let b = self.byte_pos();
        let mut spans = vec![Span::styled(self.buf[..b].to_string(), base)];
        match self.buf[b..].chars().next() {
            Some(c) => {
                spans.push(Span::styled(
                    c.to_string(),
                    base.add_modifier(Modifier::REVERSED)
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(self.buf[b + c.len_utf8()..].to_string(), base));
            }
            None => spans.push(Span::styled(
                " ",
                base.bg(Color::LightBlue).add_modifier(Modifier::BOLD),
            )),
        }
        spans
    }
}

/// pop 框种类。提交动作不用闭包:动作就两个(wifi 增/改),主循环按 kind
/// 直接分发,行数据保持纯数据。
#[derive(Clone)]
enum PopupKind {
    /// 新增 WiFi:ssid + pass 两个字段。
    WifiAdd,
    /// 改某条 WiFi 的密码。
    WifiPass { ssid: String },
}

impl PopupKind {
    /// (标题, 字段名列表)。
    fn spec(&self) -> (&'static str, &'static [&'static str]) {
        match self {
            Self::WifiAdd => (" add wifi ", &["ssid", "pass"]),
            Self::WifiPass { .. } => (" wifi: change password ", &["pass"]),
        }
    }
}

#[derive(Clone)]
enum RowAction {
    /// 清空 ASR 配置。
    AsrClear,
    /// 设备重启使已存配置生效。
    ResetDevice,
    /// 行内单行编辑(选中回车,该行变编辑框)。
    Edit(Field),
    /// 枚举选项:回车直接切到下一个值,不进编辑框。
    Cycle {
        field: Field,
        values: Vec<&'static str>,
    },
    /// 回车弹 pop 框分字段填写。
    Popup(PopupKind),
    /// 一条现有 WiFi:回车弹框改密码,`d` 删除。
    WifiEntry { ssid: String },
}

/// 设备页一行:所属节 + 标签 + 值 + 动作。行随配置快照重建(wifi 列表是动态的)。
struct DevRow {
    section: &'static str,
    label: String,
    value: String,
    action: RowAction,
}

impl DevRow {
    fn edit(section: &'static str, label: &str, field: Field, value: String) -> Self {
        Self {
            section,
            label: label.to_string(),
            value,
            action: RowAction::Edit(field),
        }
    }
}

struct DeviceTab {
    kind: crate::ble::DeviceKind,
    /// 连接流程状态:进 tab 扫描 → 选设备 → 连接 → Ready 后才是配置界面。
    conn: Conn,
    /// 扫描出的候选设备(Pick 状态下的选择列表)。
    pick: Vec<crate::ble::FoundDevice>,
    pick_sel: usize,
    /// 入口菜单的选中项(0 = 扫描设备,1 = 从网页配置)。
    menu_sel: usize,
    config: Option<serde_json::Value>,
    /// 选中的设备(Ready 后所有动作和重读都发给它,顶部显示名字)。
    connected: Option<Connected>,
    /// 正在连接的设备,连接成功后转正为 `connected`。
    pending: Option<Connected>,
    rows: Vec<DevRow>,
    sel: usize,
    /// demo 模式:不碰 BLE,动作改内存里的假配置,扫描/连接用假设备走流程。
    demo: bool,
    mock: serde_json::Value,
    /// 行内编辑:选中行下标 + 当前输入。
    editing: Option<(usize, Input)>,
    /// 打开的 pop 框。
    popup: Option<PopupState>,
    /// 进行中的后台扫描/连接。
    job: Option<Job>,
    /// 常驻 BLE 会话(真机 Ready 后持有,动作走它,断连事件从它来)。
    session: Option<crate::ble::Session>,
    /// 转圈动画帧计数(事件循环空转时递增)。
    spin: usize,
}

/// 已连上/正在连的设备:名字给界面顶部显示,连接本体在 `session` 里。
struct Connected {
    name: String,
}

/// 运行中的 pop 框:字段展开成带光标的输入,提交按 kind 分发。
struct PopupState {
    kind: PopupKind,
    title: String,
    labels: Vec<&'static str>,
    inputs: Vec<Input>,
    cur: usize,
}

impl PopupState {
    /// `initials` 按字段顺序给初始值(改密码预填旧密码;新增 WiFi 全空)。
    fn new(kind: PopupKind, initials: Vec<String>) -> Self {
        let (title, labels) = kind.spec();
        Self {
            kind,
            title: title.into(),
            labels: labels.to_vec(),
            inputs: labels
                .iter()
                .enumerate()
                .map(|(i, _)| Input::with_str(initials.get(i).map(String::as_str).unwrap_or("")))
                .collect(),
            cur: 0,
        }
    }

    /// 当前字段的光标 overlay(反显粗块,包住光标处字符;末尾则是色块)。
    fn cursor_spans(&self, i: usize, base: Style) -> Vec<Span<'static>> {
        if i != self.cur {
            return vec![];
        }
        let input = &self.inputs[i];
        let b = input.byte_pos();
        match input.buf[b..].chars().next() {
            Some(c) => vec![Span::styled(
                c.to_string(),
                base.add_modifier(Modifier::REVERSED)
                    .add_modifier(Modifier::BOLD),
            )],
            None => vec![Span::styled(
                " ",
                base.bg(Color::LightBlue).add_modifier(Modifier::BOLD),
            )],
        }
    }
}

impl DeviceTab {
    fn new(kind: crate::ble::DeviceKind, demo: bool) -> Self {
        let mut t = Self {
            kind,
            conn: Conn::Fresh,
            pick: Vec::new(),
            pick_sel: 0,
            menu_sel: 0,
            config: None,
            connected: None,
            pending: None,
            rows: Vec::new(),
            sel: 0,
            demo,
            mock: sample_config(kind),
            editing: None,
            popup: None,
            job: None,
            session: None,
            spin: 0,
        };
        t.rebuild_rows();
        t
    }

    /// 常驻会话(真机动作走它)。
    fn session(&self) -> Result<&crate::ble::Session> {
        self.session
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("device session is closed"))
    }

    /// 提交一个编辑值:demo 落假配置,真机走常驻会话(瞬时,不重扫重连)。
    fn do_field(&mut self, field: Field, val: &str) -> Result<()> {
        let val = val.trim();
        match field {
            Field::ServerUrl => {
                if self.demo {
                    self.mock["server_url"] = val.into();
                    return Ok(());
                }
                self.session()?.write_config_field("server_url", val.into())
            }
            Field::AsrUri => {
                if self.demo {
                    mock_asr_set(&mut self.mock, Some(val), None, None);
                    return Ok(());
                }
                // 与 CLI `asr --uri` 同款:只带 uri,其余置 null(固件忽略)。
                self.session()?.write_config_field(
                    "asr_config",
                    serde_json::json!({
                        "platform": "whisper",
                        "uri": val,
                        "api_key": serde_json::Value::Null,
                        "model": serde_json::Value::Null,
                    }),
                )
            }
            Field::AsrApiKey => {
                if self.demo {
                    mock_asr_set(&mut self.mock, None, Some(val), None);
                    return Ok(());
                }
                self.session()?.write_config_field(
                    "asr_config",
                    serde_json::json!({
                        "platform": "whisper",
                        "uri": serde_json::Value::Null,
                        "api_key": val,
                        "model": serde_json::Value::Null,
                    }),
                )
            }
            Field::AsrModel => {
                if self.demo {
                    mock_asr_set(&mut self.mock, None, None, Some(val));
                    return Ok(());
                }
                self.session()?.write_config_field(
                    "asr_config",
                    serde_json::json!({
                        "platform": "whisper",
                        "uri": serde_json::Value::Null,
                        "api_key": serde_json::Value::Null,
                        "model": val,
                    }),
                )
            }
            Field::MicModel => {
                if self.demo {
                    let v = match val.to_lowercase().as_str() {
                        "ptt" => 0,
                        "toggle" => 1,
                        other => bail!("invalid mic mode `{other}` — use ptt or toggle"),
                    };
                    self.mock["mic_model"] = serde_json::json!(v);
                    return Ok(());
                }
                let v = match val.to_lowercase().as_str() {
                    "ptt" => 0,
                    "toggle" => 1,
                    other => bail!("invalid mic mode `{other}` — use ptt or toggle"),
                };
                self.session()?
                    .write_config_field("mic_model", serde_json::json!(v))
            }
            Field::PreferAsr => {
                let on = match val.to_lowercase().as_str() {
                    "on" | "true" => true,
                    "off" | "false" => false,
                    other => bail!("invalid value `{other}` — use on or off"),
                };
                if self.demo {
                    self.mock["prefer_builtin_asr"] = serde_json::json!(on);
                    return Ok(());
                }
                self.session()?
                    .write_config_field("prefer_builtin_asr", serde_json::json!(on))
            }
            Field::Keymap => {
                let Some((key, binding)) = val.split_once('=') else {
                    bail!("expected `KEY=BINDING`, e.g. CUSTOM=\"/compact\\n\"");
                };
                if self.demo {
                    mock_keymap(&mut self.mock, key.trim(), binding.trim());
                    return Ok(());
                }
                let payload = config_cmd::build_keymap_config(key.trim(), binding.trim());
                self.session()?.write_keymap(&payload)
            }
            Field::Tailscale => {
                if self.demo {
                    self.mock["tailscale_key"] = val.into();
                    return Ok(());
                }
                if val.is_empty() {
                    bail!("empty tailscale auth key");
                }
                // 手表上这条写是独占的:固件存完立刻跳 Tailscale 页、断连 ——
                // 断连会以 Disconnected 事件到达,弹窗是预期表现。
                self.session()?
                    .write_config_field("tailscale_key", serde_json::Value::String(val.into()))
            }
        }
    }

    /// WiFi 增改(同 ssid = 更新密码):demo 落假配置,真机读-改-写整个列表
    /// (和 CLI `wifi add` 一致,顺序 = 优先级)。
    fn do_wifi_add(&mut self, ssid: &str, pass: &str) -> Result<()> {
        if ssid.trim().is_empty() {
            bail!("ssid must not be empty");
        }
        if self.demo {
            mock_wifi_add(&mut self.mock, ssid.trim(), pass);
            return Ok(());
        }
        let session = self.session()?;
        let list = crate::ble::parse_wifi_list(&serde_json::to_string(&session.read_config()?)?);
        let mut list = list;
        match list.iter_mut().find(|(s, _)| *s == ssid.trim()) {
            Some(e) => e.1 = pass.to_string(),
            None => list.push((ssid.trim().to_string(), pass.to_string())),
        }
        if list.len() > crate::ble::MAX_WIFI_CREDS {
            bail!(
                "wifi list is full ({}/{}): remove one first",
                list.len(),
                crate::ble::MAX_WIFI_CREDS
            );
        }
        session.write_config_field("wifi_list", crate::ble::wifi_list_to_json(&list))
    }

    /// WiFi 删除。
    fn do_wifi_remove(&mut self, ssid: &str) -> Result<()> {
        if self.demo {
            mock_wifi_remove(&mut self.mock, ssid);
            return Ok(());
        }
        let session = self.session()?;
        let list = crate::ble::parse_wifi_list(&serde_json::to_string(&session.read_config()?)?);
        let rest: Vec<_> = list.iter().filter(|(s, _)| *s != ssid).cloned().collect();
        if rest.len() == list.len() {
            bail!("wifi `{ssid}' not in the list");
        }
        session.write_config_field("wifi_list", crate::ble::wifi_list_to_json(&rest))
    }

    /// 清空 ASR 配置(与 vibekeys_app `asr-config --clean` 一致:全空串)。
    fn do_asr_clear(&mut self) -> Result<()> {
        if self.demo {
            self.mock["asr_config"] =
                serde_json::json!({"platform": "whisper", "uri": "", "api_key": "", "model": ""});
            return Ok(());
        }
        self.session()?.write_config_field(
            "asr_config",
            serde_json::json!({"platform": "whisper", "uri": "", "api_key": "", "model": ""}),
        )
    }

    /// 设备重启使已存配置生效。重启会掐断连接,断连事件随后到 → 弹窗,
    /// 回车重扫即可重连。
    fn do_reset(&mut self) -> Result<()> {
        if self.demo {
            return Ok(());
        }
        self.session()?.reset()
    }

    /// 进入设备页:先弹入口菜单(扫描设备 / 从网页配置),不自动扫描。
    fn enter(&mut self) {
        if self.conn == Conn::Fresh {
            self.conn = Conn::Menu;
        }
    }

    /// 网页配置:切到地址弹窗,URL 由用户自己复制去浏览器打开。
    fn show_web_setup(&mut self) {
        let url = self.kind.setup_url();
        self.conn = Conn::WebSetup { url };
    }

    fn start_scan(&mut self) {
        // 旧任务直接弃:线程跑完自然退出,结果没人收而已。
        self.job = None;
        self.conn = Conn::Scanning;
        let kind = self.kind;
        self.job = Some(match self.demo {
            true => demo_scan_job(kind),
            false => spawn_job(move || JobOut::Scan(crate::ble::scan_devices(kind))),
        });
    }

    fn start_connect(&mut self, dev: crate::ble::FoundDevice) {
        self.job = None;
        self.session = None;
        self.pending = Some(Connected {
            name: dev.name.clone(),
        });
        self.conn = Conn::Connecting(dev.name);
        let kind = self.kind;
        match self.demo {
            // demo:假任务 0.4s 后回样例配置。
            true => self.job = Some(demo_connect_job(kind)),
            // 真机:起常驻会话,连接结果(Ready/Disconnected)从事件来。
            false => {
                let target = crate::ble::Target::picked(self.kind, dev.id.clone());
                self.session = Some(crate::ble::Session::open(target));
            }
        }
    }

    /// 轮询后台任务;完成则推进状态机,返回给 footer 的消息。
    fn poll_job(&mut self) -> Option<String> {
        let job = self.job.take()?;
        if !job.done.load(std::sync::atomic::Ordering::Relaxed) {
            self.job = Some(job);
            return None;
        }
        match job.rx.try_recv().ok() {
            Some(JobOut::Scan(res)) => Some(self.on_scan_result(res)),
            Some(JobOut::Config(res)) => Some(self.on_connect_result(res)),
            // done 置位却没有结果:线程 panic 了,极少见,按失败走。
            None => {
                self.conn = Conn::Failed("background task died".into());
                Some("background task died".into())
            }
        }
    }

    fn on_scan_result(&mut self, res: anyhow::Result<Vec<crate::ble::FoundDevice>>) -> String {
        match res {
            Ok(devs) if !devs.is_empty() => {
                self.pick = devs;
                self.pick_sel = 0;
                self.conn = Conn::Pick;
                format!("found {} device(s) — pick one", self.pick.len())
            }
            Ok(_) => {
                self.conn = Conn::Failed("no devices found".into());
                "no devices found".into()
            }
            Err(e) => {
                let msg = format!("{e:#}");
                self.conn = Conn::Failed(msg.clone());
                msg
            }
        }
    }

    fn on_connect_result(&mut self, res: anyhow::Result<serde_json::Value>) -> String {
        match res {
            Ok(cfg) => {
                self.config = Some(cfg);
                self.connected = Some(self.pending.take().unwrap_or_else(|| Connected {
                    name: self.kind.name().into(),
                }));
                self.rebuild_rows();
                self.sel = self.sel.min(self.rows.len() - 1);
                self.conn = Conn::Ready;
                "connected".into()
            }
            Err(e) => {
                let msg = format!("{e:#}");
                self.conn = Conn::Failed(msg.clone());
                msg
            }
        }
    }

    /// 轮询常驻会话事件(连接建立/断连)。断连按所处阶段分流:Ready → 弹窗;
    /// 还在连接中 → 失败屏。
    fn poll_session(&mut self) -> Option<String> {
        let ev = self.session.as_ref()?.try_event()?;
        Some(self.on_session_event(ev))
    }

    fn on_session_event(&mut self, ev: crate::ble::SessionEvent) -> String {
        match ev {
            crate::ble::SessionEvent::Ready(res) => self.on_connect_result(res),
            crate::ble::SessionEvent::Disconnected(e) => {
                self.session = None;
                if self.conn == Conn::Ready {
                    self.editing = None;
                    self.popup = None;
                    self.conn = Conn::Disconnected;
                    format!("device disconnected: {e:#}")
                } else {
                    // 连接还没建立就断了:走失败屏。
                    self.on_connect_result(Err(e))
                }
            }
        }
    }

    /// 按当前配置快照重建行(wifi 列表是动态的,其余节的值也一并刷新)。
    /// 行是纯数据:动作在主循环里按 RowAction 分发到 `do_*` 方法。
    fn rebuild_rows(&mut self) {
        let empty = serde_json::json!({});
        let cfg = self.config.as_ref().unwrap_or(&empty);
        let keys = self.kind == crate::ble::DeviceKind::VibeKeys;
        let mut rows: Vec<DevRow> = Vec::new();

        rows.push(DevRow::edit(
            "server",
            "server-url",
            Field::ServerUrl,
            cfg_get(cfg, &["server_url"]),
        ));

        // wifi:现有网络每条一行(回车改密码,d 删除),末尾固定 add +。
        for e in cfg
            .get("wifi_list")
            .and_then(|v| v.as_array())
            .unwrap_or(&vec![])
        {
            let ssid = e
                .get("ssid")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let pass = e
                .get("pass")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            rows.push(DevRow {
                section: "wifi",
                label: ssid.clone(),
                value: if pass.is_empty() {
                    "(open)".into()
                } else {
                    "••••••".into()
                },
                action: RowAction::WifiEntry { ssid: ssid.clone() },
            });
        }
        rows.push(DevRow {
            section: "wifi",
            label: "add +".into(),
            value: String::new(),
            action: RowAction::Popup(PopupKind::WifiAdd),
        });

        rows.push(DevRow::edit(
            "asr",
            "uri",
            Field::AsrUri,
            asr_cfg(cfg, "uri"),
        ));
        rows.push(DevRow::edit(
            "asr",
            "api-key",
            Field::AsrApiKey,
            asr_cfg(cfg, "api_key"),
        ));
        rows.push(DevRow::edit(
            "asr",
            "model",
            Field::AsrModel,
            asr_cfg(cfg, "model"),
        ));
        rows.push(DevRow {
            section: "asr",
            label: "clear".into(),
            value: String::new(),
            action: RowAction::AsrClear,
        });

        if keys {
            rows.push(DevRow {
                section: "keyboard",
                label: "mic-model".into(),
                value: match cfg.get("mic_model").and_then(|v| v.as_u64()) {
                    Some(0) => "ptt".into(),
                    Some(1) => "toggle".into(),
                    _ => String::new(),
                },
                action: RowAction::Cycle {
                    field: Field::MicModel,
                    values: vec!["ptt", "toggle"],
                },
            });
            rows.push(DevRow {
                section: "keyboard",
                label: "prefer-asr".into(),
                value: match cfg.get("prefer_builtin_asr").and_then(|v| v.as_bool()) {
                    Some(true) => "on".into(),
                    Some(false) => "off".into(),
                    _ => String::new(),
                },
                action: RowAction::Cycle {
                    field: Field::PreferAsr,
                    values: vec!["on", "off"],
                },
            });
            rows.push(DevRow::edit(
                "keyboard",
                "keymap (KEY=BINDING)",
                Field::Keymap,
                match cfg.get("keymap").and_then(|v| v.as_object()) {
                    Some(m) if !m.is_empty() => m
                        .iter()
                        .map(|(k, val)| format!("{k}={val}"))
                        .collect::<Vec<_>>()
                        .join("  "),
                    _ => String::new(),
                },
            ));
        }

        rows.push(DevRow::edit(
            "device",
            "tailscale: bind",
            Field::Tailscale,
            {
                // 真机快照不含 tailscale_key(仅写);demo 里改过才显示。
                let v = cfg_get(cfg, &["tailscale_key"]);
                if v.is_empty() { v } else { "(set)".into() }
            },
        ));
        rows.push(DevRow {
            section: "device",
            label: "reset device".into(),
            value: String::new(),
            action: RowAction::ResetDevice,
        });

        self.sel = self.sel.min(rows.len() - 1);
        self.rows = rows;
    }

    /// 重读配置(r 键、动作成功后)。demo 读内存假数据;真机走常驻会话,
    /// 瞬时完成。返回错误消息,None = 成功;失败保留旧快照,界面不塌。
    /// 断连不走这里 —— 会话会推 Disconnected 事件,由 `on_session_event` 弹窗。
    fn load_config(&mut self) -> Option<String> {
        if self.demo {
            self.config = Some(self.mock.clone());
            self.rebuild_rows();
            return None;
        }
        let session = self.session.as_ref()?;
        match session.read_config() {
            Ok(cfg) => {
                self.config = Some(cfg);
                self.rebuild_rows();
                None
            }
            Err(e) => Some(format!("{e:#}")),
        }
    }
}

// ===== 标签 =====

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Keys,
    Watch,
    Pkgs,
}

impl Tab {
    fn all() -> [Tab; 3] {
        [Tab::Keys, Tab::Watch, Tab::Pkgs]
    }

    fn title(self) -> &'static str {
        match self {
            Tab::Keys => "Keyboard",
            Tab::Watch => "Watch",
            Tab::Pkgs => "Packages",
        }
    }

    fn next(self) -> Self {
        let all = Self::all();
        let i = all.iter().position(|t| *t == self).unwrap_or(0);
        all[(i + 1) % all.len()]
    }
}

pub fn run(dir: Option<PathBuf>, dry_run: bool, demo: bool) -> Result<()> {
    let dir = pkg::install_dir(dir.as_deref());
    let mut pkg_entries: Vec<Entry> = PKGS.iter().map(Entry::new).collect();
    let mut pkg_sel = 0usize;
    let mut allow_pre = false;
    let mut keys_tab = DeviceTab::new(crate::ble::DeviceKind::VibeKeys, demo);
    let mut watch_tab = DeviceTab::new(crate::ble::DeviceKind::Watch, demo);
    // 默认停在「管理」:不碰蓝牙,启动零延迟;设备页进了才连。
    let mut tab = Tab::Pkgs;
    let mut message = String::from("Tab tabs · ↑↓ select · Enter run/edit · r refresh · q quit");
    fetch(&mut pkg_entries, &dir);

    let mut terminal = ratatui::init();
    // 启动前排空 tty 里残留的旧按键(比如上一次会话漏下的 `q`),否则一进
    // 主循环就被当成快捷键直接退出。
    while event::poll(std::time::Duration::ZERO).unwrap_or(false) {
        let _ = event::read();
    }
    loop {
        let editing = match tab {
            Tab::Keys => keys_tab.editing.as_ref(),
            Tab::Watch => watch_tab.editing.as_ref(),
            Tab::Pkgs => None,
        };
        terminal.draw(|f| {
            ui(
                f,
                UiRefs {
                    tab,
                    keys: &keys_tab,
                    watch: &watch_tab,
                    pkgs: &pkg_entries,
                    pkg_sel,
                    allow_pre,
                    editing,
                    message: &message,
                    demo,
                },
            )
        })?;
        let ev = if keys_tab.job.is_some()
            || watch_tab.job.is_some()
            || keys_tab.session.is_some()
            || watch_tab.session.is_some()
        {
            // 后台扫描/连接进行中,或常驻会话活着(要收断连事件):限时等键 +
            // 轮询;转圈动画靠空转重画推进。
            if event::poll(std::time::Duration::from_millis(80))? {
                event::read()?
            } else {
                if let Some(m) = keys_tab.poll_job() {
                    message = m;
                }
                if let Some(m) = watch_tab.poll_job() {
                    message = m;
                }
                if let Some(m) = keys_tab.poll_session() {
                    message = m;
                }
                if let Some(m) = watch_tab.poll_session() {
                    message = m;
                }
                if keys_tab.job.is_some()
                    || matches!(keys_tab.conn, Conn::Connecting(_))
                {
                    keys_tab.spin += 1;
                }
                if watch_tab.job.is_some()
                    || matches!(watch_tab.conn, Conn::Connecting(_))
                {
                    watch_tab.spin += 1;
                }
                continue;
            }
        } else {
            event::read()?
        };
        let Event::Key(key) = ev else {
            continue;
        };
        // 支持长按:终端按住会连发 Press(老协议)或 Repeat(kitty 协议),都收。
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }

        // ---- pop 框独占按键 ----
        let popup_open = keys_tab.popup.is_some() || watch_tab.popup.is_some();
        if popup_open {
            let t = if keys_tab.popup.is_some() {
                &mut keys_tab
            } else {
                &mut watch_tab
            };
            let Some(popup) = t.popup.as_mut() else {
                unreachable!()
            };
            match key.code {
                KeyCode::Esc => {
                    t.popup = None;
                    message = "cancelled".into();
                }
                KeyCode::Tab | KeyCode::Down => {
                    popup.cur = (popup.cur + 1) % popup.labels.len();
                }
                KeyCode::Up => {
                    popup.cur = (popup.cur + popup.labels.len() - 1) % popup.labels.len();
                }
                KeyCode::Backspace => popup.inputs[popup.cur].backspace(),
                KeyCode::Delete => popup.inputs[popup.cur].delete(),
                KeyCode::Left => popup.inputs[popup.cur].left(),
                KeyCode::Right => popup.inputs[popup.cur].right(),
                KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    popup.inputs[popup.cur].left();
                }
                KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    popup.inputs[popup.cur].right();
                }
                KeyCode::Home => popup.inputs[popup.cur].home(),
                KeyCode::End => popup.inputs[popup.cur].end(),
                KeyCode::Enter => {
                    if popup.cur + 1 < popup.labels.len() {
                        popup.cur += 1;
                    } else {
                        let state = t.popup.take().unwrap();
                        let vals: Vec<String> =
                            state.inputs.iter().map(|i| i.buf.clone()).collect();
                        let title = state.title.trim().to_string();
                        // 会话动作瞬时完成,不再挂起 TUI。
                        let res = match state.kind {
                            PopupKind::WifiAdd => t.do_wifi_add(
                                vals.first().map(String::as_str).unwrap_or(""),
                                vals.get(1).map(String::as_str).unwrap_or(""),
                            ),
                            PopupKind::WifiPass { ref ssid } => t.do_wifi_add(
                                ssid,
                                vals.first().map(String::as_str).unwrap_or(""),
                            ),
                        };
                        message = format!("{title}: {}", describe(&res, dry_run));
                        if res.is_ok() && !dry_run
                            && let Some(err) = t.load_config() {
                                message = format!("read failed: {err}");
                            }
                    }
                }
                KeyCode::Char(c) => popup.inputs[popup.cur].insert(c),
                _ => {}
            }
            continue;
        }

        // ---- 行内编辑独占按键 ----
        let editing_open = keys_tab.editing.is_some() || watch_tab.editing.is_some();
        if editing_open {
            let t = if keys_tab.editing.is_some() {
                &mut keys_tab
            } else {
                &mut watch_tab
            };
            let Some((_, input)) = t.editing.as_mut() else {
                unreachable!()
            };
            match key.code {
                KeyCode::Esc => {
                    t.editing = None;
                    message = "edit cancelled".into();
                }
                KeyCode::Enter => {
                    let (idx, val) = t.editing.take().unwrap();
                    let field = match &t.rows[idx].action {
                        RowAction::Edit(f) => *f,
                        _ => unreachable!(),
                    };
                    let label = t.rows[idx].label.clone();
                    let res = t.do_field(field, &val.buf);
                    message = format!("{label}: {}", describe(&res, dry_run));
                    if res.is_ok() && !dry_run
                        && let Some(err) = t.load_config() {
                            message = format!("read failed: {err}");
                        }
                }
                KeyCode::Backspace => input.backspace(),
                KeyCode::Delete => input.delete(),
                KeyCode::Left => input.left(),
                KeyCode::Right => input.right(),
                KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    input.left();
                }
                KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    input.right();
                }
                KeyCode::Home => input.home(),
                KeyCode::End => input.end(),
                KeyCode::Char(c) => input.insert(c),
                _ => {}
            }
            continue;
        }

        // ---- 设备连接流程(未就绪)独占按键;q / Ctrl+C 仍可退出 ----
        let is_quit = key.code == KeyCode::Char('q')
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL));
        // q / Ctrl+C 退出、Tab 切标签,连接流程弹窗期间也保持可用。
        if !is_quit
            && key.code != KeyCode::Tab
            && matches!(tab, Tab::Keys | Tab::Watch)
        {
            let t = if tab == Tab::Keys {
                &mut keys_tab
            } else {
                &mut watch_tab
            };
            if !matches!(t.conn, Conn::Ready) {
                device_flow_key(t, key.code, &mut message);
                continue;
            }
        }

        match key.code {
            KeyCode::Char('q') => break,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
            KeyCode::Tab => {
                tab = tab.next();
                message = default_message(tab);
                match tab {
                    Tab::Keys => keys_tab.enter(),
                    Tab::Watch => watch_tab.enter(),
                    Tab::Pkgs => {}
                }
            }
            KeyCode::Char('1') | KeyCode::Char('2') | KeyCode::Char('3') => {
                let i = match key.code {
                    KeyCode::Char('1') => 0,
                    KeyCode::Char('2') => 1,
                    _ => 2,
                };
                tab = Tab::all()[i];
                message = default_message(tab);
                match tab {
                    Tab::Keys => keys_tab.enter(),
                    Tab::Watch => watch_tab.enter(),
                    Tab::Pkgs => {}
                }
            }
            KeyCode::Char('j') | KeyCode::Down => match tab {
                Tab::Keys => keys_tab.sel = (keys_tab.sel + 1).min(keys_tab.rows.len() - 1),
                Tab::Watch => watch_tab.sel = (watch_tab.sel + 1).min(watch_tab.rows.len() - 1),
                Tab::Pkgs => pkg_sel = (pkg_sel + 1) % pkg_entries.len(),
            },
            KeyCode::Char('k') | KeyCode::Up => match tab {
                Tab::Keys => keys_tab.sel = keys_tab.sel.saturating_sub(1),
                Tab::Watch => watch_tab.sel = watch_tab.sel.saturating_sub(1),
                Tab::Pkgs => pkg_sel = (pkg_sel + pkg_entries.len() - 1) % pkg_entries.len(),
            },
            KeyCode::Char('p') => {
                allow_pre = !allow_pre;
                message = format!("prerelease: {}", on_off(allow_pre));
            }
            KeyCode::Char('r') => match tab {
                Tab::Keys | Tab::Watch => {
                    let t = if tab == Tab::Keys {
                        &mut keys_tab
                    } else {
                        &mut watch_tab
                    };
                    if let Some(err) = t.load_config() {
                        message = format!("read failed: {err}");
                    }
                }
                Tab::Pkgs => {
                    message = "refreshing…".into();
                    let editing = None;
                    terminal.draw(|f| {
                        ui(
                            f,
                            UiRefs {
                                tab,
                                keys: &keys_tab,
                                watch: &watch_tab,
                                pkgs: &pkg_entries,
                                pkg_sel,
                                allow_pre,
                                editing,
                                message: &message,
                                demo,
                            },
                        )
                    })?;
                    fetch(&mut pkg_entries, &dir);
                    message = "refreshed".into();
                }
            },
            KeyCode::Char('d') if tab != Tab::Pkgs => {
                let t = if tab == Tab::Keys {
                    &mut keys_tab
                } else {
                    &mut watch_tab
                };
                if let RowAction::WifiEntry { ssid } = &t.rows[t.sel].action {
                    let ssid = ssid.clone();
                    let res = t.do_wifi_remove(&ssid);
                    message = format!("wifi {ssid}: {}", describe(&res, dry_run));
                    if res.is_ok() && !dry_run
                        && let Some(err) = t.load_config() {
                            message = format!("read failed: {err}");
                        }
                }
            }
            KeyCode::Enter => match tab {
                Tab::Keys | Tab::Watch => {
                    let t = if tab == Tab::Keys {
                        &mut keys_tab
                    } else {
                        &mut watch_tab
                    };
                    // 克隆动作,免得借用 rows 卡住后面的 &mut 方法。
                    let action = t.rows[t.sel].action.clone();
                    match action {
                        RowAction::AsrClear => {
                            let label = t.rows[t.sel].label.clone();
                            let res = t.do_asr_clear();
                            message = format!("{label}: {}", describe(&res, dry_run));
                            if res.is_ok() && !dry_run
                                && let Some(err) = t.load_config() {
                                    message = format!("read failed: {err}");
                                }
                        }
                        RowAction::ResetDevice => {
                            let label = t.rows[t.sel].label.clone();
                            let res = t.do_reset();
                            message = format!("{label}: {}", describe(&res, dry_run));
                            if res.is_ok() && !dry_run
                                && let Some(err) = t.load_config() {
                                    message = format!("read failed: {err}");
                                }
                        }
                        RowAction::Edit(_) => {
                            // 预填当前值:方便只改一部分。
                            t.editing = Some((t.sel, Input::with_str(&t.rows[t.sel].value)));
                            message = "editing — Enter save · Esc cancel".into();
                        }
                        RowAction::Cycle { field, values } => {
                            // 枚举选项:回车直接切到下一个值。
                            let current = t.rows[t.sel].value.trim().to_lowercase();
                            let next = next_in_cycle(&values, &current);
                            let label = t.rows[t.sel].label.clone();
                            let res = t.do_field(field, next);
                            message = format!("{label}: {}", describe(&res, dry_run));
                            if res.is_ok() && !dry_run
                                && let Some(err) = t.load_config() {
                                    message = format!("read failed: {err}");
                                }
                        }
                        RowAction::Popup(kind) => {
                            t.popup = Some(PopupState::new(kind, Vec::new()));
                            message = "Tab/↑↓ field · Enter save · Esc cancel".into();
                        }
                        RowAction::WifiEntry { ssid } => {
                            // 从快照里拿当前密码,弹框改;提交 = 同 ssid 覆盖。
                            let pass = t
                                .config
                                .as_ref()
                                .and_then(|c| c.get("wifi_list").and_then(|v| v.as_array()))
                                .and_then(|arr| {
                                    arr.iter()
                                        .find(|e| e.get("ssid").and_then(|v| v.as_str()) == Some(ssid.as_str()))
                                })
                                .and_then(|e| e.get("pass"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            t.popup = Some(PopupState::new(
                                PopupKind::WifiPass { ssid },
                                vec![pass],
                            ));
                            message = "Enter save · Esc cancel".into();
                        }
                    }
                }
                Tab::Pkgs => {
                    let e = &pkg_entries[pkg_sel];
                    let target = e.target(allow_pre);
                    let pkg = e.pkg;
                    let res = suspend(&mut terminal, || {
                        install::install(pkg, target.clone(), allow_pre, &dir, dry_run)
                    });
                    message = format!("{}: {}", pkg.name, describe(&res, dry_run));
                    fetch(&mut pkg_entries, &dir);
                }
            },
            KeyCode::Char('u') => {
                if tab == Tab::Pkgs {
                    let e = &pkg_entries[pkg_sel];
                    let target = e.target(allow_pre);
                    let pkg = e.pkg;
                    let res = suspend(&mut terminal, || {
                        install::install(pkg, target.clone(), allow_pre, &dir, dry_run)
                    });
                    message = format!("{}: {}", pkg.name, describe(&res, dry_run));
                    fetch(&mut pkg_entries, &dir);
                }
            }
            KeyCode::Char('U') if tab == Tab::Pkgs => {
                let all: Vec<(&Pkg, Option<String>)> = pkg_entries
                    .iter()
                    .map(|e| (e.pkg, e.target(allow_pre)))
                    .collect();
                let res = suspend(&mut terminal, || {
                    all.iter()
                        .map(|&(p, ref t)| install::install(p, t.clone(), allow_pre, &dir, dry_run))
                        .collect::<Result<Vec<_>>>()
                        .map(|_| ())
                });
                message = format!("update all: {}", describe(&res, dry_run));
                fetch(&mut pkg_entries, &dir);
            }
            _ => {}
        }
    }
    ratatui::restore();
    Ok(())
}

fn describe<T>(res: &Result<T>, dry_run: bool) -> String {
    match res {
        Ok(_) if dry_run => "dry-run ok".into(),
        Ok(_) => "done".into(),
        Err(e) => format!("{e:#}"),
    }
}

fn default_message(tab: Tab) -> String {
    match tab {
        Tab::Keys => "vibekeys — Enter run/edit · r re-read config".into(),
        Tab::Watch => "watch — Enter run/edit · r re-read config".into(),
        Tab::Pkgs => "packages — u update · U all · p prerelease".into(),
    }
}

/// 退出 TUI 模式执行阻塞动作(install / BLE 要往正常屏幕打 stdout),完事重建 TUI。
fn suspend<T>(terminal: &mut DefaultTerminal, f: impl FnOnce() -> Result<T>) -> Result<T> {
    ratatui::restore();
    // 动作执行期间(下载几秒、BLE 扫描 2-5 秒)终端在正常屏,底下是 shell。
    // 保持 raw mode:按键不回显、不进 shell 的行缓冲,只静静排队,回来后排掉。
    // 否则挂起窗口里敲的 `q` 之类会漏给 shell 真的执行。
    let _raw = crossterm::terminal::enable_raw_mode();
    let res = f();
    // 排空挂起期间积压的按键,免得恢复后被当成 TUI 快捷键(比如 `q`)。
    while event::poll(std::time::Duration::ZERO).unwrap_or(false) {
        let _ = event::read();
    }
    drop(_raw);
    *terminal = ratatui::init();
    res
}

/// 设备页连接流程按键:入口菜单 / 网页配置 / 扫描中 / 选设备 / 连接中 / 失败 /
/// 断连弹窗时的键位(Ready 走主循环)。
fn device_flow_key(t: &mut DeviceTab, code: KeyCode, message: &mut String) {
    // 先折成 Copy 的小枚举,免得借用 &t.conn 卡住后面的 &mut 方法。
    #[derive(Clone, Copy)]
    enum Flow {
        Fresh,
        Menu,
        WebSetup,
        Scanning,
        Pick,
        Connecting,
        Failed,
        Disconnected,
    }
    let flow = match t.conn {
        Conn::Fresh => Flow::Fresh,
        Conn::Menu => Flow::Menu,
        Conn::WebSetup { .. } => Flow::WebSetup,
        Conn::Scanning => Flow::Scanning,
        Conn::Pick => Flow::Pick,
        Conn::Connecting(_) => Flow::Connecting,
        Conn::Ready => return,
        Conn::Failed(_) => Flow::Failed,
        Conn::Disconnected => Flow::Disconnected,
    };
    match (flow, code) {
        (Flow::Fresh, KeyCode::Enter | KeyCode::Char('r')) => t.start_scan(),
        (Flow::Menu, KeyCode::Char('j') | KeyCode::Down) => {
            t.menu_sel = (t.menu_sel + 1).min(1);
        }
        (Flow::Menu, KeyCode::Char('k') | KeyCode::Up) => {
            t.menu_sel = t.menu_sel.saturating_sub(1);
        }
        (Flow::Menu, KeyCode::Enter) => match t.menu_sel {
            0 => t.start_scan(),
            _ => t.show_web_setup(),
        },
        // 数字直选(此时全局 1/2/3 切标签让位给菜单)。
        (Flow::Menu, KeyCode::Char('1')) => t.start_scan(),
        (Flow::Menu, KeyCode::Char('2')) => t.show_web_setup(),
        (Flow::WebSetup, KeyCode::Esc | KeyCode::Enter) => {
            // Esc/Enter 回菜单(Esc 惯例;Enter 兼容"看完按回车")。
            t.conn = Conn::Menu;
        }
        (Flow::Scanning, KeyCode::Char('r')) => t.start_scan(),
        (Flow::Scanning, KeyCode::Esc) => {
            t.job = None;
            t.conn = Conn::Menu;
            *message = "scan cancelled".into();
        }
        (Flow::Pick, KeyCode::Char('j') | KeyCode::Down) => {
            t.pick_sel = (t.pick_sel + 1).min(t.pick.len().saturating_sub(1));
        }
        (Flow::Pick, KeyCode::Char('k') | KeyCode::Up) => {
            t.pick_sel = t.pick_sel.saturating_sub(1);
        }
        (Flow::Pick, KeyCode::Enter) => {
            if let Some(dev) = t.pick.get(t.pick_sel).cloned() {
                t.start_connect(dev);
            }
        }
        (Flow::Pick, KeyCode::Char('r') | KeyCode::Esc) => t.start_scan(),
        (Flow::Connecting, KeyCode::Esc) => {
            // 弃掉连接任务/会话,回列表;设备也许还在广播,重连再试即可。
            t.job = None;
            t.session = None;
            t.conn = Conn::Pick;
            *message = "connect cancelled".into();
        }
        (Flow::Failed, KeyCode::Enter | KeyCode::Char('r')) => t.start_scan(),
        // 断连弹窗:按回车确认,回扫描页(用户要求只认回车)。
        (Flow::Disconnected, KeyCode::Enter) => t.start_scan(),
        _ => {}
    }
}

fn on_off(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

/// 窄屏下的紧凑文案。
fn short_status(s: &str) -> &str {
    match s {
        "update available" => "update",
        "not installed" => "-",
        other => other,
    }
}

// ===== 绘制 =====

struct UiRefs<'a> {
    tab: Tab,
    keys: &'a DeviceTab,
    watch: &'a DeviceTab,
    pkgs: &'a [Entry],
    pkg_sel: usize,
    allow_pre: bool,
    editing: Option<&'a (usize, Input)>,
    message: &'a str,
    demo: bool,
}

fn ui(f: &mut Frame, r: UiRefs) {
    let UiRefs {
        tab,
        keys: keys_tab,
        watch: watch_tab,
        pkgs: pkg_entries,
        pkg_sel,
        allow_pre,
        editing,
        message,
        demo,
    } = r;

    let [tabs, area, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .areas(f.area());

    // 标签栏。
    let spans: Vec<Span> = Tab::all()
        .iter()
        .map(|t| {
            let text = format!(
                " {} {} ",
                Tab::all().iter().position(|x| x == t).unwrap() + 1,
                t.title()
            );
            let style = if *t == tab {
                Style::default()
                    .bg(Color::LightBlue)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Span::styled(text, style)
        })
        .collect();
    f.render_widget(Line::from(spans), tabs);

    match tab {
        Tab::Keys | Tab::Watch => {
            let t = if tab == Tab::Keys {
                keys_tab
            } else {
                watch_tab
            };
            render_device_tab(f, area, t, editing);
        }
        Tab::Pkgs => render_packages_tab(f, area, pkg_entries, pkg_sel, allow_pre),
    }

    let title = format!(
        " vibeup {} · prerelease: {}{} ",
        env!("CARGO_PKG_VERSION"),
        on_off(allow_pre),
        if demo { " · DEMO" } else { "" }
    );
    let lines = vec![
        Line::from(title),
        Line::from(format!("{message}  ·  q quit · Tab tabs · r refresh")),
    ];
    f.render_widget(Paragraph::new(lines), footer);
}

/// 连续行按 section 分组。
struct Section {
    name: &'static str,
    start: usize,
    len: usize,
}

fn sections_of(rows: &[DevRow]) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        match out.last_mut() {
            Some(s) if s.name == row.section => s.len += 1,
            _ => out.push(Section {
                name: row.section,
                start: i,
                len: 1,
            }),
        }
    }
    out
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn render_device_tab(f: &mut Frame, area: Rect, t: &DeviceTab, editing: Option<&(usize, Input)>) {
    // 连接流程没走完(扫描/选设备/连接/失败):整页显示流程屏。
    if t.conn != Conn::Ready {
        render_conn_flow(f, area, t);
        return;
    }
    // 顶部一块:当前连着的设备(名字来自扫描列表的广播名)。
    let area = if let Some(c) = &t.connected {
        let [head, rest] = Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(area);
        let line = Line::from(vec![
            Span::styled(" ● ", Style::default().fg(Color::Green)),
            Span::styled(c.name.clone(), Style::default().add_modifier(Modifier::BOLD)),
        ]);
        f.render_widget(line, head);
        rest
    } else {
        area
    };
    let sections = sections_of(&t.rows);
    let areas = Layout::vertical(
        sections
            .iter()
            .map(|s| Constraint::Length(s.len as u16 + 1)),
    )
    .split(area);

    for (sec, sec_area) in sections.iter().zip(areas.iter()) {
        // 每节:只有一条上边框,节名嵌在框线上(像 HTML 的 <fieldset><legend>)。
        let block = Block::default()
            .borders(Borders::TOP)
            .title(format!(" {} ", sec.name))
            .title_style(Style::default().add_modifier(Modifier::BOLD));
        let inner = block.inner(*sec_area);
        f.render_widget(block, *sec_area);

        let mut lines: Vec<Line> = Vec::with_capacity(sec.len);
        for i in sec.start..sec.start + sec.len {
            let row = &t.rows[i];
            let selected = i == t.sel;
            let editing_this = editing.is_some_and(|(idx, _)| *idx == i);
            let mut spans = vec![
                Span::raw("  "),
                Span::styled(
                    row.label.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ];

            if editing_this {
                // 原地编辑框:值的位置变成输入框 + 块状光标。
                let edit_style = Style::default()
                    .bg(Color::LightBlue)
                    .add_modifier(Modifier::BOLD);
                spans.push(Span::raw("  "));
                if let Some((_, input)) = editing {
                    spans.extend(input.spans(true, edit_style));
                }
                let hint = match &row.action {
                    RowAction::Edit(field) => field_hint(*field),
                    _ => "",
                };
                if !hint.is_empty() {
                    spans.push(Span::styled(
                        format!("  ({hint})"),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
            } else if !row.value.is_empty() {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    row.value.clone(),
                    Style::default().fg(Color::Gray),
                ));
            }

            let line = Line::from(spans);
            lines.push(if selected {
                line.style(Style::default().bg(Color::DarkGray))
            } else {
                line
            });
        }
        f.render_widget(Paragraph::new(lines), inner);
    }

    // pop 框:居中浮层,画在内容之上。
    if let Some(popup) = &t.popup {
        let w = 56.min(area.width.saturating_sub(4)).max(24);
        let h = (popup.labels.len() as u16 + 4).min(area.height);
        let rect = Rect {
            x: area.x + (area.width.saturating_sub(w)) / 2,
            y: area.y + (area.height.saturating_sub(h)) / 2,
            width: w,
            height: h,
        };
        f.render_widget(Clear, rect);
        let block = Block::bordered().title(popup.title.clone());
        let inner = block.inner(rect);
        f.render_widget(block, rect);

        let mut lines: Vec<Line> = Vec::new();
        for (i, label) in popup.labels.iter().enumerate() {
            let active = i == popup.cur;
            let style = if active {
                Style::default()
                    .bg(Color::LightBlue)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let mut line_spans = vec![
                Span::raw(" "),
                Span::styled(
                    format!("{label:>6}: "),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(popup.inputs[i].buf.clone(), style),
            ];
            line_spans.extend(popup.cursor_spans(i, style));
            lines.push(Line::from(line_spans));
        }
        lines.push(Line::from(Span::styled(
            " Tab/↑↓ field · Enter save · Esc cancel ",
            Style::default().fg(Color::DarkGray),
        )));
        f.render_widget(Paragraph::new(lines), inner);
    }
}

/// 连接流程屏(内容居中):扫描转圈 / 候选设备列表 / 连接转圈 / 失败提示 /
/// 断连弹窗。
fn render_conn_flow(f: &mut Frame, area: Rect, t: &DeviceTab) {
    let frame = SPINNER[t.spin % SPINNER.len()];
    let dim = Style::default().fg(Color::DarkGray);
    let err = Style::default().fg(Color::Red);
    // 各状态的内容;(弹窗标题, 行, 红 = 告警态)。
    let (title, lines, alert): (&str, Vec<Line>, bool) = match &t.conn {
        Conn::Ready => return,
        Conn::Fresh => (
            " scan ",
            vec![
                Line::from(" Not scanned yet"),
                Line::from(Span::styled(" Enter/r to scan", dim)),
            ],
            false,
        ),
        Conn::Menu => {
            let mut lines = Vec::new();
            for (i, (num, label)) in [("1", "Scan devices"), ("2", "Configure on the web")]
                .into_iter()
                .enumerate()
            {
                let selected = i == t.menu_sel;
                let marker = if selected { "❯" } else { " " };
                let line = Line::from(format!(" {marker} {num}. {label}"));
                lines.push(if selected {
                    line.style(
                        Style::default()
                            .bg(Color::DarkGray)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    line
                });
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                " ↑↓/1/2 select · Enter confirm · Tab switch",
                dim,
            )));
            (" setup ", lines, false)
        }
        Conn::WebSetup { url } => (
            " web setup ",
            vec![
                Line::from(" Configure via web Bluetooth — open in your browser:"),
                Line::from(Span::styled(
                    format!(" {url}"),
                    Style::default().fg(Color::LightBlue),
                )),
                Line::from(""),
                Line::from(Span::styled(" Esc back", dim)),
            ],
            false,
        ),
        Conn::Scanning => (
            " scanning ",
            vec![
                Line::from(format!(" {frame} Scanning for devices…")),
                Line::from(Span::styled(" Esc cancel", dim)),
            ],
            false,
        ),
        Conn::Pick => {
            let mut lines = vec![Line::from(format!(
                " Found {} device(s) — pick one to configure:",
                t.pick.len()
            ))];
            for (i, d) in t.pick.iter().enumerate() {
                let selected = i == t.pick_sel;
                let marker = if selected { "❯ " } else { "  " };
                let line = Line::from(format!(" {marker}{}", d.name));
                lines.push(if selected {
                    line.style(
                        Style::default()
                            .bg(Color::DarkGray)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    line
                });
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                " ↑↓ select · Enter connect · r rescan",
                dim,
            )));
            (" devices ", lines, false)
        }
        Conn::Connecting(name) => (
            " connecting ",
            vec![
                Line::from(format!(" {frame} Connecting to {name}…")),
                Line::from(Span::styled(" Esc back to list", dim)),
            ],
            false,
        ),
        Conn::Failed(e) => (
            " error ",
            vec![
                Line::from(Span::styled(format!(" ✗ {e}"), err)),
                Line::from(Span::styled(" Enter/r retry", dim)),
            ],
            true,
        ),
        Conn::Disconnected => (
            " disconnected ",
            vec![
                Line::from(" Device disconnected"),
                Line::from(Span::styled(" Enter to rescan", dim)),
            ],
            true,
        ),
    };
    let mut lines = lines;
    // demo 提示只对设备相关的状态有意义(菜单/网页配置页没有"设备"概念)。
    if t.demo
        && matches!(
            t.conn,
            Conn::Scanning | Conn::Pick | Conn::Connecting(_) | Conn::Ready
        )
    {
        lines.push(Line::from(Span::styled(" (demo fake devices)", dim)));
    }
    // 居中弹窗,四周边框;告警态(失败/断连)用红框。
    let h = (lines.len() as u16 + 2).min(area.height); // +2 = 上下边框
    let content_w = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let w = (content_w + 2 + 2) // +2 = 内容左右各留一空格,+2 = 边框
        .clamp(22, area.width);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, rect);
    let border_style = if alert { err } else { Style::default() };
    let block = Block::bordered()
        .title(Span::styled(
            title,
            border_style.add_modifier(Modifier::BOLD),
        ))
        .border_style(border_style);
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_packages_tab(f: &mut Frame, area: Rect, entries: &[Entry], sel: usize, allow_pre: bool) {
    let wide = area.width >= 78;
    let header = if wide {
        ["package", "installed", "latest", "latest-pre", "status"]
    } else {
        ["package", "inst", "latest", "pre", "status"]
    };
    let header = TRow::new(header)
        .style(Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED));

    let rows: Vec<TRow> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let style = if i == sel {
                Style::default().bg(Color::DarkGray)
            } else {
                Style::default()
            };
            let installed = e
                .installed
                .clone()
                .unwrap_or_else(|| "not installed".into());
            let status = e.status(allow_pre);
            let status: String = if wide {
                status.into()
            } else {
                short_status(status).into()
            };
            let cells = [
                Cell::from(e.pkg.name),
                Cell::from(if wide {
                    installed
                } else {
                    short_status(&installed).into()
                }),
                Cell::from(e.latest.clone().unwrap_or_else(|| "?".into())),
                Cell::from(
                    e.any
                        .clone()
                        .filter(|a| e.latest.as_ref().is_some_and(|l| a != l))
                        .unwrap_or_else(|| "-".into()),
                ),
                Cell::from(status),
            ];
            TRow::new(cells).style(style)
        })
        .collect();

    let widths = if wide {
        [
            Constraint::Length(12),
            Constraint::Length(16),
            Constraint::Length(13),
            Constraint::Length(13),
            Constraint::Min(18),
        ]
    } else {
        [
            Constraint::Length(12),
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Length(13),
            Constraint::Min(10),
        ]
    };
    let table = Table::new(rows, widths).header(header);
    f.render_widget(table, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ble::DeviceKind;

    fn mock() -> serde_json::Value {
        sample_config(DeviceKind::VibeKeys)
    }

    fn tab() -> DeviceTab {
        let mut t = DeviceTab::new(DeviceKind::VibeKeys, true);
        t.conn = Conn::Ready;
        t.connected = Some(Connected {
            name: "VibeKeys-1A2B".into(),
        });
        t.config = Some(t.mock.clone());
        t.rebuild_rows();
        t
    }

    fn devs() -> Vec<crate::ble::FoundDevice> {
        vec![
            crate::ble::FoundDevice {
                name: "VibeKeys-1A2B".into(),
                id: "demo-1A2B".into(),
            },
            crate::ble::FoundDevice {
                name: "VibeKeys-3C4D".into(),
                id: "demo-3C4D".into(),
            },
        ]
    }

    #[test]
    fn sample_config_has_device_specific_fields() {
        let keys = sample_config(DeviceKind::VibeKeys);
        assert!(keys.get("mic_model").is_some());
        assert!(keys.get("prefer_builtin_asr").is_some());
        let watch = sample_config(DeviceKind::Watch);
        assert!(watch.get("mic_model").is_none());
    }

    #[test]
    fn mock_wifi_add_upserts() {
        let mut m = mock();
        mock_wifi_add(&mut m, "Cafe", "pw");
        assert_eq!(m["wifi_list"].as_array().unwrap().len(), 3);
        mock_wifi_add(&mut m, "Cafe", "pw2");
        let list = m["wifi_list"].as_array().unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[2]["pass"], "pw2");
    }

    #[test]
    fn mock_wifi_remove_filters() {
        let mut m = mock();
        mock_wifi_remove(&mut m, "Home");
        let list = m["wifi_list"].as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["ssid"], "Office");
    }

    #[test]
    fn mock_asr_set_keeps_unset_fields() {
        let mut m = mock();
        mock_asr_set(&mut m, Some("https://x"), None, Some("m2"));
        let asr = &m["asr_config"];
        assert_eq!(asr["uri"], "https://x");
        assert_eq!(asr["api_key"], "sk-demo");
        assert_eq!(asr["model"], "m2");
    }

    #[test]
    fn mock_keymap_uses_physical_key_and_binding_parser() {
        let mut m = mock();
        mock_keymap(&mut m, "yolo", "\"/review\\n\"");
        let km = &m["keymap"];
        assert_eq!(km["SWITCH"]["type"], "text");
        assert_eq!(km["SWITCH"]["value"], "/review\n");
    }

    #[test]
    fn sections_group_consecutive_rows() {
        let t = tab();
        let secs = sections_of(&t.rows);
        let names: Vec<&str> = secs.iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["server", "wifi", "asr", "keyboard", "device"]);
        assert_eq!(secs.iter().map(|s| s.len).sum::<usize>(), t.rows.len());
    }

    #[test]
    fn wifi_rows_are_dynamic_with_add_row_last() {
        let t = tab();
        let wifi: Vec<&DevRow> = t.rows.iter().filter(|r| r.section == "wifi").collect();
        // 2 条现有网络 + add +
        assert_eq!(wifi.len(), 3);
        assert_eq!(wifi[0].label, "Home");
        assert_eq!(wifi[1].label, "Office");
        assert_eq!(wifi[2].label, "add +");
        // 现有网络 = WifiEntry(回车改密码,d 删除);add + = Popup。
        assert!(matches!(&wifi[0].action, RowAction::WifiEntry { ssid } if ssid == "Home"));
        assert!(matches!(wifi[2].action, RowAction::Popup(_)));
    }

    #[test]
    fn wifi_pass_edit_popup_upserts_same_ssid() {
        let mut t = tab();
        // 第一条 wifi 行(Home)的改密码 pop 框提交 → 同 ssid 更新,不新增。
        let rows_after = t.rows.len();
        t.do_wifi_add("Home", "new-pass").unwrap();
        let list = t.mock["wifi_list"].as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["pass"], "new-pass");
        assert_eq!(t.rows.len(), rows_after);
    }

    #[test]
    fn cycle_picks_next_and_wraps() {
        let vals = ["ptt", "toggle"];
        assert_eq!(next_in_cycle(&vals, "toggle"), "ptt");
        assert_eq!(next_in_cycle(&vals, "ptt"), "toggle");
        // 未知/空值 → 第一个。
        assert_eq!(next_in_cycle(&vals, ""), "ptt");
        assert_eq!(next_in_cycle(&vals, "bogus"), "ptt");
    }

    #[test]
    fn do_field_demo_updates_mock() {
        let mut t = DeviceTab::new(DeviceKind::VibeKeys, true);
        t.do_field(Field::ServerUrl, "mqtt://x").unwrap();
        assert_eq!(t.mock["server_url"], "mqtt://x");
        t.do_field(Field::MicModel, "ptt").unwrap();
        assert_eq!(t.mock["mic_model"], 0);
        t.do_field(Field::Keymap, "CUSTOM=\"/compact\\n\"")
            .unwrap();
        assert_eq!(t.mock["keymap"]["CUSTOM"]["type"], "text");
        t.do_field(Field::MicModel, "bogus").unwrap_err();
    }

    #[test]
    fn scan_result_fills_pick_list_and_connect_reaches_ready() {
        let mut t = DeviceTab::new(DeviceKind::VibeKeys, true);
        t.on_scan_result(Ok(devs()));
        assert_eq!(t.conn, Conn::Pick);
        assert_eq!(t.pick.len(), 2);
        assert_eq!(t.pick_sel, 0);
        t.start_connect(t.pick[0].clone());
        assert_eq!(t.conn, Conn::Connecting("VibeKeys-1A2B".into()));
        assert!(t.pending.is_some());
        t.on_connect_result(Ok(sample_config(DeviceKind::VibeKeys)));
        assert_eq!(t.conn, Conn::Ready);
        // 转正后带广播名,顶部设备块用它显示。
        assert_eq!(t.connected.as_ref().unwrap().name, "VibeKeys-1A2B");
        assert!(!t.rows.is_empty());
    }

    #[test]
    fn scan_and_connect_failures_land_in_failed_state() {
        let mut t = DeviceTab::new(DeviceKind::VibeKeys, true);
        t.on_scan_result(Err(anyhow::anyhow!("no vibekeys in provisioning mode")));
        assert!(matches!(t.conn, Conn::Failed(_)));
        t.on_connect_result(Err(anyhow::anyhow!("connection refused")));
        assert!(matches!(t.conn, Conn::Failed(_)));
    }

    #[test]
    fn disconnect_event_pops_dialog_then_rescan_on_enter() {
        let mut t = tab();
        assert_eq!(t.conn, Conn::Ready);
        // 看门狗/前置检查报的断连事件 → 弹窗状态。
        let msg = t.on_session_event(crate::ble::SessionEvent::Disconnected(
            anyhow::anyhow!("device disconnected"),
        ));
        assert_eq!(t.conn, Conn::Disconnected);
        assert!(t.session.is_none());
        assert!(msg.starts_with("device disconnected"));
        let mut msg = String::new();
        device_flow_key(&mut t, KeyCode::Enter, &mut msg);
        assert_eq!(t.conn, Conn::Scanning);
    }

    #[test]
    fn disconnect_event_while_connecting_lands_in_failed_state() {
        let mut t = DeviceTab::new(DeviceKind::VibeKeys, true);
        let dev = crate::ble::FoundDevice {
            name: "VibeKeys-1A2B".into(),
            id: "demo-1A2B".into(),
        };
        t.start_connect(dev);
        assert!(matches!(t.conn, Conn::Connecting(_)));
        t.on_session_event(crate::ble::SessionEvent::Disconnected(
            anyhow::anyhow!("no longer advertising"),
        ));
        assert!(matches!(t.conn, Conn::Failed(_)));
    }

    #[test]
    fn ready_event_from_session_reaches_connected() {
        let mut t = DeviceTab::new(DeviceKind::VibeKeys, true);
        t.start_connect(crate::ble::FoundDevice {
            name: "VibeKeys-1A2B".into(),
            id: "demo-1A2B".into(),
        });
        let msg = t.on_session_event(crate::ble::SessionEvent::Ready(Ok(
            sample_config(DeviceKind::VibeKeys),
        )));
        assert_eq!(t.conn, Conn::Ready);
        assert_eq!(msg, "connected");
        assert_eq!(t.connected.as_ref().unwrap().name, "VibeKeys-1A2B");
    }

    #[test]
    fn flow_keys_navigate_pick_and_rescan() {
        let mut t = DeviceTab::new(DeviceKind::VibeKeys, true);
        t.on_scan_result(Ok(devs()));
        let mut msg = String::new();
        device_flow_key(&mut t, KeyCode::Down, &mut msg);
        assert_eq!(t.pick_sel, 1);
        device_flow_key(&mut t, KeyCode::Down, &mut msg);
        assert_eq!(t.pick_sel, 1); // 不越界
        device_flow_key(&mut t, KeyCode::Char('k'), &mut msg);
        assert_eq!(t.pick_sel, 0);
        device_flow_key(&mut t, KeyCode::Char('r'), &mut msg); // 重扫(demo 线程结果没人收,无碍)
        assert_eq!(t.conn, Conn::Scanning);
    }

    #[test]
    fn enter_shows_menu_and_menu_keys_dispatch() {
        let mut t = DeviceTab::new(DeviceKind::VibeKeys, true);
        t.enter();
        assert_eq!(t.conn, Conn::Menu);
        let mut msg = String::new();
        // 菜单导航。
        device_flow_key(&mut t, KeyCode::Down, &mut msg);
        assert_eq!(t.menu_sel, 1);
        device_flow_key(&mut t, KeyCode::Up, &mut msg);
        assert_eq!(t.menu_sel, 0);
        // 数字直选:1 = 扫描。
        device_flow_key(&mut t, KeyCode::Char('1'), &mut msg);
        assert_eq!(t.conn, Conn::Scanning);
        // Esc 取消回菜单,2 = 网页配置。
        device_flow_key(&mut t, KeyCode::Esc, &mut msg);
        assert_eq!(t.conn, Conn::Menu);
        t.menu_sel = 1;
        device_flow_key(&mut t, KeyCode::Char('2'), &mut msg);
        assert!(matches!(t.conn, Conn::WebSetup { .. }));
        // Esc 回菜单。
        device_flow_key(&mut t, KeyCode::Esc, &mut msg);
        assert_eq!(t.conn, Conn::Menu);
    }

    #[test]
    fn demo_devices_look_like_broadcast_names() {
        let keys = demo_devices(DeviceKind::VibeKeys);
        assert_eq!(keys.len(), 2);
        assert!(keys.iter().all(|d| d.name.starts_with("VibeKeys-")));
        let watch = demo_devices(DeviceKind::Watch);
        assert!(watch.iter().all(|d| d.name.starts_with("Watch-")));
    }
}

#[cfg(test)]
mod input_tests {
    use super::{Input, Style};

    #[test]
    fn insert_at_cursor_and_move() {
        let mut i = Input::default();
        for c in "abc".chars() {
            i.insert(c);
        }
        assert_eq!(i.buf, "abc");
        i.left();
        i.insert('X');
        assert_eq!(i.buf, "abXc");
        assert_eq!(i.pos, 3);
    }

    #[test]
    fn left_at_start_is_noop_and_backspace_removes_before_cursor() {
        let mut i = Input::with_str("ab");
        i.left();
        i.left();
        i.left();
        assert_eq!(i.pos, 0);
        i.insert('X');
        assert_eq!(i.buf, "Xab");
        i.backspace();
        assert_eq!(i.buf, "ab");
        assert_eq!(i.pos, 0);
    }

    #[test]
    fn delete_removes_at_cursor() {
        let mut i = Input::with_str("abc");
        i.left();
        i.delete(); // 删掉 c
        assert_eq!(i.buf, "ab");
        i.left();
        i.delete(); // 删掉 b
        assert_eq!(i.buf, "a");
        assert_eq!(i.pos, 1);
        i.delete(); // 光标在末尾,no-op
        assert_eq!(i.buf, "a");
    }

    #[test]
    fn home_end_and_multibyte() {
        let mut i = Input::with_str("中文ab");
        i.home();
        i.right();
        i.insert('X');
        assert_eq!(i.buf, "中X文ab");
        i.end();
        assert_eq!(i.pos, i.chars());
        i.backspace();
        assert_eq!(i.buf, "中X文a");
        // 块状光标:反显包住光标处字符;光标在末尾时是色块占位。
        // 不在编辑态时只有整段文本。
        let spans = i.spans(true, Style::default());
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "中X文a");
        assert_eq!(spans[1].content, " ");
        let plain = i.spans(false, Style::default());
        assert_eq!(plain.len(), 1);
        assert_eq!(plain[0].content, "中X文a");
    }
}
