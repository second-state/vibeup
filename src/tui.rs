//! `vibeup tui` —— 三个标签:**键盘** / **手表**(BLE 设备配置)+ **管理**
//! (vibetty / vibekeys 的安装更新)。
//!
//! Tab 或 1/2/3 切标签。设备页按功能分节(server / wifi / asr / …),每节只有一条
//! 上边框带节名;每行是「动作 + 当前值」,选中后回车:可编辑项原地变成编辑框
//! (Enter 提交、Esc 取消),立即执行的项(如 reset)直接跑。
//! BLE 和安装动作都是同步阻塞的,执行时先退出 TUI 模式让输出打到正常屏幕,
//! 结束后再回来(见 `suspend`);设备配置复用 `config_cmd` 的函数,demo 模式
//! 改内存假数据。

use crate::config_cmd::{self, WifiAction};
use crate::install;
use crate::pkg::{self, PKGS, Pkg};
use anyhow::{Result, bail};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row as TRow, Table};
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

/// 可编辑字段:决定回车提交时调用哪个动作。
#[derive(Clone, Copy, PartialEq)]
enum Field {
    ServerUrl,
    WifiAdd,
    WifiRemove,
    AsrUri,
    AsrApiKey,
    AsrModel,
    MicModel,
    PreferAsr,
    Keymap,
    Tailscale,
}

/// 编辑框的占位提示(底部消息行)。
fn field_hint(field: Field) -> &'static str {
    match field {
        Field::ServerUrl => "mqtt://user:pass@host:1883",
        Field::WifiAdd => "ssid password (password 可含空格,空 = 开放网络)",
        Field::WifiRemove => "ssid",
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
        Field::WifiAdd => {
            let Some((ssid, pass)) = val.split_once(' ') else {
                bail!("需要 `ssid password`(空格分隔;密码可含空格则只写 ssid)");
            };
            if demo {
                mock_wifi_add(mock, ssid, pass);
                println!("wifi `{ssid}' saved (demo)");
                return Ok(());
            }
            config_cmd::wifi(
                kind,
                WifiAction::Add {
                    ssid: ssid.to_string(),
                    pass: pass.to_string(),
                },
                false,
            )
        }
        Field::WifiRemove => {
            if demo {
                mock_wifi_remove(mock, val);
                println!("wifi `{val}' removed (demo)");
                return Ok(());
            }
            config_cmd::wifi(
                kind,
                WifiAction::Remove {
                    ssid: val.to_string(),
                },
                false,
            )
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

/// 每行:所属节 + 标签 + 行为(立即执行 / 行内编辑)+ 当前值。
enum DevRow {
    Run {
        section: &'static str,
        label: &'static str,
        run: Rc<dyn Fn() -> Result<()>>,
        value: Rc<dyn Fn(&serde_json::Value) -> String>,
    },
    Edit {
        section: &'static str,
        label: &'static str,
        field: Field,
        value: Rc<dyn Fn(&serde_json::Value) -> String>,
    },
}

impl DevRow {
    fn section(&self) -> &'static str {
        match self {
            Self::Run { section, .. } | Self::Edit { section, .. } => section,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Run { label, .. } | Self::Edit { label, .. } => label,
        }
    }

    fn value(&self, cfg: &serde_json::Value) -> String {
        match self {
            Self::Run { value, .. } | Self::Edit { value, .. } => value(cfg),
        }
    }
}

fn device_rows(kind: crate::ble::DeviceKind, demo: bool, mock: Mock) -> Vec<DevRow> {
    use crate::ble::DeviceKind;
    let keys = kind == DeviceKind::VibeKeys;
    let mut v: Vec<DevRow> = Vec::new();
    let origin = mock;

    v.push(DevRow::Edit {
        section: "server",
        label: "server-url",
        field: Field::ServerUrl,
        value: Rc::new(|c| cfg_get(c, &["server_url"])),
    });

    v.push(DevRow::Edit {
        section: "wifi",
        label: "add (ssid pass)",
        field: Field::WifiAdd,
        value: Rc::new(|_| String::new()),
    });
    v.push(DevRow::Edit {
        section: "wifi",
        label: "remove (ssid)",
        field: Field::WifiRemove,
        value: Rc::new(|_| String::new()),
    });
    let mock = origin.clone();
    v.push(DevRow::Run {
        section: "wifi",
        label: "show list",
        run: Rc::new(move || {
            if demo {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&mock.borrow()["wifi_list"])?
                );
                return Ok(());
            }
            config_cmd::wifi(kind, WifiAction::Show, false)
        }),
        value: Rc::new(|c| match c.get("wifi_list").and_then(|v| v.as_array()) {
            Some(arr) => format!(
                "{} network(s): {}",
                arr.len(),
                arr.iter()
                    .filter_map(|e| e.get("ssid").and_then(|v| v.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            None => String::new(),
        }),
    });

    v.push(DevRow::Edit {
        section: "asr",
        label: "uri",
        field: Field::AsrUri,
        value: Rc::new(|c| asr_cfg(c, "uri")),
    });
    v.push(DevRow::Edit {
        section: "asr",
        label: "api-key",
        field: Field::AsrApiKey,
        value: Rc::new(|c| asr_cfg(c, "api_key")),
    });
    v.push(DevRow::Edit {
        section: "asr",
        label: "model",
        field: Field::AsrModel,
        value: Rc::new(|c| asr_cfg(c, "model")),
    });
    let mock = origin.clone();
    v.push(DevRow::Run {
        section: "asr",
        label: "clear",
        run: Rc::new(move || {
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
        }),
        value: Rc::new(|_| String::new()),
    });

    if keys {
        v.push(DevRow::Edit {
            section: "keyboard",
            label: "mic-model",
            field: Field::MicModel,
            value: Rc::new(|c| match c.get("mic_model").and_then(|v| v.as_u64()) {
                Some(0) => "ptt".into(),
                Some(1) => "toggle".into(),
                _ => String::new(),
            }),
        });
        v.push(DevRow::Edit {
            section: "keyboard",
            label: "prefer-asr",
            field: Field::PreferAsr,
            value: Rc::new(
                |c| match c.get("prefer_builtin_asr").and_then(|v| v.as_bool()) {
                    Some(true) => "on".into(),
                    Some(false) => "off".into(),
                    _ => String::new(),
                },
            ),
        });
        v.push(DevRow::Edit {
            section: "keyboard",
            label: "keymap (KEY=BINDING)",
            field: Field::Keymap,
            value: Rc::new(|c| match c.get("keymap").and_then(|v| v.as_object()) {
                Some(m) if !m.is_empty() => {
                    let pairs: Vec<String> =
                        m.iter().map(|(k, val)| format!("{k}={val}")).collect();
                    pairs.join("  ")
                }
                _ => String::new(),
            }),
        });
    }

    v.push(DevRow::Edit {
        section: "device",
        label: "tailscale: bind",
        field: Field::Tailscale,
        value: Rc::new(|c| {
            // 真机快照不含 tailscale_key(仅写);demo 里改过才显示。
            let v = cfg_get(c, &["tailscale_key"]);
            if v.is_empty() { v } else { "(set)".into() }
        }),
    });
    v.push(DevRow::Run {
        section: "device",
        label: "reset device",
        run: Rc::new(move || {
            if demo {
                println!("device rebooting (demo)");
                return Ok(());
            }
            config_cmd::reset(kind)
        }),
        value: Rc::new(|_| String::new()),
    });

    v
}

struct DeviceTab {
    kind: crate::ble::DeviceKind,
    config: Option<serde_json::Value>,
    rows: Vec<DevRow>,
    sel: usize,
    /// demo 模式:不碰 BLE,动作改内存里的假配置。
    demo: bool,
    mock: Mock,
    /// 行内编辑:选中行下标 + 当前输入。None = 浏览模式。
    editing: Option<(usize, String)>,
}

impl DeviceTab {
    fn new(kind: crate::ble::DeviceKind, demo: bool) -> Self {
        let mock = Rc::new(std::cell::RefCell::new(sample_config(kind)));
        let rows = device_rows(kind, demo, mock.clone());
        Self {
            kind,
            config: None,
            rows,
            sel: 0,
            demo,
            mock,
            editing: None,
        }
    }

    fn load_config(&mut self, terminal: &mut DefaultTerminal) {
        if self.demo {
            self.config = Some(self.mock.borrow().clone());
            return;
        }
        let kind = self.kind;
        let res = suspend(terminal, || {
            crate::ble::with_device(kind, |p| async move { crate::ble::read_config(&p).await })
        });
        match res {
            Ok(cfg) => self.config = Some(cfg),
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
        if key.kind != KeyEventKind::Press {
            continue;
        }

        // 设备页的行内编辑独占按键。
        if tab != Tab::Pkgs && keys_tab.editing.is_some() || watch_tab.editing.is_some() {
            let t = if keys_tab.editing.is_some() {
                &mut keys_tab
            } else {
                &mut watch_tab
            };
            let Some((_, buf)) = t.editing.as_mut() else {
                unreachable!()
            };
            match key.code {
                KeyCode::Esc => {
                    t.editing = None;
                    message = "edit cancelled".into();
                }
                KeyCode::Enter => {
                    let (idx, val) = t.editing.take().unwrap();
                    let (label, field) = match &t.rows[idx] {
                        DevRow::Edit { label, field, .. } => (*label, *field),
                        DevRow::Run { .. } => unreachable!(),
                    };
                    let (mock, kind) = (t.mock.clone(), t.kind);
                    let res = suspend(&mut terminal, || {
                        apply_field(kind, demo, &mock, field, &val)
                    });
                    message = format!("{label}: {}", describe(&res, dry_run));
                    if res.is_ok() && !dry_run {
                        t.load_config(&mut terminal);
                    }
                }
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) => buf.push(c),
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
                                editing: None,
                                message: &message,
                                demo,
                            },
                        )
                    })?;
                    fetch(&mut pkg_entries, &dir);
                    message = "refreshed".into();
                }
            },
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
                    match &t.rows[t.sel] {
                        DevRow::Run { label, run, .. } => {
                            let (label, run) = (*label, run.clone());
                            let res = suspend(&mut terminal, || run());
                            message = format!("{label}: {}", describe(&res, dry_run));
                            if res.is_ok() && !dry_run {
                                t.load_config(&mut terminal);
                            }
                        }
                        DevRow::Edit { .. } => {
                            t.editing = Some((t.sel, String::new()));
                            message = "editing — Enter save · Esc cancel".into();
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
    editing: Option<&'a (usize, String)>,
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
            Some(s) if s.name == row.section() => s.len += 1,
            _ => out.push(Section {
                name: row.section(),
                start: i,
                len: 1,
            }),
        }
    }
    out
}

fn render_device_tab(f: &mut Frame, area: Rect, t: &DeviceTab, editing: Option<&(usize, String)>) {
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
                Span::styled(row.label(), Style::default().add_modifier(Modifier::BOLD)),
            ];

            if editing_this {
                // 原地编辑框:值的位置变成输入缓冲 + 光标。
                let buf = editing.map(|(_, b)| b.clone()).unwrap_or_default();
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    format!("{}▏", buf),
                    Style::default()
                        .bg(Color::LightBlue)
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(
                    format!("  ({})", field_hint(row_field(row))),
                    Style::default().fg(Color::DarkGray),
                ));
            } else {
                let val = t.config.as_ref().map(|c| row.value(c)).unwrap_or_default();
                if !val.is_empty() {
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled(val, Style::default().fg(Color::Gray)));
                }
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
}

fn row_field(row: &DevRow) -> Field {
    match row {
        DevRow::Edit { field, .. } => *field,
        DevRow::Run { .. } => unreachable!("edit box on a Run row"),
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
        let rows = device_rows(DeviceKind::VibeKeys, true, mock());
        let secs = sections_of(&rows);
        let names: Vec<&str> = secs.iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["server", "wifi", "asr", "keyboard", "device"]);
        assert_eq!(secs.iter().map(|s| s.len).sum::<usize>(), rows.len());
    }

    #[test]
    fn apply_field_demo_updates_mock() {
        let m = mock();
        apply_field(DeviceKind::VibeKeys, true, &m, Field::ServerUrl, "mqtt://x").unwrap();
        assert_eq!(m.borrow()["server_url"], "mqtt://x");
        apply_field(DeviceKind::VibeKeys, true, &m, Field::MicModel, "ptt").unwrap();
        assert_eq!(m.borrow()["mic_model"], 0);
        apply_field(DeviceKind::VibeKeys, true, &m, Field::WifiAdd, "Cafe pw 1").unwrap();
        let list = m.borrow()["wifi_list"].clone();
        assert_eq!(list.as_array().unwrap().len(), 3);
        assert_eq!(list.as_array().unwrap()[2]["pass"], "pw 1");
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
