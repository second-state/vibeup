//! `vibeup tui` —— 三个标签:**键盘** / **手表**(BLE 设备配置)+ **管理**
//! (vibetty / vibekeys 的安装更新)。
//!
//! Tab 或 1/2/3 切标签;设备页是「CONFIG 摘要 + 动作菜单」,需要参数的动作进入
//! 逐项输入(底部一行,Enter 提交、Esc 取消)。BLE 和安装动作都是同步阻塞的,
//! 执行时先退出 TUI 模式让输出打到正常屏幕,结束后再回来(见 `suspend`);
//! 设备配置直接复用 `config_cmd` 的函数,每条动作自成一次 BLE 会话。

use crate::config_cmd::{self, WifiAction};
use crate::install;
use crate::pkg::{self, PKGS, Pkg};
use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Cell, Paragraph, Row as TRow, Table};
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

/// 一个设备动作:label 显示;prompts 非空则先进逐项输入,收齐后交给 run。
/// 动作闭包类型:收齐的答案 → 执行(自己开一次 BLE 会话)。
type ActionFn = Box<dyn Fn(&[String]) -> Result<()>>;

struct Action {
    label: &'static str,
    prompts: Vec<&'static str>,
    run: ActionFn,
}

impl Action {
    fn imm(label: &'static str, run: impl Fn(&[String]) -> Result<()> + 'static) -> Self {
        Self {
            label,
            prompts: vec![],
            run: Box::new(run),
        }
    }

    fn ask(
        label: &'static str,
        prompts: &[&'static str],
        run: impl Fn(&[String]) -> Result<()> + 'static,
    ) -> Self {
        Self {
            label,
            prompts: prompts.to_vec(),
            run: Box::new(run),
        }
    }
}

fn device_actions(kind: crate::ble::DeviceKind, demo: bool, mock: Mock) -> Vec<Action> {
    use crate::ble::DeviceKind;
    let keys = kind == DeviceKind::VibeKeys;
    let mut v: Vec<Action> = Vec::new();
    let origin = mock;

    let mock = origin.clone();
    v.push(Action::imm("wifi: show list", move |_| {
        if demo {
            println!(
                "{}",
                serde_json::to_string_pretty(&mock.borrow()["wifi_list"])?
            );
            return Ok(());
        }
        config_cmd::wifi(kind, WifiAction::Show, false)
    }));

    let mock = origin.clone();
    v.push(Action::ask(
        "wifi: add / update",
        &["SSID:", "password (empty = open):"],
        move |a| {
            if demo {
                mock_wifi_add(&mock, &a[0], &a[1]);
                println!("wifi `{}' saved (demo)", a[0]);
                return Ok(());
            }
            config_cmd::wifi(
                kind,
                WifiAction::Add {
                    ssid: a[0].clone(),
                    pass: a[1].clone(),
                },
                false,
            )
        },
    ));

    let mock = origin.clone();
    v.push(Action::ask("wifi: remove", &["SSID:"], move |a| {
        if demo {
            mock_wifi_remove(&mock, &a[0]);
            println!("wifi `{}' removed (demo)", a[0]);
            return Ok(());
        }
        config_cmd::wifi(kind, WifiAction::Remove { ssid: a[0].clone() }, false)
    }));

    let mock = origin.clone();
    v.push(Action::ask("server-url", &["MQTT broker URL:"], move |a| {
        if demo {
            mock_patch(&mock, "server_url", a[0].clone().into());
            println!("server_url = {} (demo)", a[0]);
            return Ok(());
        }
        config_cmd::server_url(kind, a[0].clone(), false)
    }));

    let mock = origin.clone();
    v.push(Action::ask(
        "asr: set",
        &[
            "uri (empty = keep):",
            "api-key (empty = keep):",
            "model (empty = keep):",
        ],
        move |a| {
            if demo {
                let opt = |s: &str| (!s.is_empty()).then(|| s.to_string());
                mock_asr_set(
                    &mock,
                    opt(&a[0]).as_deref(),
                    opt(&a[1]).as_deref(),
                    opt(&a[2]).as_deref(),
                );
                println!("asr_config updated (demo)");
                return Ok(());
            }
            let opt = |s: &str| (!s.is_empty()).then(|| s.to_string());
            config_cmd::asr(
                kind,
                config_cmd::AsrOpts {
                    uri: opt(&a[0]),
                    api_key: opt(&a[1]),
                    model: opt(&a[2]),
                    clean: false,
                },
                false,
            )
        },
    ));

    let mock = origin.clone();
    v.push(Action::imm("asr: clear", move |_| {
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
    }));

    v.push(Action::imm("reset device", move |_| {
        if demo {
            println!("device rebooting (demo)");
            return Ok(());
        }
        config_cmd::reset(kind)
    }));

    if keys {
        let mock = origin.clone();
        v.push(Action::ask("mic-model", &["ptt | toggle:"], move |a| {
            if demo {
                let val = match a[0].to_lowercase().as_str() {
                    "ptt" => 0,
                    "toggle" => 1,
                    other => anyhow::bail!("invalid mic mode `{other}` — use ptt or toggle"),
                };
                mock_patch(&mock, "mic_model", serde_json::json!(val));
                println!("mic_model = {val} (demo)");
                return Ok(());
            }
            config_cmd::mic_model(kind, a[0].clone(), false)
        }));

        let mock = origin.clone();
        v.push(Action::ask("prefer-asr", &["on | off:"], move |a| {
            let on = match a[0].to_lowercase().as_str() {
                "on" | "true" => true,
                "off" | "false" => false,
                other => anyhow::bail!("invalid value `{other}` — use on or off"),
            };
            if demo {
                mock_patch(&mock, "prefer_builtin_asr", serde_json::json!(on));
                println!("prefer_builtin_asr = {on} (demo)");
                return Ok(());
            }
            config_cmd::prefer_asr(kind, on, false)
        }));

        let mock = origin.clone();
        v.push(Action::ask(
            "keymap",
            &[
                concat!(
                    "key (",
                    "MIC/CUSTOM/ESC/NEXT/BACKSPACE/SWITCH/ACCEPT/ROTATE",
                    "):"
                ),
                "binding (text: \"/compact\\n\"  combo: Ctrl+C):",
            ],
            move |a| {
                if demo {
                    mock_keymap(&mock, &a[0], &a[1]);
                    println!("keymap {} updated (demo)", a[0]);
                    return Ok(());
                }
                config_cmd::keymap(kind, a[0].clone(), a[1].clone())
            },
        ));
    }

    let mock = origin.clone();
    v.push(Action::ask(
        "tailscale: bind",
        &["tailscale auth key:"],
        move |a| {
            if demo {
                mock_patch(&mock, "tailscale_key", a[0].clone().into());
                println!("tailscale key bound (demo)");
                return Ok(());
            }
            config_cmd::tailscale(kind, a[0].clone())
        },
    ));

    v
}

/// demo 用的假设备状态(内存配置,动作直接改它)。
type Mock = std::rc::Rc<std::cell::RefCell<serde_json::Value>>;

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

struct DeviceTab {
    kind: crate::ble::DeviceKind,
    config: Option<serde_json::Value>,
    actions: Vec<Action>,
    sel: usize,
    /// 按钮网格的列数,渲染时算出、按键导航用(Cell 是因为 ui() 只拿得到 &Self)。
    grid_cols: std::cell::Cell<usize>,
    /// demo 模式:不碰 BLE,动作改内存里的假配置。
    demo: bool,
    mock: Mock,
}

impl DeviceTab {
    fn new(kind: crate::ble::DeviceKind, demo: bool) -> Self {
        let mock = std::rc::Rc::new(std::cell::RefCell::new(sample_config(kind)));
        let actions = device_actions(kind, demo, mock.clone());
        Self {
            kind,
            config: None,
            actions,
            sel: 0,
            grid_cols: std::cell::Cell::new(1),
            demo,
            mock,
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

    /// CONFIG 摘要行(显示在动作列表上方)。
    fn summary(&self) -> Vec<String> {
        let Some(c) = &self.config else {
            return vec!["(config not loaded — press r to read from device)".into()];
        };
        let gets = |k: &str| {
            c.get(k)
                .map(|v| match v {
                    serde_json::Value::Null => "not set".to_string(),
                    other => other.to_string(),
                })
                .unwrap_or_else(|| "not set".into())
        };
        let wifi = c
            .get("wifi_list")
            .and_then(|v| v.as_array())
            .map(|arr| {
                let names: Vec<&str> = arr
                    .iter()
                    .filter_map(|e| e.get("ssid").and_then(|v| v.as_str()))
                    .collect();
                format!("{} network(s): {}", names.len(), names.join(", "))
            })
            .unwrap_or_else(|| "not set".into());
        let mut lines = vec![
            format!("server_url: {}", gets("server_url")),
            format!("wifi_list: {wifi}"),
            format!(
                "asr: {}",
                c.get("asr_config")
                    .and_then(|v| v.get("uri"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("not set")
            ),
        ];
        if self.kind == crate::ble::DeviceKind::VibeKeys {
            let mic = match c.get("mic_model").and_then(|v| v.as_u64()) {
                Some(0) => "ptt",
                Some(1) => "toggle",
                _ => "not set",
            };
            let pref = c
                .get("prefer_builtin_asr")
                .and_then(|v| v.as_bool())
                .map(|b| if b { "on" } else { "off" })
                .unwrap_or("not set");
            lines.push(format!("mic_model: {mic}    prefer_builtin_asr: {pref}"));
        }
        lines
    }
}

// ===== 输入模式 =====

/// 逐项收集一个动作的参数。动作本体留在 `DeviceTab.actions` 里(生命周期更长),
/// 这里只记它的下标和已收集的答案。
struct InputState {
    /// 剩余要问的 prompt;buffer 是当前这条的输入。
    prompts: Vec<String>,
    answers: Vec<String>,
    buffer: String,
    action_idx: usize,
}

impl InputState {
    fn current_prompt(&self) -> Option<&str> {
        self.prompts.first().map(|s| s.as_str())
    }

    /// Enter:收下当前答案。返回 true = 全部收齐,可以执行了。
    fn submit(&mut self) -> bool {
        let answer = std::mem::take(&mut self.buffer);
        self.answers.push(answer);
        self.prompts.remove(0);
        self.prompts.is_empty()
    }
}

// ===== 主循环 =====

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
    let mut input: Option<InputState> = None;
    let mut message = String::from("Tab switch tabs · ↑↓ select · Enter run · r refresh · q quit");
    fetch(&mut pkg_entries, &dir);

    let mut terminal = ratatui::init();
    // 启动前排空 tty 里残留的旧按键(比如上一次会话漏下的 `q`),否则一进
    // 主循环就被当成快捷键直接退出。
    while event::poll(std::time::Duration::ZERO).unwrap_or(false) {
        let _ = event::read();
    }
    loop {
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
                    input: input.as_ref(),
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

        // 输入模式独占按键。
        if let Some(state) = input.as_mut() {
            match key.code {
                KeyCode::Esc => {
                    input = None;
                    message = "input cancelled".into();
                }
                KeyCode::Enter => {
                    // 收答案;收齐了就地执行(动作本体在 DeviceTab.actions 里)。
                    let mut done = None;
                    if let Some(state) = input.as_mut()
                        && state.submit()
                    {
                        done = Some((state.action_idx, state.answers.clone()));
                    }
                    if let Some((idx, answers)) = done {
                        input = None;
                        let t = match tab {
                            Tab::Keys => &mut keys_tab,
                            Tab::Watch => &mut watch_tab,
                            Tab::Pkgs => unreachable!("input only starts on device tabs"),
                        };
                        let action = &t.actions[idx];
                        let label = action.label;
                        let run = &action.run;
                        let res = suspend(&mut terminal, || run(&answers));
                        message = format!("{label}: {}", describe(&res, dry_run));
                        if res.is_ok() && !dry_run {
                            t.load_config(&mut terminal);
                        }
                    }
                }
                KeyCode::Backspace => {
                    state.buffer.pop();
                }
                KeyCode::Char(c) => state.buffer.push(c),
                _ => {}
            }
            continue;
        }

        match key.code {
            KeyCode::Char('q') => break,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                break;
            }
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
                // 按钮网格里 = 向下移动一行;包列表 = 下一行。
                Tab::Keys | Tab::Watch => {
                    let t = if tab == Tab::Keys {
                        &mut keys_tab
                    } else {
                        &mut watch_tab
                    };
                    let cols = t.grid_cols.get();
                    t.sel = (t.sel + cols).min(t.actions.len() - 1);
                }
                Tab::Pkgs => pkg_sel = (pkg_sel + 1) % pkg_entries.len(),
            },
            KeyCode::Char('k') | KeyCode::Up => match tab {
                Tab::Keys | Tab::Watch => {
                    let t = if tab == Tab::Keys {
                        &mut keys_tab
                    } else {
                        &mut watch_tab
                    };
                    t.sel = t.sel.saturating_sub(t.grid_cols.get());
                }
                Tab::Pkgs => pkg_sel = (pkg_sel + pkg_entries.len() - 1) % pkg_entries.len(),
            },
            KeyCode::Char('l') | KeyCode::Right => match tab {
                Tab::Keys | Tab::Watch => {
                    let t = if tab == Tab::Keys {
                        &mut keys_tab
                    } else {
                        &mut watch_tab
                    };
                    t.sel = (t.sel + 1).min(t.actions.len() - 1);
                }
                Tab::Pkgs => {}
            },
            KeyCode::Char('h') | KeyCode::Left => match tab {
                Tab::Keys | Tab::Watch => {
                    let t = if tab == Tab::Keys {
                        &mut keys_tab
                    } else {
                        &mut watch_tab
                    };
                    t.sel = t.sel.saturating_sub(1);
                }
                Tab::Pkgs => {}
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
                                input: input.as_ref(),
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
                    let t = match tab {
                        Tab::Keys => &mut keys_tab,
                        _ => &mut watch_tab,
                    };
                    if t.config.is_none() {
                        t.load_config(&mut terminal);
                    }
                    let action = &t.actions[t.sel];
                    if action.prompts.is_empty() {
                        let run = &action.run;
                        let res = suspend(&mut terminal, || run(&[]));
                        message = format!("{}: {}", action.label, describe(&res, dry_run));
                        if res.is_ok() {
                            let kind = t.kind;
                            t.config = suspend(&mut terminal, || {
                                crate::ble::with_device(kind, |p| async move {
                                    crate::ble::read_config(&p).await
                                })
                            })
                            .ok();
                        }
                    } else {
                        input = Some(InputState {
                            prompts: action.prompts.iter().map(|s| s.to_string()).collect(),
                            answers: vec![],
                            buffer: String::new(),
                            action_idx: t.sel,
                        });
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
        Tab::Keys => "vibekeys — Enter runs, r re-reads config".into(),
        Tab::Watch => "watch — Enter runs, r re-reads config".into(),
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

/// `ui` 的一揽子状态引用。
struct UiRefs<'a> {
    tab: Tab,
    keys: &'a DeviceTab,
    watch: &'a DeviceTab,
    pkgs: &'a [Entry],
    pkg_sel: usize,
    allow_pre: bool,
    input: Option<&'a InputState>,
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
        input,
        message,
        demo,
    } = r;
    let [tabs, area, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(if input.is_some() { 3 } else { 2 }),
    ])
    .areas(f.area());

    // 标签栏。
    let spans: Vec<ratatui::text::Span> = Tab::all()
        .iter()
        .map(|t| {
            let text = match t {
                Tab::Keys => format!(" 1 {} ", t.title()),
                Tab::Watch => format!(" 2 {} ", t.title()),
                Tab::Pkgs => format!(" 3 {} ", t.title()),
            };
            let style = if *t == tab {
                Style::default()
                    .bg(Color::LightBlue)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ratatui::text::Span::styled(text, style)
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
            render_device_tab(f, area, t, input);
        }
        Tab::Pkgs => render_packages_tab(f, area, pkg_entries, pkg_sel, allow_pre),
    }

    let title = format!(
        " vibeup {} · prerelease: {}{} ",
        env!("CARGO_PKG_VERSION"),
        on_off(allow_pre),
        if demo { " · DEMO" } else { "" }
    );
    let mut lines = vec![
        Line::from(title),
        Line::from(format!("{message}  ·  q quit · Tab tabs · r refresh")),
    ];
    if let Some(state) = input
        && let Some(prompt) = state.current_prompt()
    {
        lines.push(Line::from(format!(
            "{prompt} {}▏  (Enter submit · Esc cancel)",
            state.buffer
        )));
    }
    f.render_widget(Paragraph::new(lines), footer);
}

fn render_device_tab(f: &mut Frame, area: Rect, t: &DeviceTab, input: Option<&InputState>) {
    // 像 HTML 卡片一样的一整块:边框 + 标题,摘要在上,动作按钮流式换行填满宽度。
    let device_title = match t.kind {
        crate::ble::DeviceKind::VibeKeys => " VibeKeys ",
        crate::ble::DeviceKind::Watch => " VibeWatch ",
    };
    let block = ratatui::widgets::Block::bordered().title(device_title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    // 摘要:卡片顶部,高度随行数走。
    let summary: Vec<Line> = t.summary().into_iter().map(Line::from).collect();
    let [sum_area, grid_area] =
        Layout::vertical([Constraint::Length(summary.len() as u16), Constraint::Min(0)])
            .areas(inner);
    f.render_widget(Paragraph::new(summary), sum_area);

    // 按钮网格(flex-wrap):等宽格子,放不下的换行,填满卡片宽度。
    let cell_w = t
        .actions
        .iter()
        .map(|a| a.label.len() + 3) // 两侧各 1 格 padding + 1 格间隔
        .max()
        .unwrap_or(1) as u16;
    let cols = ((grid_area.width / cell_w) as usize).max(1);
    t.grid_cols.set(cols);
    for (i, a) in t.actions.iter().enumerate() {
        let (row, col) = ((i / cols) as u16, (i % cols) as u16);
        let x = grid_area.x + col * cell_w;
        let y = grid_area.y + row;
        if y >= grid_area.bottom() || x >= grid_area.right() {
            break; // 卡片高度不够放完就停(动作不会多到放不下)
        }
        let w = (cell_w - 1).min(grid_area.right() - x);
        let rect = Rect {
            x,
            y,
            width: w,
            height: 1,
        };
        let style = if i == t.sel {
            Style::default()
                .bg(Color::LightBlue)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().bg(Color::DarkGray)
        };
        f.render_widget(Paragraph::new(format!(" {} ", a.label)).style(style), rect);
    }
    let _ = input;
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
        std::rc::Rc::new(std::cell::RefCell::new(sample_config(DeviceKind::VibeKeys)))
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
}
