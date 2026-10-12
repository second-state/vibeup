//! `vibeup tui` —— 三个标签:**键盘** / **手表**(BLE 设备配置)+ **管理**
//! (vibetty / vibekeys 的安装更新)。
//!
//! Tab 或 1/2/3 切标签。设备页按功能分节(server / wifi / asr / …),每节只有一条
//! 上边框带节名;每行是「动作 + 当前值」,选中后回车:可编辑项原地变成编辑框,
//! 复杂输入(加 WiFi)弹居中的 pop 框分字段填写,立即执行的项直接跑。
//! wifi 节是动态列表:现有网络每条一行(回车改密码,`d` 删除),末尾固定一行
//! `add +` 弹框新增。BLE 和安装动作都是同步阻塞的,执行时先退出 TUI 模式让输出
//! 打到正常屏幕,结束后再回来(见 `suspend`);设备配置复用 `config_cmd`,demo
//! 模式改内存假数据。

use crate::config_cmd::{self, WifiAction};
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
use std::rc::Rc;

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

/// demo 用的假设备状态(内存配置,动作直接改它)。
type Mock = Rc<std::cell::RefCell<serde_json::Value>>;

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

fn mock_patch(mock: &Mock, field: &str, v: serde_json::Value) {
    mock.borrow_mut()[field] = v;
}

fn mock_wifi_add(mock: &Mock, ssid: &str, pass: &str) {
    let mut m = mock.borrow_mut();
    let list = m["wifi_list"].as_array_mut().unwrap();
    match list.iter_mut().find(|e| e["ssid"] == ssid) {
        Some(e) => e["pass"] = pass.into(),
        None => list.push(serde_json::json!({"ssid": ssid, "pass": pass})),
    }
}

fn mock_wifi_remove(mock: &Mock, ssid: &str) {
    mock.borrow_mut()["wifi_list"]
        .as_array_mut()
        .unwrap()
        .retain(|e| e["ssid"] != ssid);
}

fn mock_asr_set(mock: &Mock, uri: Option<&str>, api_key: Option<&str>, model: Option<&str>) {
    let mut m = mock.borrow_mut();
    let asr = m.get_mut("asr_config").unwrap();
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

fn mock_keymap(mock: &Mock, key: &str, binding: &str) {
    let mut m = mock.borrow_mut();
    let physical = config_cmd::physical_key_name(key);
    let parsed = config_cmd::parse_key_binding(binding);
    m["keymap"][&physical] = parsed;
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
        Field::Keymap => "KEY=BINDING,如 CUSTOM=\"/compact\\n\"",
        Field::Tailscale => "tskey-auth-…",
    }
}

/// 提交一个编辑值:demo 落到假配置,真机走 config_cmd(一次 BLE 会话)。
fn apply_field(
    kind: crate::ble::DeviceKind,
    demo: bool,
    mock: &Mock,
    field: Field,
    val: &str,
) -> Result<()> {
    let val = val.trim();
    match field {
        Field::ServerUrl => {
            if demo {
                mock_patch(mock, "server_url", val.into());
                println!("server_url = {val} (demo)");
                return Ok(());
            }
            config_cmd::server_url(kind, val.to_string(), false)
        }
        Field::AsrUri => {
            if demo {
                mock_asr_set(mock, Some(val), None, None);
                println!("asr uri = {val} (demo)");
                return Ok(());
            }
            config_cmd::asr(
                kind,
                config_cmd::AsrOpts {
                    uri: Some(val.to_string()),
                    ..Default::default()
                },
                false,
            )
        }
        Field::AsrApiKey => {
            if demo {
                mock_asr_set(mock, None, Some(val), None);
                println!("asr api-key updated (demo)");
                return Ok(());
            }
            config_cmd::asr(
                kind,
                config_cmd::AsrOpts {
                    api_key: Some(val.to_string()),
                    ..Default::default()
                },
                false,
            )
        }
        Field::AsrModel => {
            if demo {
                mock_asr_set(mock, None, None, Some(val));
                println!("asr model = {val} (demo)");
                return Ok(());
            }
            config_cmd::asr(
                kind,
                config_cmd::AsrOpts {
                    model: Some(val.to_string()),
                    ..Default::default()
                },
                false,
            )
        }
        Field::MicModel => {
            if demo {
                let v = match val.to_lowercase().as_str() {
                    "ptt" => 0,
                    "toggle" => 1,
                    other => bail!("invalid mic mode `{other}` — use ptt or toggle"),
                };
                mock_patch(mock, "mic_model", serde_json::json!(v));
                println!("mic_model = {v} (demo)");
                return Ok(());
            }
            config_cmd::mic_model(kind, val.to_string(), false)
        }
        Field::PreferAsr => {
            let on = match val.to_lowercase().as_str() {
                "on" | "true" => true,
                "off" | "false" => false,
                other => bail!("invalid value `{other}` — use on or off"),
            };
            if demo {
                mock_patch(mock, "prefer_builtin_asr", serde_json::json!(on));
                println!("prefer_builtin_asr = {on} (demo)");
                return Ok(());
            }
            config_cmd::prefer_asr(kind, on, false)
        }
        Field::Keymap => {
            let Some((key, binding)) = val.split_once('=') else {
                bail!("需要 `KEY=BINDING`,如 CUSTOM=\"/compact\\n\"");
            };
            if demo {
                mock_keymap(mock, key.trim(), binding.trim());
                println!("keymap {} updated (demo)", key.trim());
                return Ok(());
            }
            config_cmd::keymap(kind, key.trim().to_string(), binding.trim().to_string())
        }
        Field::Tailscale => {
            if demo {
                mock_patch(mock, "tailscale_key", val.into());
                println!("tailscale key bound (demo)");
                return Ok(());
            }
            config_cmd::tailscale(kind, val.to_string())
        }
    }
}

/// WiFi 增改(同 ssid = 更新密码):demo 落假配置,真机走一次 BLE。
fn apply_wifi_add(
    kind: crate::ble::DeviceKind,
    demo: bool,
    mock: &Mock,
    ssid: &str,
    pass: &str,
) -> Result<()> {
    if ssid.trim().is_empty() {
        bail!("ssid 不能为空");
    }
    if demo {
        mock_wifi_add(mock, ssid.trim(), pass);
        println!("wifi `{ssid}' saved (demo)");
        return Ok(());
    }
    config_cmd::wifi(
        kind,
        WifiAction::Add {
            ssid: ssid.trim().to_string(),
            pass: pass.to_string(),
        },
        false,
    )
}

/// WiFi 删除。
fn apply_wifi_remove(
    kind: crate::ble::DeviceKind,
    demo: bool,
    mock: &Mock,
    ssid: &str,
) -> Result<()> {
    if demo {
        mock_wifi_remove(mock, ssid);
        println!("wifi `{ssid}' removed (demo)");
        return Ok(());
    }
    config_cmd::wifi(
        kind,
        WifiAction::Remove {
            ssid: ssid.to_string(),
        },
        false,
    )
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

/// pop 框的字段提交动作。
type Submit = Rc<dyn Fn(&[String]) -> Result<()>>;

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
            Some(c) => spans.push(Span::styled(
                c.to_string(),
                base.add_modifier(Modifier::REVERSED)
                    .add_modifier(Modifier::BOLD),
            )),
            None => spans.push(Span::styled(
                " ",
                base.bg(Color::LightBlue).add_modifier(Modifier::BOLD),
            )),
        }
        spans.push(Span::styled(
            self.buf[b + self.buf[b..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(0)..]
                .to_string(),
            base,
        ));
        spans
    }
}

/// pop 框定义:标题 + 字段(带初始值)+ 提交动作。
struct PopupSpec {
    title: &'static str,
    fields: Vec<(&'static str, String)>,
    submit: Submit,
}

enum RowAction {
    /// 立即执行(wifi show list / asr clear / reset)。
    Run(Rc<dyn Fn() -> Result<()>>),
    /// 行内单行编辑(选中回车,该行变编辑框)。
    Edit(Field),
    /// 回车弹 pop 框分字段填写。
    Popup(PopupSpec),
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
    config: Option<serde_json::Value>,
    rows: Vec<DevRow>,
    sel: usize,
    /// demo 模式:不碰 BLE,动作改内存里的假配置。
    demo: bool,
    mock: Mock,
    /// 行内编辑:选中行下标 + 当前输入。
    editing: Option<(usize, Input)>,
    /// 打开的 pop 框。
    popup: Option<PopupState>,
}

/// 运行中的 pop 框:字段初始值展开成带光标的输入。
struct PopupState {
    title: String,
    labels: Vec<&'static str>,
    inputs: Vec<Input>,
    cur: usize,
    submit: Submit,
}

impl PopupState {
    fn new(spec: &PopupSpec) -> Self {
        Self {
            title: spec.title.to_string(),
            labels: spec.fields.iter().map(|(l, _)| *l).collect(),
            inputs: spec
                .fields
                .iter()
                .map(|(_, v)| Input::with_str(v))
                .collect(),
            cur: 0,
            submit: spec.submit.clone(),
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
        let mock = Rc::new(std::cell::RefCell::new(sample_config(kind)));
        let mut t = Self {
            kind,
            config: None,
            rows: Vec::new(),
            sel: 0,
            demo,
            mock,
            editing: None,
            popup: None,
        };
        t.rebuild_rows();
        t
    }

    /// 按当前配置快照重建行(wifi 列表是动态的,其余节的值也一并刷新)。
    fn rebuild_rows(&mut self) {
        let empty = serde_json::json!({});
        let cfg = self.config.as_ref().unwrap_or(&empty);
        let kind = self.kind;
        let demo = self.demo;
        let keys = kind == crate::ble::DeviceKind::VibeKeys;
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
        let submit: Submit = {
            let mock = self.mock.clone();
            Rc::new(move |vals| {
                apply_wifi_add(
                    kind,
                    demo,
                    &mock,
                    vals.first().map(String::as_str).unwrap_or(""),
                    vals.get(1).map(String::as_str).unwrap_or(""),
                )
            })
        };
        rows.push(DevRow {
            section: "wifi",
            label: "add +".into(),
            value: String::new(),
            action: RowAction::Popup(PopupSpec {
                title: " add wifi ",
                fields: vec![("ssid", String::new()), ("pass", String::new())],
                submit,
            }),
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
        let mock = self.mock.clone();
        rows.push(DevRow {
            section: "asr",
            label: "clear".into(),
            value: String::new(),
            action: RowAction::Run(Rc::new(move || {
                if demo {
                    mock_patch(
                        &mock,
                        "asr_config",
                        serde_json::json!({"platform": "whisper", "uri": "", "api_key": "", "model": ""}),
                    );
                    println!("asr_config cleared (demo)");
                    return Ok(());
                }
                config_cmd::asr(
                    kind,
                    config_cmd::AsrOpts {
                        uri: None,
                        api_key: None,
                        model: None,
                        clean: true,
                    },
                    false,
                )
            })),
        });

        if keys {
            rows.push(DevRow::edit(
                "keyboard",
                "mic-model",
                Field::MicModel,
                match cfg.get("mic_model").and_then(|v| v.as_u64()) {
                    Some(0) => "ptt".into(),
                    Some(1) => "toggle".into(),
                    _ => String::new(),
                },
            ));
            rows.push(DevRow::edit(
                "keyboard",
                "prefer-asr",
                Field::PreferAsr,
                match cfg.get("prefer_builtin_asr").and_then(|v| v.as_bool()) {
                    Some(true) => "on".into(),
                    Some(false) => "off".into(),
                    _ => String::new(),
                },
            ));
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
            action: RowAction::Run(Rc::new(move || {
                if demo {
                    println!("device rebooting (demo)");
                    return Ok(());
                }
                config_cmd::reset(kind)
            })),
        });

        self.sel = self.sel.min(rows.len() - 1);
        self.rows = rows;
    }

    fn load_config(&mut self, terminal: &mut DefaultTerminal) {
        if self.demo {
            self.config = Some(self.mock.borrow().clone());
            self.rebuild_rows();
            return;
        }
        let kind = self.kind;
        let res = suspend(terminal, || {
            crate::ble::with_device(kind, |p| async move { crate::ble::read_config(&p).await })
        });
        match res {
            Ok(cfg) => {
                self.config = Some(cfg);
                self.rebuild_rows();
            }
            Err(_) => self.config = None,
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
            Tab::Keys => "键盘",
            Tab::Watch => "手表",
            Tab::Pkgs => "管理",
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
        let Event::Key(key) = event::read()? else {
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
                        let res = suspend(&mut terminal, || (state.submit)(&vals));
                        message = format!("{}: {}", state.title.trim(), describe(&res, dry_run));
                        if res.is_ok() && !dry_run {
                            t.load_config(&mut terminal);
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
                    let (mock, kind) = (t.mock.clone(), t.kind);
                    let res = suspend(&mut terminal, || {
                        apply_field(kind, demo, &mock, field, &val.buf)
                    });
                    message = format!("{label}: {}", describe(&res, dry_run));
                    if res.is_ok() && !dry_run {
                        t.load_config(&mut terminal);
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

        match key.code {
            KeyCode::Char('q') => break,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
            KeyCode::Tab => {
                tab = tab.next();
                message = default_message(tab);
                // demo 不碰蓝牙,进设备页直接把假配置摆出来。
                if demo {
                    match tab {
                        Tab::Keys if keys_tab.config.is_none() => {
                            keys_tab.load_config(&mut terminal)
                        }
                        Tab::Watch if watch_tab.config.is_none() => {
                            watch_tab.load_config(&mut terminal)
                        }
                        _ => {}
                    }
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
                if demo {
                    match tab {
                        Tab::Keys if keys_tab.config.is_none() => {
                            keys_tab.load_config(&mut terminal)
                        }
                        Tab::Watch if watch_tab.config.is_none() => {
                            watch_tab.load_config(&mut terminal)
                        }
                        _ => {}
                    }
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
                Tab::Keys => keys_tab.load_config(&mut terminal),
                Tab::Watch => watch_tab.load_config(&mut terminal),
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
                    let (mock, kind, ssid) = (t.mock.clone(), t.kind, ssid.clone());
                    let res = suspend(&mut terminal, || {
                        apply_wifi_remove(kind, demo, &mock, &ssid)
                    });
                    message = format!("wifi {ssid}: {}", describe(&res, dry_run));
                    if res.is_ok() && !dry_run {
                        t.load_config(&mut terminal);
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
                    if t.config.is_none() {
                        t.load_config(&mut terminal);
                    }
                    match &t.rows[t.sel].action {
                        RowAction::Run(run) => {
                            let run = run.clone();
                            let label = t.rows[t.sel].label.clone();
                            let res = suspend(&mut terminal, || run());
                            message = format!("{label}: {}", describe(&res, dry_run));
                            if res.is_ok() && !dry_run {
                                t.load_config(&mut terminal);
                            }
                        }
                        RowAction::Edit(_) => {
                            // 预填当前值:方便只改一部分。
                            t.editing = Some((t.sel, Input::with_str(&t.rows[t.sel].value)));
                            message = "editing — Enter save · Esc cancel".into();
                        }
                        RowAction::Popup(spec) => {
                            t.popup = Some(PopupState::new(spec));
                            message = "Tab/↑↓ field · Enter save · Esc cancel".into();
                        }
                        RowAction::WifiEntry { ssid } => {
                            // 从快照里拿当前密码,弹框改;提交 = 同 ssid 覆盖。
                            let pass = t
                                .config
                                .as_ref()
                                .and_then(|c| c.get("wifi_list").and_then(|v| v.as_array()))
                                .and_then(|arr| {
                                    arr.iter().find(|e| {
                                        e.get("ssid").and_then(|v| v.as_str()) == Some(ssid)
                                    })
                                })
                                .and_then(|e| e.get("pass"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let (mock, kind, ssid) = (t.mock.clone(), t.kind, ssid.clone());
                            let submit: Submit = Rc::new(move |vals| {
                                apply_wifi_add(
                                    kind,
                                    demo,
                                    &mock,
                                    &ssid,
                                    vals.first().map(String::as_str).unwrap_or(""),
                                )
                            });
                            t.popup = Some(PopupState {
                                title: " wifi: change password ".into(),
                                labels: vec!["pass"],
                                inputs: vec![Input::with_str(&pass)],
                                cur: 0,
                                submit,
                            });
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

fn render_device_tab(f: &mut Frame, area: Rect, t: &DeviceTab, editing: Option<&(usize, Input)>) {
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

    fn mock() -> Mock {
        Rc::new(std::cell::RefCell::new(sample_config(DeviceKind::VibeKeys)))
    }

    fn tab() -> DeviceTab {
        let mut t = DeviceTab::new(DeviceKind::VibeKeys, true);
        t.config = Some(t.mock.borrow().clone());
        t.rebuild_rows();
        t
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
        let m = mock();
        mock_wifi_add(&m, "Cafe", "pw");
        assert_eq!(m.borrow()["wifi_list"].as_array().unwrap().len(), 3);
        mock_wifi_add(&m, "Cafe", "pw2");
        let list = m.borrow()["wifi_list"].clone();
        let list = list.as_array().unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[2]["pass"], "pw2");
    }

    #[test]
    fn mock_wifi_remove_filters() {
        let m = mock();
        mock_wifi_remove(&m, "Home");
        let list = m.borrow()["wifi_list"].clone();
        let list = list.as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["ssid"], "Office");
    }

    #[test]
    fn mock_asr_set_keeps_unset_fields() {
        let m = mock();
        mock_asr_set(&m, Some("https://x"), None, Some("m2"));
        let asr = &m.borrow()["asr_config"];
        assert_eq!(asr["uri"], "https://x");
        assert_eq!(asr["api_key"], "sk-demo");
        assert_eq!(asr["model"], "m2");
    }

    #[test]
    fn mock_keymap_uses_physical_key_and_binding_parser() {
        let m = mock();
        mock_keymap(&m, "yolo", "\"/review\\n\"");
        let km = &m.borrow()["keymap"];
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
        let t = tab();
        // 第一条 wifi 行(Home)的改密码 pop 框提交 → 同 ssid 更新,不新增。
        let rows_after = t.rows.len();
        apply_wifi_add(DeviceKind::VibeKeys, true, &t.mock, "Home", "new-pass").unwrap();
        let m = t.mock.borrow();
        let list = m["wifi_list"].as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["pass"], "new-pass");
        drop(m);
        assert_eq!(t.rows.len(), rows_after);
    }

    #[test]
    fn apply_field_demo_updates_mock() {
        let m = mock();
        apply_field(DeviceKind::VibeKeys, true, &m, Field::ServerUrl, "mqtt://x").unwrap();
        assert_eq!(m.borrow()["server_url"], "mqtt://x");
        apply_field(DeviceKind::VibeKeys, true, &m, Field::MicModel, "ptt").unwrap();
        assert_eq!(m.borrow()["mic_model"], 0);
        apply_field(
            DeviceKind::VibeKeys,
            true,
            &m,
            Field::Keymap,
            "CUSTOM=\"/compact\\n\"",
        )
        .unwrap();
        assert_eq!(m.borrow()["keymap"]["CUSTOM"]["type"], "text");
        apply_field(DeviceKind::VibeKeys, true, &m, Field::MicModel, "bogus").unwrap_err();
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
        assert_eq!(plain[0].content, "中X文ab");
    }
}
