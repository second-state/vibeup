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

fn device_actions(kind: crate::ble::DeviceKind) -> Vec<Action> {
    use crate::ble::DeviceKind;
    let keys = kind == DeviceKind::VibeKeys;
    let mut v = vec![
        Action::imm("wifi: show list", move |_| {
            config_cmd::wifi(kind, WifiAction::Show, false)
        }),
        Action::ask(
            "wifi: add / update",
            &["SSID:", "password (empty = open):"],
            move |a| {
                config_cmd::wifi(
                    kind,
                    WifiAction::Add {
                        ssid: a[0].clone(),
                        pass: a[1].clone(),
                    },
                    false,
                )
            },
        ),
        Action::ask("wifi: remove", &["SSID:"], move |a| {
            config_cmd::wifi(kind, WifiAction::Remove { ssid: a[0].clone() }, false)
        }),
        Action::ask("server-url", &["MQTT broker URL:"], move |a| {
            config_cmd::server_url(kind, a[0].clone(), false)
        }),
        Action::ask(
            "asr: set",
            &[
                "uri (empty = keep):",
                "api-key (empty = keep):",
                "model (empty = keep):",
            ],
            move |a| {
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
        ),
        Action::imm("asr: clear", move |_| {
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
        Action::imm("reset device", move |_| config_cmd::reset(kind)),
    ];
    if keys {
        v.push(Action::ask("mic-model", &["ptt | toggle:"], move |a| {
            config_cmd::mic_model(kind, a[0].clone(), false)
        }));
        v.push(Action::ask("prefer-asr", &["on | off:"], move |a| {
            let on = match a[0].to_lowercase().as_str() {
                "on" | "true" => true,
                "off" | "false" => false,
                other => anyhow::bail!("invalid value `{other}` — use on or off"),
            };
            config_cmd::prefer_asr(kind, on, false)
        }));
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
            move |a| config_cmd::keymap(kind, a[0].clone(), a[1].clone()),
        ));
    }
    v.push(Action::ask(
        "tailscale: bind",
        &["tailscale auth key:"],
        move |a| config_cmd::tailscale(kind, a[0].clone()),
    ));
    v
}

struct DeviceTab {
    kind: crate::ble::DeviceKind,
    config: Option<serde_json::Value>,
    actions: Vec<Action>,
    sel: usize,
}

impl DeviceTab {
    fn new(kind: crate::ble::DeviceKind) -> Self {
        Self {
            kind,
            config: None,
            actions: device_actions(kind),
            sel: 0,
        }
    }

    fn load_config(&mut self, terminal: &mut DefaultTerminal) {
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

pub fn run(dir: Option<PathBuf>, dry_run: bool) -> Result<()> {
    let dir = pkg::install_dir(dir.as_deref());
    let mut pkg_entries: Vec<Entry> = PKGS.iter().map(Entry::new).collect();
    let mut pkg_sel = 0usize;
    let mut allow_pre = false;
    let mut keys_tab = DeviceTab::new(crate::ble::DeviceKind::VibeKeys);
    let mut watch_tab = DeviceTab::new(crate::ble::DeviceKind::Watch);
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
            }
            KeyCode::Char('1') | KeyCode::Char('2') | KeyCode::Char('3') => {
                let i = match key.code {
                    KeyCode::Char('1') => 0,
                    KeyCode::Char('2') => 1,
                    _ => 2,
                };
                tab = Tab::all()[i];
                message = default_message(tab);
            }
            KeyCode::Char('j') | KeyCode::Down => match tab {
                Tab::Keys => keys_tab.sel = (keys_tab.sel + 1) % keys_tab.actions.len(),
                Tab::Watch => watch_tab.sel = (watch_tab.sel + 1) % watch_tab.actions.len(),
                Tab::Pkgs => pkg_sel = (pkg_sel + 1) % pkg_entries.len(),
            },
            KeyCode::Char('k') | KeyCode::Up => match tab {
                Tab::Keys => {
                    keys_tab.sel =
                        (keys_tab.sel + keys_tab.actions.len() - 1) % keys_tab.actions.len()
                }
                Tab::Watch => {
                    watch_tab.sel =
                        (watch_tab.sel + watch_tab.actions.len() - 1) % watch_tab.actions.len()
                }
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
                                input: input.as_ref(),
                                message: &message,
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
        " vibeup {} · prerelease: {} ",
        env!("CARGO_PKG_VERSION"),
        on_off(allow_pre)
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
    // 摘要块高度随行数走:写死会在没读到配置(1 行)时留一大段空白。
    let summary: Vec<Line> = t.summary().into_iter().map(Line::from).collect();
    let [head, list] =
        Layout::vertical([Constraint::Length(summary.len() as u16), Constraint::Min(0)])
            .areas(area);
    f.render_widget(Paragraph::new(summary), head);

    let rows: Vec<TRow> = t
        .actions
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let style = if i == t.sel {
                Style::default().bg(Color::DarkGray)
            } else {
                Style::default()
            };
            TRow::new([Cell::from(a.label)]).style(style)
        })
        .collect();
    let table = Table::new(rows, [Constraint::Percentage(100)]);
    f.render_widget(table, list);
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
