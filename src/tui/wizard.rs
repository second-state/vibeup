//! `vibeup wizard` —— 对话式引导配置,step-by-step.md 的 TUI 实现。
//!
//! 线性状态机:每步一个居中弹屏(自绘 Widget,与 keymap 弹窗同套路),
//! 按键分发在 [`wizard_key`],后台轮询在 [`poll`]。设备写入复用常驻
//! `ble::Session`(demo 无会话,落 mock);安装复用 `install::install`
//! + 共享进度;demo 模式全程造假不碰真机。

use super::actions::scan_devices_thread;
use super::demo;
use super::state::{DownloadProgress, Input, SharedProgress};
use crate::ble::{self, DeviceKind, FoundDevice, Target};
use crate::config_cmd::build_keymap_config;
use crate::install;
use crate::pkg;
use ratatui_kit::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui_kit::prelude::*;
use ratatui_kit::ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph, Widget},
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

// ===== 步骤 =====

#[derive(Clone, PartialEq, Debug)]
pub enum Step {
    /// 1:要设置的是不是键盘
    IsKeyboard,
    /// 2/18:扫描 → 挑选 → 连接(device 指明键盘还是手表)
    Scan,
    /// 3:输入方式
    InputMethod,
    /// 4:捕捉输入法快捷键 → MIC + prefer_asr=off
    HotkeyCapture,
    /// 5:whisper API url + api-key → asr + prefer_asr=on
    AsrApi,
    /// 6:装 vibekeys 包?
    VibekeysApp,
    /// 6a:装 Claude Code plugin(显示命令,yes 才装)
    PluginClaude,
    /// 6b:装 Codex plugin(显示命令,yes 才装)
    PluginCodex,
    /// 8:装 vibetty?
    InstallVibettyAsk,
    /// 10/19:局域网外用 remote?
    RemoteOutside,
    /// 11:有 tailscale 账号?
    HasTailscale,
    /// 11b:注册 tailscale / emqx / 自建
    BrokerChoice,
    /// 12:tailscale auth-key
    TailscaleKey,
    /// 13:本机 tailscale ip 检查(可重试)
    TailscaleIp,
    /// 15:手输 broker 地址(emqx / 自建)
    BrokerInput,
    /// 17:配置完成确认页(✓ 总结,Enter 继续)
    Done,
    /// 18 之前:是否继续配置手表
    WatchAsk,
}

/// 后台任务种类(决定任务完成后的去向)。
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum JobKind {
    Scan,
    DemoConnect,
    InstallVibekeys,
    InstallVibetty,
    PluginClaude,
    PluginCodex,
}

// ===== 状态 =====

#[derive(Clone)]
pub struct WizardState {
    pub step: Step,
    pub demo: bool,
    pub dry_run: bool,
    /// 当前在配置的设备(键盘 or 手表)。
    pub device: DeviceKind,
    /// 扫描结果与选中项。
    pub pick: Vec<FoundDevice>,
    pub pick_sel: usize,
    /// 正在连接的设备名(Some = 连接中)。
    pub connecting: Option<String>,
    /// 本步错误(alert 框)。
    pub err: Option<String>,
    /// 选项列表光标。
    pub choice: usize,
    /// 文本输入(单/双字段)。
    pub input: Input,
    pub input2: Input,
    pub field2: bool,
    /// 步骤 4 捕捉到的快捷键。
    pub captured: Option<String>,
    /// Some(label) = 后台任务进行中,吞掉按键。
    pub busy: Option<String>,
    pub job_kind: Option<JobKind>,
    pub progress: Option<(u64, Option<u64>)>,
    pub spin: usize,
    /// 步骤 13 查到的本机 tailscale ip。
    pub ts_ip: Option<String>,
    /// 已完成的配置项(步骤 17/21 总结)。
    pub log: Vec<String>,
    /// 手表已配置完。
    pub watch_done: bool,
    /// vibetty 已就绪(本来就有或本次装的)。
    pub vibetty_done: bool,
    /// plugin 询问已问过(不管答了什么)。
    pub plugin_claude_asked: bool,
    pub plugin_codex_asked: bool,
}

impl WizardState {
    pub fn new(demo: bool, dry_run: bool) -> Self {
        Self {
            demo,
            dry_run,
            device: DeviceKind::VibeKeys,
            step: Step::IsKeyboard,
            pick: Vec::new(),
            pick_sel: 0,
            connecting: None,
            err: None,
            choice: 0,
            input: Input::default(),
            input2: Input::default(),
            field2: false,
            captured: None,
            busy: None,
            job_kind: None,
            progress: None,
            spin: 0,
            ts_ip: None,
            log: Vec::new(),
            watch_done: false,
            vibetty_done: false,
            plugin_claude_asked: false,
            plugin_codex_asked: false,
        }
    }
}

/// 后台句柄(非 Clone,进 Arc<Mutex> 供事件 handler 与 ticker 共用)。
pub struct WizardRuntime {
    pub job: Option<WizJob>,
    pub session: Option<ble::Session>,
    /// 真机连接中的设备名(Session::open 无 job,名字先存这里)。
    pub pending_name: Option<String>,
    pub progress: SharedProgress,
    /// demo 模式的假配置(无会话,写入落这里)。
    pub mock: serde_json::Value,
}

impl Default for WizardRuntime {
    fn default() -> Self {
        Self {
            job: None,
            session: None,
            pending_name: None,
            progress: Arc::new(Mutex::new(DownloadProgress::default())),
            mock: serde_json::Value::Null,
        }
    }
}

// ===== 后台任务(线程 + channel,actions 同款)=====

pub struct WizJob {
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    rx: std::sync::mpsc::Receiver<WizOut>,
}

pub enum WizOut {
    Scan(anyhow::Result<Vec<FoundDevice>>),
    Connected(anyhow::Result<serde_json::Value>),
    Installed(anyhow::Result<String>),
}

impl WizJob {
    fn is_done(&self) -> bool {
        self.done.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn try_recv(&self) -> Option<WizOut> {
        self.rx.try_recv().ok()
    }
}

fn spawn_wiz(f: impl FnOnce() -> WizOut + Send + 'static) -> WizJob {
    use std::sync::atomic::{AtomicBool, Ordering};
    let (tx, rx) = std::sync::mpsc::channel();
    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    std::thread::spawn(move || {
        let _ = tx.send(f());
        flag.store(true, Ordering::Relaxed);
    });
    WizJob { done, rx }
}

// ===== 主机侧探测/写入 =====

/// vibetty 二进制版本(未装 = None)。
fn vibetty_binary(dir: &Path) -> Option<String> {
    let p = pkg::find("vibetty").ok()?;
    pkg::installed_version(p, dir)
}

/// `~/.vibetty/config.toml` 里 `[mqtt] broker` 的值。
pub fn vibetty_broker() -> Option<String> {
    let path = dirs::home_dir()?.join(".vibetty").join("config.toml");
    let content = std::fs::read_to_string(path).ok()?;
    let mut in_mqtt = false;
    for line in content.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_mqtt = t.eq_ignore_ascii_case("[mqtt]");
            continue;
        }
        if in_mqtt
            && let Some((k, v)) = t.split_once('=')
            && k.trim() == "broker"
        {
            let s = v.trim().trim_matches('"').trim();
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// 写 `~/.vibetty/config.toml` 的 `[mqtt] broker`。文件不存在就建;已存在
/// 则行级替换/插入,保留其余内容(不引 toml 依赖,注释也保得住)。
pub fn set_vibetty_broker(url: &str) -> anyhow::Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    let path = home.join(".vibetty").join("config.toml");
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = content.lines().map(String::from).collect();
    let mut mqtt_at: Option<usize> = None;
    let mut broker_at: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if t.starts_with('[') {
            if t.eq_ignore_ascii_case("[mqtt]") {
                mqtt_at = Some(i);
            } else if mqtt_at.is_some() {
                break; // [mqtt] 段结束,没找到 broker 行
            }
            continue;
        }
        if mqtt_at.is_some()
            && let Some((k, _)) = t.split_once('=')
            && k.trim() == "broker"
        {
            broker_at = Some(i);
            break;
        }
    }
    let newline = format!("broker = \"{url}\"");
    match (mqtt_at, broker_at) {
        (_, Some(i)) => lines[i] = newline,
        (Some(i), None) => lines.insert(i + 1, newline),
        (None, _) => {
            if !lines.is_empty() && !lines[lines.len() - 1].trim().is_empty() {
                lines.push(String::new());
            }
            lines.push("[mqtt]".into());
            lines.push(newline);
        }
    }
    let mut out = lines.join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    std::fs::write(&path, out)?;
    Ok(())
}

/// 本机局域网 ip(UDP connect 技巧,不发包)。
fn lan_ip() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    Some(s.local_addr().ok()?.ip())
}

/// `tailscale ip -4`(命令不存在/未登录 = None)。
fn tailscale_ip() -> Option<String> {
    let out = std::process::Command::new("tailscale")
        .args(["ip", "-4"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// 步骤 15 的 broker 地址校验:scheme 合法 + host 非空。
fn broker_url_valid(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    if !matches!(scheme, "mqtt" | "mqtts" | "ws" | "wss") {
        return false;
    }
    let authority = rest.split(['/', '?']).next().unwrap_or("");
    // 允许 user:pass@host:port,取 @ 后面的部分。
    let host = authority.rsplit('@').next().unwrap_or("");
    !host.is_empty()
}

fn cli_on_path(cli: &str) -> bool {
    std::process::Command::new(cli)
        .arg("--version")
        .output()
        .is_ok()
}

/// 把按键事件转成 keymap binding 字符串("Ctrl+Space" / "A" / "F5")。
/// Esc 与 Ctrl-C 不参与捕捉;字符键的 Shift 并入大写。
fn key_event_binding(key: &KeyEvent) -> Option<String> {
    let m = key.modifiers;
    if key.code == KeyCode::Esc
        || (key.code == KeyCode::Char('c') && m.contains(KeyModifiers::CONTROL))
    {
        return None;
    }
    let name: String = match key.code {
        KeyCode::Char(' ') => "Space".into(),
        KeyCode::Char(c) => c.to_ascii_uppercase().to_string(),
        KeyCode::F(n) => format!("F{n}"),
        KeyCode::Enter => "Enter".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::Backspace => "Backspace".into(),
        KeyCode::Insert => "Insert".into(),
        KeyCode::Home => "Home".into(),
        KeyCode::End => "End".into(),
        KeyCode::PageUp => "PageUp".into(),
        KeyCode::PageDown => "PageDown".into(),
        KeyCode::Up => "Up".into(),
        KeyCode::Down => "Down".into(),
        KeyCode::Left => "Left".into(),
        KeyCode::Right => "Right".into(),
        _ => return None,
    };
    let mut parts: Vec<String> = Vec::new();
    if m.contains(KeyModifiers::CONTROL) {
        parts.push("Ctrl".into());
    }
    if m.intersects(KeyModifiers::ALT | KeyModifiers::META | KeyModifiers::SUPER) {
        parts.push("Alt".into());
    }
    if m.contains(KeyModifiers::SHIFT) && !matches!(key.code, KeyCode::Char(_)) {
        parts.push("Shift".into());
    }
    parts.push(name);
    Some(parts.join("+"))
}

// ===== 会话写入(有会话走真机;无会话 = demo,落 mock)=====

fn wiz_write_field(
    st: &mut WizardState,
    rt: &mut WizardRuntime,
    field: &str,
    value: serde_json::Value,
) -> anyhow::Result<()> {
    let Some(session) = rt.session.as_ref() else {
        if rt.mock.is_null() {
            anyhow::bail!("device not connected");
        }
        if let serde_json::Value::Object(map) = &mut rt.mock {
            map.insert(field.to_string(), value);
        } else {
            rt.mock = serde_json::json!({ field: value });
        }
        let _ = st;
        return Ok(());
    };
    match session.write_config_field(field, value) {
        Ok(()) => Ok(()),
        // 写完设备端主动断连不算失败(手表配网路径同款容忍)。
        Err(e) if e.to_string().to_lowercase().contains("disconnect") => {
            rt.session = None;
            Ok(())
        }
        Err(e) => Err(e),
    }
}

fn wiz_write_keymap(
    rt: &mut WizardRuntime,
    key: &str,
    binding: &str,
) -> anyhow::Result<()> {
    let Some(session) = rt.session.as_ref() else {
        if rt.mock.is_null() {
            anyhow::bail!("device not connected");
        }
        demo::mock_keymap(&mut rt.mock, key, binding);
        return Ok(());
    };
    let payload = build_keymap_config(key, binding);
    session.write_keymap(&payload)
}

// ===== 流程推进 =====

fn kick_scan(st: &mut WizardState, rt: &mut WizardRuntime) {
    st.pick.clear();
    st.pick_sel = 0;
    st.err = None;
    st.connecting = None;
    st.busy = Some(format!("scanning for {}", st.device.name()));
    st.job_kind = Some(JobKind::Scan);
    st.progress = None;
    let kind = st.device;
    let demo = st.demo;
    rt.job = Some(spawn_wiz(move || match demo {
        true => {
            std::thread::sleep(std::time::Duration::from_millis(900));
            WizOut::Scan(Ok(demo::demo_devices(kind)))
        }
        false => WizOut::Scan(scan_devices_thread(kind)),
    }));
}

/// demo 假连接(真机走 Session::open,Ready 事件驱动)。
fn demo_connect(st: &mut WizardState, rt: &mut WizardRuntime, name: String) {
    st.connecting = Some(name);
    st.busy = Some(format!("connecting to {}", st.device.name()));
    st.job_kind = Some(JobKind::DemoConnect);
    let kind = st.device;
    rt.job = Some(spawn_wiz(move || {
        std::thread::sleep(std::time::Duration::from_millis(400));
        WizOut::Connected(Ok(demo::sample_config(kind)))
    }));
}

fn start_install(st: &mut WizardState, rt: &mut WizardRuntime, kind: JobKind) {
    let (name, label) = match kind {
        JobKind::InstallVibekeys => ("vibekeys", "installing vibekeys"),
        JobKind::InstallVibetty => ("vibetty", "installing vibetty"),
        _ => return,
    };
    let Ok(p) = pkg::find(name) else {
        st.err = Some(format!("unknown package `{name}`"));
        return;
    };
    st.busy = Some(label.to_string());
    st.job_kind = Some(kind);
    st.progress = None;
    if let Ok(mut g) = rt.progress.lock() {
        *g = DownloadProgress::default();
    }
    let dry_run = st.dry_run;
    if st.demo {
        // demo:不真下载,假任务 1s 后成功。
        st.job_kind = Some(kind);
        let label = label.to_string();
        rt.job = Some(spawn_wiz(move || {
            std::thread::sleep(std::time::Duration::from_secs(1));
            WizOut::Installed(Ok(format!("{label} (demo)\n")))
        }));
        return;
    }
    let progress = rt.progress.clone();
    let tick_progress = progress.clone();
    rt.job = Some(spawn_wiz(move || {
        let tick = move |d: u64, t: Option<u64>| {
            if let Ok(mut g) = tick_progress.lock() {
                g.downloaded = d;
                g.total = t;
            }
        };
        match install::install(
            p,
            None,
            false,
            &pkg::install_dir(None),
            dry_run,
            false,
            &tick,
        ) {
            Ok(log) => WizOut::Installed(Ok(log)),
            Err(e) => WizOut::Installed(Err(e)),
        }
    }));
}

/// 装 (cli, label) 的 agent plugin:marketplace add + plugin install。
/// hook 配置都在 plugin 里,装上即生效。返回总结页要显示的行。
fn install_plugin(cli: &str, action: &str, label: &str) -> String {
    let mkt = std::process::Command::new(cli)
        .args(["plugin", "marketplace", "add", "second-state/marketplace"])
        .output();
    let inst = std::process::Command::new(cli)
        .args(["plugin", action, "vibekeys@second-state-tools"])
        .output();
    match (mkt, inst) {
        (Ok(a), Ok(b)) if a.status.success() && b.status.success() => {
            format!("{label} plugin installed")
        }
        _ => format!(
            "{label} plugin install failed — later: `{cli} plugin marketplace add second-state/marketplace` + `{cli} plugin {action} vibekeys@second-state-tools`"
        ),
    }
}

/// demo 假装装 plugin(1s 成功)。
fn demo_install_plugin(label: &str) -> String {
    std::thread::sleep(std::time::Duration::from_secs(1));
    format!("{label} plugin installed (demo)")
}

/// 连接成功后的分流:键盘 → 输入方式;手表 → 确保 vibetty → remote 询问。
fn after_connect(st: &mut WizardState, dir: &Path) {
    st.log.push(format!("connected to {}", st.device.name()));
    match st.device {
        DeviceKind::VibeKeys => st.step = Step::InputMethod,
        _ => {
            let missing = st.demo || vibetty_binary(dir).is_none();
            if !st.vibetty_done && missing {
                // 19 注:手表路径没 vibetty 先问装(同 8-9)。
                st.choice = 0;
                prep_form(st);
                st.step = Step::InstallVibettyAsk;
            } else {
                st.step = Step::RemoteOutside;
            }
        }
    }
}

/// plugin 链推进:Claude → Codex → 步骤 7。CLI 不在本机的直接跳过。
fn plugin_next(st: &mut WizardState, rt: &mut WizardRuntime, dir: &Path) {
    st.choice = 0;
    if !st.plugin_claude_asked && cli_on_path("claude") {
        st.plugin_claude_asked = true;
        prep_form(st);
        st.step = Step::PluginClaude;
        return;
    }
    if !st.plugin_codex_asked && cli_on_path("codex") {
        st.plugin_codex_asked = true;
        prep_form(st);
        st.step = Step::PluginCodex;
        return;
    }
    vibetty_check(st, rt, dir);
}

/// 步骤 7:vibetty 装配状态分流(二进制与 config 两者都查)。
/// demo 不查真机,一律当没装,走 8→9→10 完整流程。
fn vibetty_check(st: &mut WizardState, rt: &mut WizardRuntime, dir: &Path) {
    let (bin, cfg) = if st.demo {
        (false, None)
    } else {
        (vibetty_binary(dir).is_some(), vibetty_broker())
    };
    match (bin, cfg) {
        (true, Some(url)) => {
            // 16:把 vibetty 现有 broker 通过 ble 发给键盘。
            match wiz_write_field(st, rt, "server_url", serde_json::json!(url)) {
                Ok(()) => {
                    st.log.push(format!("server_url = {url} (from vibetty config)"));
                    st.vibetty_done = true;
                    st.choice = 0;
                    st.step = Step::Done;
                }
                Err(e) => {
                    st.err = Some(format!("{e:#}"));
                    st.step = Step::RemoteOutside;
                }
            }
        }
        (true, None) => {
            st.choice = 0;
            st.step = Step::RemoteOutside;
        }
        (false, _) => {
            st.choice = 0;
            st.step = Step::InstallVibettyAsk;
        }
    }
}

/// 14/14a/15:当前设备写 server-url + 主机 vibetty config 写 broker。
fn apply_broker(st: &mut WizardState, rt: &mut WizardRuntime, device_url: String, vibetty_url: &str) {
    if let Err(e) = wiz_write_field(st, rt, "server_url", serde_json::json!(device_url)) {
        st.err = Some(format!("{e:#}"));
        return;
    }
    st.log
        .push(format!("{} server_url = {device_url}", st.device.name()));
    if st.demo {
        // demo:不碰真机文件。
        st.log
            .push(format!("vibetty broker = {vibetty_url} (demo)"));
        if st.device != DeviceKind::VibeKeys {
            st.watch_done = true;
        }
        st.choice = 0;
        st.step = Step::Done;
        return;
    }
    if let Err(e) = set_vibetty_broker(vibetty_url) {
        st.err = Some(format!("vibetty config: {e:#}"));
        return;
    }
    st.log.push(format!("vibetty broker = {vibetty_url}"));
    if st.device != DeviceKind::VibeKeys {
        st.watch_done = true;
    }
    st.choice = 0;
    st.step = Step::Done;
}

/// 进任何表单步骤前复位输入状态(防 field2/旧文本残留)。
fn prep_form(st: &mut WizardState) {
    st.input = Input::default();
    st.input2 = Input::default();
    st.field2 = false;
}

pub struct Outcome {
    pub exit: bool,
}

impl Outcome {
    fn none() -> Self {
        Self { exit: false }
    }
    fn exit() -> Self {
        Self { exit: true }
    }
}

/// 向导按键分发。返回 exit = 退出整个向导。
pub fn wizard_key(
    st: &mut WizardState,
    rt: &mut WizardRuntime,
    key: &KeyEvent,
    dir: &Path,
) -> Outcome {
    // 退出优先;文本输入步骤里 q 是普通字符(Ctrl-C 始终可退)。
    let text_step = matches!(
        st.step,
        Step::AsrApi | Step::TailscaleKey | Step::BrokerInput
    );
    if key.code == KeyCode::Esc
        || (key.code == KeyCode::Char('q') && !text_step)
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
    {
        return Outcome::exit();
    }
    if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
        return Outcome::none();
    }
    // 后台任务进行中:吞按键。
    if st.busy.is_some() {
        return Outcome::none();
    }
    st.err = None;

    let enter = key.code == KeyCode::Enter;
    let up = key.code == KeyCode::Up;
    let down = key.code == KeyCode::Down;

    match st.step {
        Step::IsKeyboard => {
            if up {
                st.choice = st.choice.saturating_sub(1);
            } else if down {
                st.choice = (st.choice + 1).min(1);
            } else if enter {
                if st.choice == 0 {
                    st.device = DeviceKind::VibeKeys;
                    st.step = Step::Scan;
                    kick_scan(st, rt);
                } else {
                    st.step = Step::WatchAsk;
                }
            }
        }

        Step::Scan => {
            if !st.pick.is_empty() {
                if up {
                    st.pick_sel = st.pick_sel.saturating_sub(1);
                } else if down {
                    st.pick_sel = (st.pick_sel + 1).min(st.pick.len() - 1);
                } else if enter {
                    let dev = st.pick[st.pick_sel].clone();
                    st.err = None;
                    rt.pending_name = Some(dev.name.clone());
                    match st.demo {
                        true => {
                            st.pick.clear();
                            demo_connect(st, rt, dev.name);
                        }
                        false => {
                            st.connecting = Some(dev.name);
                            rt.session =
                                Some(ble::Session::open(Target::picked(st.device, dev.id)));
                        }
                    }
                }
            } else if enter {
                // 出错后的重扫。
                kick_scan(st, rt);
            }
        }

        Step::InputMethod => {
            if up {
                st.choice = st.choice.saturating_sub(1);
            } else if down {
                st.choice = (st.choice + 1).min(1);
            } else if enter {
                let local = st.choice == 0;
                st.input = Input::default();
                st.input2 = Input::default();
                st.field2 = false;
                st.captured = None;
                st.step = if local {
                    Step::HotkeyCapture
                } else {
                    Step::AsrApi
                };
            }
        }

        Step::HotkeyCapture => {
            if enter {
                let Some(binding) = st.captured.clone() else {
                    return Outcome::none();
                };
                let res = wiz_write_keymap(rt, "MIC", &binding).and_then(|()| {
                    wiz_write_field(st, rt, "prefer_builtin_asr", serde_json::json!(false))
                });
                match res {
                    Ok(()) => {
                        st.log.push(format!("keyboard MIC = {binding}"));
                        st.log.push("prefer_builtin_asr = off".into());
                        st.choice = 0;
                        st.step = Step::VibekeysApp;
                    }
                    Err(e) => st.err = Some(format!("{e:#}")),
                }
            } else if let Some(binding) = key_event_binding(key) {
                st.captured = Some(binding);
            }
        }

        Step::AsrApi => {
            if key.code == KeyCode::Tab || up || down {
                st.field2 = !st.field2;
            } else if enter {
                if !st.field2 {
                    st.field2 = true;
                } else {
                    let url = st.input.buf.trim().to_string();
                    let api_key = st.input2.buf.trim().to_string();
                    if url.is_empty() {
                        st.err = Some("whisper API url is required".into());
                        st.field2 = false;
                        return Outcome::none();
                    }
                    let asr = serde_json::json!({
                        "platform": "whisper",
                        "uri": url,
                        "api_key": api_key,
                        "model": null,
                    });
                    let res = wiz_write_field(st, rt, "asr_config", asr).and_then(|()| {
                        wiz_write_field(st, rt, "prefer_builtin_asr", serde_json::json!(true))
                    });
                    match res {
                        Ok(()) => {
                            st.log.push(format!("asr_config = {url}"));
                            st.log.push("prefer_builtin_asr = on".into());
                            st.choice = 0;
                            st.step = Step::VibekeysApp;
                        }
                        Err(e) => st.err = Some(format!("{e:#}")),
                    }
                }
            } else {
                feed_input(st, key);
            }
        }

        Step::VibekeysApp => {
            if up {
                st.choice = st.choice.saturating_sub(1);
            } else if down {
                st.choice = (st.choice + 1).min(1);
            } else if enter {
                if st.choice == 0 {
                    start_install(st, rt, JobKind::InstallVibekeys);
                } else {
                    st.log.push("skipped vibekeys".into());
                    vibetty_check(st, rt, dir);
                }
            }
        }

        Step::PluginClaude | Step::PluginCodex => {
            if up {
                st.choice = st.choice.saturating_sub(1);
            } else if down {
                st.choice = (st.choice + 1).min(1);
            } else if enter {
                let (cli, action, label, kind) = if st.step == Step::PluginClaude {
                    ("claude", "install", "Claude Code", JobKind::PluginClaude)
                } else {
                    ("codex", "add", "Codex", JobKind::PluginCodex)
                };
                if st.choice == 0 {
                    // yes:后台执行两条命令(demo 假装成功)。
                    st.busy = Some(format!("installing {label} plugin"));
                    st.job_kind = Some(kind);
                    st.progress = None;
                    let demo = st.demo;
                    rt.job = Some(spawn_wiz(move || {
                        let res = match demo {
                            true => demo_install_plugin(label),
                            false => install_plugin(cli, action, label),
                        };
                        WizOut::Installed(Ok(res))
                    }));
                } else {
                    st.log.push(format!("{label} plugin skipped"));
                    plugin_next(st, rt, dir);
                }
            }
        }

        Step::InstallVibettyAsk => {
            if up {
                st.choice = st.choice.saturating_sub(1);
            } else if down {
                st.choice = (st.choice + 1).min(1);
            } else if enter {
                if st.choice == 0 {
                    start_install(st, rt, JobKind::InstallVibetty);
                } else if st.device == DeviceKind::VibeKeys {
                    // 8 否 → 17(先看完成页,再问手表)
                    st.choice = 0;
                    st.step = Step::Done;
                } else {
                    // 手表只有 remote,不装 vibetty 也继续配 broker。
                    st.step = Step::RemoteOutside;
                }
            }
        }

        Step::RemoteOutside => {
            if up {
                st.choice = st.choice.saturating_sub(1);
            } else if down {
                st.choice = (st.choice + 1).min(1);
            } else if enter {
                if st.choice == 0 {
                    st.step = Step::HasTailscale;
                } else {
                    // 14a:局域网模式。
                    match lan_ip() {
                        Some(ip) => {
                            apply_broker(st, rt, format!("{ip}:1883"), "mqtt://127.0.0.1:1883");
                        }
                        None => {
                            st.err = Some("could not detect the LAN IP — is the machine online?".into());
                        }
                    }
                }
            }
        }

        Step::HasTailscale => {
            if up {
                st.choice = st.choice.saturating_sub(1);
            } else if down {
                st.choice = (st.choice + 1).min(1);
            } else if enter {
                let has = st.choice == 0;
                st.choice = 0;
                prep_form(st);
                st.step = if has {
                    Step::TailscaleKey
                } else {
                    Step::BrokerChoice
                };
            }
        }

        Step::BrokerChoice => {
            if up {
                st.choice = st.choice.saturating_sub(1);
            } else if down {
                st.choice = (st.choice + 1).min(2);
            } else if enter {
                let pick = st.choice;
                prep_form(st);
                match pick {
                    0 => st.step = Step::TailscaleKey,
                    _ => st.step = Step::BrokerInput,
                }
            }
        }

        Step::TailscaleKey => {
            if enter {
                let key = st.input.buf.trim().to_string();
                if key.is_empty() {
                    st.err = Some("paste the auth key first".into());
                    return Outcome::none();
                }
                match wiz_write_field(st, rt, "tailscale_key", serde_json::json!(key)) {
                    Ok(()) => {
                        st.log.push("tailscale auth key written to device".into());
                        // 13:立刻查本机 tailscale ip;查到直接落 broker。
                        st.ts_ip = tailscale_ip();
                        match st.ts_ip.clone() {
                            Some(ip) => {
                                apply_broker(st, rt, format!("{ip}:1883"), "mqtt://127.0.0.1:1883");
                            }
                            None => st.step = Step::TailscaleIp,
                        }
                    }
                    Err(e) => st.err = Some(format!("{e:#}")),
                }
            } else {
                feed_input(st, key);
            }
        }

        Step::TailscaleIp => match key.code {
            KeyCode::Enter | KeyCode::Char('r') => {
                st.ts_ip = tailscale_ip();
                if let Some(ip) = st.ts_ip.clone() {
                    apply_broker(st, rt, format!("{ip}:1883"), "mqtt://127.0.0.1:1883");
                }
            }
            KeyCode::Char('e') => {
                st.choice = 2;
                prep_form(st);
                st.step = Step::BrokerInput;
            }
            _ => {}
        },

        Step::BrokerInput => {
            if enter {
                let url = st.input.buf.trim().to_string();
                if !broker_url_valid(&url) {
                    st.err = Some(
                        "not a valid broker URL — mqtt:// mqtts:// ws:// or wss:// with a host"
                            .into(),
                    );
                    return Outcome::none();
                }
                let vibetty_url = url.clone();
                apply_broker(st, rt, url, &vibetty_url);
            } else {
                feed_input(st, key);
            }
        }

        // 17:完成确认页。Enter 继续;手表已配完则直接结束。
        Step::Done => {
            if enter {
                if st.watch_done {
                    return Outcome::exit();
                }
                st.choice = 0;
                st.step = Step::WatchAsk;
            }
        }

        // 是否继续配置手表。
        Step::WatchAsk => {
            if up {
                st.choice = st.choice.saturating_sub(1);
            } else if down {
                st.choice = (st.choice + 1).min(1);
            } else if enter {
                if st.choice == 0 {
                    st.device = DeviceKind::Watch;
                    st.step = Step::Scan;
                    kick_scan(st, rt);
                } else {
                    return Outcome::exit();
                }
            }
        }
    }

    Outcome::none()
}

/// 表单字符输入(field2 决定落在哪个框)。
fn feed_input(st: &mut WizardState, key: &KeyEvent) {
    let second = st.field2;
    let input = if second { &mut st.input2 } else { &mut st.input };
    match key.code {
        KeyCode::Char(c) => {
            if key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::META)
            {
                return;
            }
            input.insert(c);
        }
        KeyCode::Backspace => input.backspace(),
        KeyCode::Delete => input.delete(),
        KeyCode::Left => input.left(),
        KeyCode::Right => input.right(),
        KeyCode::Home => input.home(),
        KeyCode::End => input.end(),
        _ => {}
    }
}

// ===== 后台轮询 =====

/// 100ms 一拍:收扫描/连接/安装任务结果与会话事件,推进进度/转圈。
pub fn poll(st: &mut WizardState, rt: &mut WizardRuntime, dir: &Path) {
    // 会话事件(真机连接的 Ready/Disconnected)。
    if let Some(ev) = rt.session.as_ref().and_then(|s| s.try_event()) {
        match ev {
            ble::SessionEvent::Ready(res) => {
                st.connecting = None;
                rt.pending_name = None;
                match res {
                    Ok(_) => after_connect(st, dir),
                    Err(e) => {
                        rt.session = None;
                        st.err = Some(format!("connect failed: {e:#} — Enter to scan again"));
                    }
                }
            }
            ble::SessionEvent::Disconnected(e) => {
                rt.session = None;
                if st.connecting.take().is_some() {
                    st.err = Some(format!("{e:#} — Enter to scan again"));
                } else {
                    // 已连接状态下掉线:回扫描页重来。
                    st.err = Some(format!("device disconnected: {e:#}"));
                    kick_scan(st, rt);
                }
            }
        }
    }

    // 后台任务。
    if rt.job.as_ref().is_some_and(|j| j.is_done()) {
        let job = rt.job.take().expect("checked above");
        let out = job.try_recv();
        let kind = st.job_kind.take();
        st.busy = None;
        st.progress = None;
        match out {
            Some(WizOut::Scan(res)) => match res {
                Ok(devs) if !devs.is_empty() => {
                    st.pick = devs;
                    st.pick_sel = 0;
                }
                Ok(_) => {
                    st.err = Some(
                        "no devices found — press ACCEPT on the device, then Enter".into(),
                    );
                }
                Err(e) => st.err = Some(format!("{e:#} — Enter to scan again")),
            },
            Some(WizOut::Connected(Ok(cfg))) => {
                st.connecting = None;
                rt.mock = cfg; // demo:后续写入落这份假配置
                after_connect(st, dir);
            }
            Some(WizOut::Connected(Err(e))) => {
                st.connecting = None;
                st.err = Some(format!("connect failed: {e:#} — Enter to scan again"));
            }
            Some(WizOut::Installed(Ok(log))) => match kind {
                Some(JobKind::InstallVibekeys) => {
                    st.log.push("installed vibekeys".into());
                    plugin_next(st, rt, dir);
                }
                Some(JobKind::InstallVibetty) => {
                    st.vibetty_done = true;
                    st.log.push("installed vibetty".into());
                    st.choice = 0;
                    st.step = Step::RemoteOutside;
                }
                Some(JobKind::PluginClaude) => {
                    st.log.push(log.trim().to_string());
                    plugin_next(st, rt, dir);
                }
                Some(JobKind::PluginCodex) => {
                    st.log.push(log.trim().to_string());
                    vibetty_check(st, rt, dir);
                }
                _ => {}
            },
            Some(WizOut::Installed(Err(e))) => {
                st.err = Some(format!("{e:#}"));
            }
            _ => {}
        }
    }

    // 进行中:进度 + 转圈。
    if st.busy.is_some() {
        st.spin = st.spin.wrapping_add(1);
        if let Ok(p) = rt.progress.lock() {
            st.progress = Some((p.downloaded, p.total));
        }
    }
}

// ===== 渲染 =====

fn dim() -> Style {
    Style::new().dark_gray()
}

fn err_style() -> Style {
    Style::new().red()
}

fn hint_line(text: &str) -> Line<'static> {
    Line::from(Span::styled(format!(" {text} "), dim()))
}

fn text_line(text: &str) -> Line<'static> {
    Line::from(format!(" {text}"))
}

fn dim_text(text: &str) -> Line<'static> {
    Line::from(Span::styled(format!(" {text}"), dim()))
}

fn option_line(text: String, selected: bool) -> Line<'static> {
    let marker = if selected { "❯" } else { " " };
    let line = Line::from(format!(" {marker} {text}"));
    if selected {
        line.style(Style::new().bg(Color::DarkGray).bold())
    } else {
        line
    }
}

fn field_line(label: &str, input: &Input, focused: bool) -> Line<'static> {
    let style = if focused {
        Style::new().light_blue().bold()
    } else {
        dim()
    };
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(format!("{label:<10}"), Style::new().bold()),
        Span::raw("  "),
    ];
    spans.extend(input.spans(focused, style));
    if input.buf.is_empty() && focused {
        spans.push(Span::styled("  (type here)", dim()));
    }
    Line::from(spans)
}

fn spinner_frame(st: &WizardState) -> String {
    SPINNER[st.spin % SPINNER.len()].to_string()
}

/// 每步的画面:title 框放说明文字,body 框放选项/输入,hint 垫在 body 底部。
#[derive(Clone)]
struct Screen {
    title: String,
    text: Vec<Line<'static>>,
    body: Vec<Line<'static>>,
    hint: Vec<Line<'static>>,
    alert: bool,
}

fn choices(st: &WizardState, opts: &[&str]) -> Vec<Line<'static>> {
    opts.iter()
        .enumerate()
        .map(|(i, o)| option_line(o.to_string(), i == st.choice))
        .collect()
}

fn step_screen(st: &WizardState) -> Screen {
    let name = st.device.name();
    let quit: Line<'static> = hint_line("Esc/q quit");

    // 后台任务进行中:进度屏(优先于一切步骤内容)。
    if let Some(label) = &st.busy {
        let frame = spinner_frame(st);
        let installing = matches!(
            st.job_kind,
            Some(JobKind::InstallVibekeys) | Some(JobKind::InstallVibetty)
        );
        let body = match st.progress {
            Some((d, Some(t))) if installing => {
                format!(" {frame} {label}  {}/{} MB", d / 1048576, t / 1048576)
            }
            _ => format!(" {frame} {label}…"),
        };
        return Screen {
            title: "wizard".into(),
            text: vec![text_line(&body)],
            body: vec![],
            hint: vec![hint_line("working…")],
            alert: false,
        };
    }

    if let Some(err) = &st.err {
        return Screen {
            title: "wizard".into(),
            text: vec![Line::from(Span::styled(
                format!(" {err} "),
                err_style().bold(),
            ))],
            body: vec![],
            hint: vec![quit],
            alert: true,
        };
    }

    let mut s = Screen {
        title: String::new(),
        text: vec![],
        body: vec![],
        hint: vec![quit],
        alert: false,
    };

    match st.step {
        Step::IsKeyboard => {
            s.title = "vibeup wizard".into();
            s.text = vec![
                text_line("Welcome! This wizard installs the vibe tools"),
                text_line("and configures your device over Bluetooth."),
            ];
            s.body = choices(st, &["Yes — set up my VibeKeys keyboard", "No"]);
            s.hint = vec![hint_line("↑↓ choose · Enter confirm · Esc quit")];
        }
        Step::Scan if st.connecting.is_some() => {
            s.title = "wizard — connecting".into();
            s.text = vec![text_line(&format!(
                "{} connecting to {}…",
                spinner_frame(st),
                st.connecting.clone().unwrap_or_default()
            ))];
            s.hint = vec![hint_line("this can take a few seconds · Esc quit")];
        }
        Step::Scan => {
            s.title = format!("wizard — set up {name}");
            if st.pick.is_empty() {
                s.text = vec![
                    text_line(&format!("{} scanning for {}…", spinner_frame(st), name)),
                    text_line(""),
                    match st.device {
                        DeviceKind::VibeKeys => dim_text(
                            "not found? press ACCEPT on the keyboard, then wait",
                        ),
                        _ => dim_text(
                            "not found? put the device in provisioning mode, then wait",
                        ),
                    },
                ];
            } else {
                s.text = vec![text_line("found:")];
                s.body = st
                    .pick
                    .iter()
                    .enumerate()
                    .map(|(i, dev)| option_line(dev.name.clone(), i == st.pick_sel))
                    .collect();
                s.hint = vec![hint_line("↑↓ choose · Enter connect · Esc quit")];
            }
        }
        Step::InputMethod => {
            s.title = "wizard — input method".into();
            s.text = vec![text_line("How do you type with the keyboard?")];
            s.body = choices(
                st,
                &[
                    "Local input (typeless-like) — uses your IME",
                    "Whisper-compatible API — keyboard does ASR",
                ],
            );
            s.hint = vec![hint_line("↑↓ choose · Enter confirm · Esc quit")];
        }
        Step::HotkeyCapture => {
            s.title = "wizard — input method hotkey".into();
            s.text = vec![text_line("What is your IME hotkey?")];
            let captured = st
                .captured
                .clone()
                .unwrap_or_else(|| "…press a key combo".into());
            s.body = vec![Line::from(Span::styled(
                format!("  {captured}  "),
                Style::new().light_blue().bold(),
            ))];
            s.hint = vec![if st.captured.is_some() {
                hint_line("Enter confirm · any other key re-capture · Esc quit")
            } else {
                hint_line("e.g. Ctrl+Space — press it now · Esc quit")
            }];
        }
        Step::AsrApi => {
            s.title = "wizard — whisper API".into();
            s.text = vec![text_line("Enter your whisper-compatible ASR endpoint.")];
            s.body = vec![
                field_line("url", &st.input, !st.field2),
                field_line("api-key", &st.input2, st.field2),
            ];
            s.hint = vec![hint_line("Tab switch field · Enter save · Esc quit")];
        }
        Step::VibekeysApp => {
            s.title = "wizard — agent monitoring".into();
            s.text = vec![
                text_line("Install vibekeys-app?"),
                dim_text("It monitors Claude Code / Codex work status on the keyboard."),
            ];
            s.body = choices(
                st,
                &["Yes — install vibekeys + agent plugin", "Skip"],
            );
            s.hint = vec![hint_line("↑↓ choose · Enter confirm · Esc quit")];
        }
        Step::PluginClaude | Step::PluginCodex => {
            let (cli, action, label) = if st.step == Step::PluginClaude {
                ("claude", "install", "Claude Code")
            } else {
                ("codex", "add", "Codex")
            };
            s.title = format!("wizard — {label} plugin");
            s.text = vec![
                text_line(&format!("Install the {label} plugin?")),
                dim_text("It shows agent work status on the keyboard."),
            ];
            s.body = vec![
                dim_text(&format!(
                    "  $ {cli} plugin marketplace add second-state/marketplace"
                )),
                dim_text(&format!(
                    "  $ {cli} plugin {action} vibekeys@second-state-tools"
                )),
                Line::from(""),
            ];
            s.body.extend(choices(st, &["Yes — run the commands", "Skip"]));
            s.hint = vec![hint_line("↑↓ choose · Enter confirm · Esc quit")];
        }
        Step::InstallVibettyAsk => {
            s.title = "wizard — vibetty".into();
            s.text = vec![text_line("Install vibetty (remote control for your agent)?")];
            let skip = if st.device == DeviceKind::VibeKeys {
                "Skip — no remote control"
            } else {
                "Skip (continue anyway)"
            };
            s.body = choices(st, &["Yes — install vibetty", skip]);
            s.hint = vec![hint_line("↑↓ choose · Enter confirm · Esc quit")];
        }
        Step::RemoteOutside => {
            s.title = "wizard — remote mode".into();
            s.text = vec![text_line("Will you use remote mode outside the LAN?")];
            s.body = choices(st, &["Yes — outside LAN", "No — LAN only"]);
            s.hint = vec![hint_line("↑↓ choose · Enter confirm · Esc quit")];
        }
        Step::HasTailscale => {
            s.title = "wizard — tailscale".into();
            s.text = vec![text_line("Do you have a Tailscale account?")];
            s.body = choices(st, &["Yes", "No — help me choose"]);
            s.hint = vec![hint_line("↑↓ choose · Enter confirm · Esc quit")];
        }
        Step::BrokerChoice => {
            s.title = "wizard — broker".into();
            s.text = vec![
                text_line("Pick a way to reach your machine from outside:"),
                dim_text("EMQX Cloud is free up to 1 GB/month."),
            ];
            s.body = choices(
                st,
                &[
                    "Sign up for Tailscale (free, private network)",
                    "Use EMQX Cloud",
                    "I have my own broker (self-hosted)",
                ],
            );
            s.hint = vec![hint_line("↑↓ choose · Enter confirm · Esc quit")];
        }
        Step::TailscaleKey => {
            s.title = "wizard — tailscale auth key".into();
            // URL 行保留高亮;其余来自 wizard-texts.toml 的 [tailscale_key]。
            s.text = vec![
                text_line("1. In the Tailscale console go to Settings → Keys:"),
                Line::from(Span::styled(
                    "   https://login.tailscale.com/admin/settings/keys".to_string(),
                    Style::new().light_blue(),
                )),
                text_line("2. Click \"Generate auth key…\" to create a one-off key"),
                text_line("   (tskey-auth-…). One-off is enough — the device"),
                text_line("   caches its own node keys."),
                text_line("3. Paste the key below:"),
            ];
            s.body = vec![field_line("auth-key", &st.input, true)];
            s.hint = vec![hint_line("Enter send to device · Esc quit")];
        }
        Step::TailscaleIp => {
            // 只有查不到 ip 才会停留在这个屏(查到直接落 broker 去总结)。
            s.title = "wizard — tailscale on this machine".into();
            s.text = vec![
                text_line("No tailscale IP found on this machine."),
                text_line("Install tailscale and run `tailscale up`, then:"),
            ];
            s.body = vec![];
            s.hint = vec![
                hint_line("Enter/r retry · e enter broker manually · Esc quit"),
            ];
        }
        Step::BrokerInput => {
            s.title = "wizard — broker address".into();
            s.text = if st.choice == 1 {
                vec![
                    text_line("Sign up at https://www.emqx.com/en/cloud"),
                    dim_text("(free 1 GB/month), then paste the broker URL:"),
                ]
            } else {
                vec![text_line("Enter your broker's address:")]
            };
            s.body = vec![field_line("broker", &st.input, true)];
            s.hint = vec![hint_line("Enter confirm · Esc quit")];
        }
        Step::Done => {
            s.title = "wizard — done".into();
            if st.log.is_empty() {
                s.text = vec![dim_text("nothing configured yet.")];
            } else {
                s.text = st
                    .log
                    .iter()
                    .map(|l| {
                        Line::from(Span::styled(
                            format!("✓ {l}"),
                            Style::new().green(),
                        ))
                    })
                    .collect();
            }
            s.body = choices(st, &["Continue"]);
            s.hint = vec![hint_line("Enter continue")];
        }
        Step::WatchAsk => {
            s.title = "wizard — watch".into();
            s.text = vec![text_line(
                "Keyboard setup is complete. Configure the VibeWatch next?",
            )];
            s.body = choices(st, &["Yes — configure the VibeWatch", "No — finish"]);
            s.hint = vec![hint_line("↑↓ choose · Enter confirm · Esc quit")];
        }
    }
    s
}

/// 自绘画面:说明框在上,选项/输入框在下(中间空一行),整体居中。
/// 不走 kit 的 Center/Border children——它们的叠画/均分布局对多行内容
/// 表现不确定,自绘完全可控。
#[derive(Clone)]
struct WizardScreen {
    screen: Screen,
}

fn line_width(l: &Line<'_>) -> usize {
    l.spans.iter().map(|s| s.content.chars().count()).sum()
}

impl ratatui_kit::ratatui::widgets::Widget for WizardScreen {
    fn render(self, area: Rect, buf: &mut ratatui_kit::ratatui::buffer::Buffer) {
        (&self).render(area, buf)
    }
}

impl ratatui_kit::ratatui::widgets::Widget for &WizardScreen {
    fn render(self, area: Rect, buf: &mut ratatui_kit::ratatui::buffer::Buffer) {
        let scr = &self.screen;
        let has_body = !scr.body.is_empty();
        // 说明框固定尺寸:宽 56、内容 4 行,不随步骤文字多少跳变;
        // 内容真超长(总结页日志)才撑大。操作框同宽,高度随内容。
        const W_TEXT: u16 = 56;
        const H_TEXT_INNER: u16 = 4;
        let content_w = scr
            .text
            .iter()
            .chain(scr.body.iter())
            .chain(scr.hint.iter())
            .map(line_width)
            .max()
            .unwrap_or(20) as u16
            + 4;
        let w = W_TEXT.max(content_w).clamp(40, 100).min(area.width.saturating_sub(2));
        let h_text = H_TEXT_INNER.max(scr.text.len() as u16) + 2;
        let h_body = if has_body {
            (scr.body.len() + scr.hint.len()) as u16 + 2
        } else {
            0
        };
        let gap = if has_body { 1u16 } else { 0 };
        let total = h_text + gap + h_body;
        if total > area.height {
            return; // 终端太矮:宁可空白也不画残框
        }
        let x = area.x + (area.width.saturating_sub(w)) / 2;
        let y = area.y + (area.height.saturating_sub(total)) / 2;

        // 说明框(title 框)。
        let rect1 = Rect { x, y, width: w, height: h_text };
        draw_box(
            rect1,
            &format!(" {} ", scr.title),
            &scr.text,
            scr.alert,
            buf,
        );

        // 操作框(选项/输入 + hint)。
        if has_body {
            let mut lines = scr.body.clone();
            lines.extend(scr.hint.iter().cloned());
            let rect2 = Rect {
                x,
                y: y + h_text + gap,
                width: w,
                height: h_body,
            };
            draw_box(rect2, "", &lines, false, buf);
        }
    }
}

fn draw_box(
    rect: Rect,
    title: &str,
    lines: &[Line<'_>],
    alert: bool,
    buf: &mut ratatui_kit::ratatui::buffer::Buffer,
) {
    let border_style = if alert { err_style() } else { Style::new() };
    let mut block = ratatui_kit::ratatui::widgets::Block::bordered()
        .border_style(border_style);
    if !title.is_empty() {
        block = block.title(Span::styled(
            title.to_string(),
            border_style.add_modifier(Modifier::BOLD),
        ));
    }
    let inner = block.inner(rect);
    Clear.render(rect, buf);
    block.render(rect, buf);
    for (i, line) in lines.iter().enumerate() {
        if i as u16 >= inner.height {
            break;
        }
        let row = Rect {
            x: inner.x,
            y: inner.y + i as u16,
            width: inner.width,
            height: 1,
        };
        Paragraph::new(line.clone()).render(row, buf);
    }
}

// ===== 根组件 =====

#[derive(Props, Default)]
pub struct WizardAppProps {
    pub dir: PathBuf,
    pub dry_run: bool,
    pub demo: bool,
}

#[component]
pub fn WizardApp(props: &WizardAppProps, mut hooks: Hooks) -> impl Into<AnyElement<'static>> {
    let state = hooks.use_state(|| WizardState::new(props.demo, props.dry_run));
    let rt: State<Arc<Mutex<WizardRuntime>>> =
        hooks.use_state(|| Arc::new(Mutex::new(WizardRuntime::default())));
    let mut exit = hooks.use_exit();

    // 后台轮询:100ms 一拍,收任务/会话事件、推进进度(锁只在同步段持有)。
    {
        let dir = props.dir.clone();
        hooks.use_future(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let rt_arc = rt.read().clone();
                let mut rtg = rt_arc.lock().unwrap();
                let mut st = state.write();
                poll(&mut st, &mut rtg, &dir);
            }
        });
    }

    // 按键分发(单一 handler,全部吞掉)。
    {
        let dir = props.dir.clone();
        hooks.use_event_handler(EventScope::Current, EventPriority::Normal, move |event| {
            let Event::Key(key) = event else {
                return EventResult::Consumed;
            };
            let rt_arc = rt.read().clone();
            let mut rtg = rt_arc.lock().unwrap();
            let mut st = state.write();
            let outcome = wizard_key(&mut st, &mut rtg, &key, &dir);
            if outcome.exit {
                exit();
            }
            EventResult::Consumed
        });
    }

    let st = state.read().clone();
    let screen = WizardScreen { screen: step_screen(&st) };
    let el: AnyElement<'static> = element!(Center { widget(screen) }).into();
    el
}
