//! TUI 全部状态类型与纯逻辑:设备页状态机、包页状态、行模型、输入框。
//! 不碰终端、不做网络/BLE——阻塞动作在 [`super::actions`] 里,渲染在
//! [`super::components`] 里。

use crate::ble::{DeviceKind, FoundDevice, Target};
use crate::pkg::{PKGS, SELF};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, Mutex};

// ===== 应用 =====

/// 三个标签:键盘 / 手表(BLE 设备配置)+ 管理(vibetty / vibekeys 安装更新)。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Tab {
    #[default]
    Packages,
    Keyboard,
    Watch,
}

impl Tab {
    pub fn next(self) -> Self {
        match self {
            Self::Packages => Self::Keyboard,
            Self::Keyboard => Self::Watch,
            Self::Watch => Self::Packages,
        }
    }
}

// ===== 设备页 =====

/// 设备页连接状态。
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum Conn {
    #[default]
    /// 还没进入过(进入 tab 时弹入口菜单)。
    Fresh,
    /// 入口菜单:1 扫描设备 / 2 从网页配置。
    Menu,
    /// 网页配置:显示 setup 页地址。
    WebSetup,
    /// 后台扫描中。
    Scanning,
    /// 扫到候选设备,等用户挑(列表在 `pick`)。
    Pick,
    /// 正在连接(后台),存设备名供提示。
    Connecting(String),
    /// 已连上,配置界面就绪。
    Ready,
    /// 扫描/连接失败,可重试。
    Failed(String),
    /// 连上之后设备断开了:弹窗等回车,回扫描页。
    Disconnected,
}

/// 带光标的单行输入:支持左右/Home/End 移动、任意位置插入、前后删除。
#[derive(Clone, Default)]
pub struct Input {
    pub buf: String,
    /// 光标位置(字符下标,不是字节)。
    pub pos: usize,
}

impl Input {
    pub fn with_str(s: &str) -> Self {
        Self {
            buf: s.to_string(),
            pos: s.chars().count(),
        }
    }

    pub fn chars(&self) -> usize {
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

    pub fn left(&mut self) {
        self.pos = self.pos.saturating_sub(1);
    }

    pub fn right(&mut self) {
        if self.pos < self.chars() {
            self.pos += 1;
        }
    }

    pub fn home(&mut self) {
        self.pos = 0;
    }

    pub fn end(&mut self) {
        self.pos = self.chars();
    }

    pub fn insert(&mut self, c: char) {
        let b = self.byte_pos();
        self.buf.insert(b, c);
        self.pos += 1;
    }

    /// 删光标前的字符(Backspace)。
    pub fn backspace(&mut self) {
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
    pub fn delete(&mut self) {
        let b = self.byte_pos();
        let Some(c) = self.buf[b..].chars().next() else {
            return;
        };
        self.buf.replace_range(b..b + c.len_utf8(), "");
    }

    /// 渲染为 spans。光标处字符反显粗块包住;光标在末尾时画一个色块。
    pub fn spans(&self, cursor: bool, base: Style) -> Vec<Span<'static>> {
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

/// pop 框种类。
#[derive(Clone, Default)]
pub enum PopupKind {
    /// 新增 WiFi:ssid + pass 两个字段。
    #[default]
    WifiAdd,
    /// 改某条 WiFi 的密码。
    WifiPass { ssid: String },
}

impl PopupKind {
    /// (标题, 字段名列表)。
    pub fn spec(&self) -> (&'static str, &'static [&'static str]) {
        match self {
            Self::WifiAdd => (" add wifi ", &["ssid", "pass"]),
            Self::WifiPass { .. } => (" wifi: change password ", &["pass"]),
        }
    }
}

/// 运行中的 pop 框:字段展开成带光标的输入,提交按 kind 分发。
#[derive(Clone, Default)]
pub struct PopupState {
    pub kind: PopupKind,
    pub title: String,
    pub labels: Vec<&'static str>,
    pub inputs: Vec<Input>,
    pub cur: usize,
}

impl PopupState {
    /// `initials` 按字段顺序给初始值(改密码预填旧密码;新增 WiFi 全空)。
    pub fn new(kind: PopupKind, initials: Vec<String>) -> Self {
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
}

/// keymap 编辑弹窗:键列表光标 + 可选的行内编辑(None = 浏览模式)。
#[derive(Clone, Default)]
pub struct KeymapPopupState {
    pub sel: usize,
    pub editing: Option<(usize, Input)>,
}

/// 固件支持键位重映射的物理键(固件 bt_keyboard_mode.rs 的 KeymapConfig)。
/// YOLO 是 SWITCH 的用户别名。
pub const KEYMAP_KEYS: [&str; 8] = [
    "MIC",
    "CUSTOM",
    "ESC",
    "NEXT",
    "BACKSPACE",
    "SWITCH",
    "ACCEPT",
    "ROTATE",
];

/// 键位的当前绑定显示:优先 raw(用户原始输入),兜底紧凑序列化。
pub fn keymap_binding(cfg: &Value, key: &str) -> String {
    match cfg.get("keymap").and_then(|m| m.get(key)) {
        Some(b) => {
            let raw = b.get("raw").and_then(|v| v.as_str()).unwrap_or("");
            if !raw.is_empty() {
                return raw.to_string();
            }
            // raw 为空的绑定(历史空文本宏)视作未设置。
            let value = b.get("value").and_then(|v| v.as_str()).unwrap_or("");
            if value.is_empty() && b.get("type").and_then(|v| v.as_str()) == Some("text") {
                String::new()
            } else {
                b.to_string()
            }
        }
        None => String::new(),
    }
}

/// 可编辑字段:决定行内编辑提交时调用哪个动作。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    ServerUrl,
    AsrUri,
    AsrApiKey,
    AsrModel,
    MicModel,
    PreferAsr,
    Tailscale,
}

/// 编辑框的格式提示。
pub fn field_hint(field: Field) -> &'static str {
    match field {
        Field::ServerUrl => "mqtt://user:pass@host:1883",
        Field::AsrUri => "https://…/audio/transcriptions",
        Field::AsrApiKey => "api key",
        Field::AsrModel => "whisper-1",
        Field::MicModel => "ptt | toggle",
        Field::PreferAsr => "on | off",
        Field::Tailscale => "tskey-auth-…",
    }
}

/// 枚举选项的下一个值:当前值在列表里则取其后一个(循环),不在/为空则取第一个。
pub fn next_in_cycle<'a>(values: &'a [&'a str], current: &str) -> &'a str {
    match values.iter().position(|v| v.eq_ignore_ascii_case(current)) {
        Some(i) => values[(i + 1) % values.len()],
        None => values[0],
    }
}

/// 从 CONFIG 快照取一个字段的展示值(走点路径,缺失 = "")。
pub fn cfg_get(v: &Value, path: &[&str]) -> String {
    let mut cur = v;
    for p in path {
        match cur.get(p) {
            Some(next) => cur = next,
            None => return String::new(),
        }
    }
    match cur {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

pub fn asr_cfg(v: &Value, key: &str) -> String {
    cfg_get(v, &["asr_config", key])
}

#[derive(Clone)]
pub enum RowAction {
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
    /// 回车打开 keymap 编辑弹窗。
    KeymapPopup,
    /// 一条现有 WiFi:回车弹框改密码,`d` 删除。
    WifiEntry { ssid: String },
}

/// 设备页一行:所属节 + 标签 + 值 + 动作。纯数据,由 `build_rows` 生成。
#[derive(Clone)]
pub struct DevRow {
    pub section: &'static str,
    pub label: String,
    pub value: String,
    pub action: RowAction,
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

/// 按配置快照构建设备页全部行(wifi 列表是动态的)。
pub fn build_rows(kind: DeviceKind, cfg: &Value) -> Vec<DevRow> {
    let keys = kind == DeviceKind::VibeKeys;
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
            action: RowAction::WifiEntry { ssid },
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
        // keymap 按钮:回车打开键位编辑弹窗。
        let configured = KEYMAP_KEYS
            .iter()
            .filter(|k| !keymap_binding(cfg, k).is_empty())
            .count();
        rows.push(DevRow {
            section: "keyboard",
            label: "keymap".into(),
            value: format!("{configured} set"),
            action: RowAction::KeymapPopup,
        });
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

    rows
}

// ===== 包页 =====

/// 管理页的一个包条目:本地已装版本 + 线上 release 查询结果。
#[derive(Clone)]
pub struct Entry {
    pub pkg: &'static crate::pkg::Pkg,
    pub installed: Option<String>,
    pub latest: Option<String>,
    pub any: Option<String>,
    pub note: Option<String>,
}

impl Entry {
    pub fn new(pkg: &'static crate::pkg::Pkg) -> Self {
        Self {
            pkg,
            installed: None,
            latest: None,
            any: None,
            note: None,
        }
    }

    pub fn status(&self, allow_pre: bool) -> &'static str {
        if self.note.is_some() {
            return "lookup failed";
        }
        let target = self.target(allow_pre);
        match (&self.installed, target) {
            (Some(_), None) => "up to date",
            (None, _) => "not installed",
            (Some(v), Some(t)) if crate::pkg::normalize(v) == crate::pkg::normalize(&t) => {
                "up to date"
            }
            (Some(_), Some(_)) => "update available",
        }
    }

    /// 本次会更新到的 tag:pre 开 → 最新 release,否则最新稳定版。
    pub fn target(&self, allow_pre: bool) -> Option<String> {
        match allow_pre {
            true => self.any.clone(),
            false => self.latest.clone(),
        }
    }
}

/// 刷新全部条目的便捷入口(后台线程用)。vibeup 自己排第一,
/// 同样检测已装版本与线上 release,允许在 TUI 里自更新。
pub fn refresh_entries(dir: &Path) -> Vec<Entry> {
    let mut entries: Vec<Entry> = std::iter::once(Entry::new(&SELF))
        .chain(PKGS.iter().map(Entry::new))
        .collect();
    fetch_entries(&mut entries, dir);
    entries
}

/// 刷新全部条目(本地版本 + 线上 release,阻塞网络,勿在渲染/输入线程调用)。
pub fn fetch_entries(entries: &mut [Entry], dir: &Path) {
    for e in entries.iter_mut() {
        e.installed = crate::pkg::installed_version(e.pkg, dir);
        match crate::pkg::releases(e.pkg.repo) {
            Ok((latest, any)) => {
                e.latest = Some(latest);
                e.any = any;
                e.note = None;
            }
            Err(err) => e.note = Some(format!("{err:#}")),
        }
    }
}

/// 窄屏下的紧凑文案。
pub fn short_status(s: &str) -> &str {
    match s {
        "update available" => "update",
        "not installed" => "-",
        other => other,
    }
}

/// 按词折行(空格分词),长句适配固定框宽;超长单词硬放一行。
pub fn wrap_words(s: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for word in s.split_whitespace() {
        let w = word.chars().count();
        if !cur.is_empty() && cur_w + 1 + w > width {
            lines.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        if !cur.is_empty() {
            cur.push(' ');
            cur_w += 1;
        }
        cur.push_str(word);
        cur_w += w;
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Packages 页回车弹出的动作菜单:打开时锁定的包下标 + 菜单光标。
#[derive(Clone)]
pub struct PkgMenu {
    pub pkg: usize,
    pub sel: usize,
}

/// 菜单选项文案(下标即执行分支)。
pub const PKG_MENU_ITEMS: [&str; 3] = [
    "Install / update latest",
    "Install prerelease",
    "Uninstall",
];

/// 下载进度快照(后台线程写,UI 每 100ms 读一次)。
#[derive(Clone, Copy, Default)]
pub struct DownloadProgress {
    pub downloaded: u64,
    pub total: Option<u64>,
}

pub type SharedProgress = Arc<Mutex<DownloadProgress>>;

/// plugin 询问窗:检测到的 CLI 勾选列表(装完 vibekeys 后弹)。
#[derive(Clone, Debug)]
pub struct PluginAsk {
    /// (cli, install 子命令, 人读名)
    pub clis: Vec<(&'static str, &'static str, &'static str)>,
    pub checked: Vec<bool>,
    pub sel: usize,
}

/// Packages 页整体状态。
#[derive(Clone, Default)]
pub struct PkgState {
    pub entries: Vec<Entry>,
    pub sel: usize,
    /// 包页待执行动作(卸载;阻塞,后台执行)。
    /// r 键请求的后台刷新(ticker 取走执行)。
    pub refresh_pending: bool,
    /// 包页动作的 footer 消息(install_pkg/update_all/uninstall_pkg 写,调用方透出)。
    pub message: String,
    pub menu: Option<PkgMenu>,
    pub job_label: String,
    pub spin: usize,
    pub progress: Option<(u64, Option<u64>)>,
    pub running: bool,
    /// 装完 vibekeys 待处理:自动跑 install-script + 弹 plugin 询问。
    pub vibekeys_pending: bool,
    pub plugin_ask: Option<PluginAsk>,
}

// ===== 设备页整体状态 =====

/// 设备页渲染状态(每帧读;后台句柄在 [`DeviceRuntime`])。
#[derive(Clone, Default)]
pub struct DeviceState {
    pub kind: DeviceKind,
    /// demo 模式:动作落内存假配置,不碰蓝牙。
    pub demo: bool,
    pub conn: Conn,
    pub pick: Vec<FoundDevice>,
    pub pick_sel: usize,
    pub menu_sel: usize,
    pub web_url: Option<&'static str>,
    pub config: Option<Value>,
    pub connected_name: Option<String>,
    pub pending_name: Option<String>,
    pub rows: Vec<DevRow>,
    pub sel: usize,
    pub spin: usize,
    pub editing: Option<(usize, Input)>,
    pub popup: Option<PopupState>,
    /// keymap 编辑弹窗(键位列表 + 行内编辑)。
    pub keymap_popup: Option<KeymapPopupState>,
}


impl DeviceState {
    pub fn new(kind: DeviceKind, demo: bool) -> Self {
        Self {
            kind,
            demo,
            ..Default::default()
        }
    }

    /// 是否有扫描/连接任务在跑(转圈动画用)。
    pub fn job_active(&self) -> bool {
        matches!(self.conn, Conn::Scanning | Conn::Connecting(_))
    }

    /// 按当前配置快照重建行(动作后 / 连接成功后调用)。
    pub fn rebuild_rows(&mut self) {
        let empty = serde_json::json!({});
        let cfg = self.config.as_ref().unwrap_or(&empty);
        self.rows = build_rows(self.kind, cfg);
        self.sel = self.sel.min(self.rows.len() - 1);
    }
}

/// 设备页后台句柄:BLE 会话、扫描任务、demo 假配置。非 Clone,包在
/// `Arc<Mutex<>>` 里由 App 持有。
#[derive(Default)]
pub struct DeviceRuntime {
    pub job: Option<super::actions::Job>,
    pub session: Option<crate::ble::Session>,
    pub pending_target: Option<Target>,
    pub mock: Value,
}

impl DeviceRuntime {
    pub fn new(kind: DeviceKind) -> Self {
        Self {
            mock: super::demo::sample_config(kind),
            ..Default::default()
        }
    }
}

/// 两台设备的后台句柄集合。
#[derive(Default)]
pub struct Runtimes {
    pub keys: DeviceRuntime,
    pub watch: DeviceRuntime,
}

impl Runtimes {
    pub fn new() -> Self {
        Self {
            keys: DeviceRuntime::new(DeviceKind::VibeKeys),
            watch: DeviceRuntime::new(DeviceKind::Watch),
        }
    }
}

#[derive(Default)]
pub struct PkgRuntime {
    pub job: Option<super::actions::PkgJob>,
    pub progress: SharedProgress,
}
