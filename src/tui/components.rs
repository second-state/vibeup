//! ratatui-kit 组件:纯渲染。所有状态在 `state.rs`(hook State + Arc<Mutex>
//! 后台句柄),按键逻辑在 `actions.rs`(App 根组件注册单一事件 handler,保持
//! 旧版的优先级链:退出 > pop 框 > 行内编辑 > 连接流程 > 全局键)。后台轮询
//! 在 App 的 `use_future`(100ms 一拍:收任务/会话事件、推进转圈、写进度)。

use super::actions;
use super::state::{Conn, DeviceState, PkgState, Tab};
use crate::ble::DeviceKind;
use ratatui_kit::prelude::*;
use ratatui_kit::ratatui::{
    layout::{Constraint, Direction},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Borders,
};
use std::path::PathBuf;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn dim() -> Style {
    Style::new().dark_gray()
}

fn err_style() -> Style {
    Style::new().red()
}

// ===== 根组件 =====

#[derive(Props, Default)]
pub struct AppProps {
    pub dir: PathBuf,
    pub dry_run: bool,
    pub demo: bool,
}

#[component]
pub fn App(props: &AppProps, mut hooks: Hooks) -> impl Into<AnyElement<'static>> {
    let tab = hooks.use_state(|| Tab::Packages);
    let keys = hooks.use_state(|| super::state::DeviceState::new(DeviceKind::VibeKeys, props.demo));
    let watch = hooks.use_state(|| super::state::DeviceState::new(DeviceKind::Watch, props.demo));
    let runtimes = hooks.use_state(|| {
        std::sync::Arc::new(std::sync::Mutex::new(super::state::Runtimes::new()))
    });
    let pkg = hooks.use_state(super::state::PkgState::default);
    let pkg_rt = hooks.use_state(|| {
        std::sync::Arc::new(std::sync::Mutex::new(super::state::PkgRuntime::default()))
    });
    let message = hooks.use_state(|| "Tab tabs · ↑↓ select · Enter actions · q quit".to_string());
    let allow_pre = hooks.use_state(|| false);
    let mut exit = hooks.use_exit();

    // 启动时刷新一次包表(阻塞网络,spawn_blocking)。
    {
        let dir = props.dir.clone();
        hooks.use_future(async move {
            let entries = tokio::task::spawn_blocking(move || {
                super::state::refresh_entries(&dir)
            })
            .await
            .unwrap_or_default();
            let mut ps = pkg.write();
            ps.entries = entries;
            ps.sel = ps.sel.min(ps.entries.len().saturating_sub(1));
        });
    }

    // 后台轮询:100ms 一拍,收扫描/会话/包任务事件、推进转圈动画。
    {
        let mut message = message;
        let dir = props.dir.clone();
        let dry_run = props.dry_run;
        hooks.use_future(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let mut msg: Option<String> = None;

                // 设备任务/会话轮询(锁只在同步段持有,不跨 await)。
                let mut devices_refreshed = false;
                {
                    let rts_arc = runtimes.read().clone();
                    let mut rts = rts_arc.lock().unwrap();
                    let super::state::Runtimes { keys: rt_keys, watch: rt_watch } = &mut *rts;
                    let mut ks = keys.write();
                    if let Some(m) = actions::poll_job(&mut ks, rt_keys) {
                        msg = msg.or(Some(m));
                    }
                    if let Some(m) = actions::poll_session(&mut ks, rt_keys) {
                        msg = msg.or(Some(m));
                    }
                    let mut ws = watch.write();
                    if let Some(m) = actions::poll_job(&mut ws, rt_watch) {
                        msg = msg.or(Some(m));
                    }
                    if let Some(m) = actions::poll_session(&mut ws, rt_watch) {
                        msg = msg.or(Some(m));
                    }
                    for t in [&mut ks, &mut ws] {
                        if t.job_active() {
                            t.spin += 1;
                        }
                    }
                }

                // 包任务轮询(同步段)。
                {
                    let prt_arc = pkg_rt.read().clone();
                    let mut prt = prt_arc.lock().unwrap();
                    let mut ps = pkg.write();
                    if let Some(m) = actions::poll_pkg_job(&mut ps, &mut prt, dry_run) {
                        msg = msg.or(Some(m));
                    }
                }

                // r 键请求的刷新:放锁后再阻塞拉取。
                let refresh_now = {
                    let mut ps = pkg.write();
                    let pending = ps.refresh_pending;
                    if pending {
                        ps.refresh_pending = false;
                    }
                    pending
                };
                if refresh_now {
                    let dir = dir.clone();
                    let entries = tokio::task::spawn_blocking(move || {
                        super::state::refresh_entries(&dir)
                    })
                    .await
                    .unwrap_or_default();
                    let mut ps = pkg.write();
                    ps.entries = entries;
                    ps.sel = ps.sel.min(ps.entries.len().saturating_sub(1));
                    ps.message = "refreshed".into();
                    msg = msg.or(Some("refreshed".into()));
                    let _ = &mut devices_refreshed;
                }
                let _ = devices_refreshed;

                if let Some(m) = msg {
                    message.set(m);
                }
            }
        });
    }

    // 全局输入:App 级单一 handler,内部保持旧版优先级链。
    {
        let dir = props.dir.clone();
        let dry_run = props.dry_run;
        hooks.use_event_handler(EventScope::Current, EventPriority::Normal, move |event| {
            let rts_arc = runtimes.read().clone();
            let mut rts = rts_arc.lock().unwrap();
            let super::state::Runtimes { keys: rt_keys, watch: rt_watch } = &mut *rts;
            let mut ks = keys.write();
            let mut ws = watch.write();
            let mut ps = pkg.write();
            let prt_arc = pkg_rt.read().clone();
            let mut prt = prt_arc.lock().unwrap();
            let mut msg = message.write();
            let mut ap = allow_pre.write();
            let outcome = actions::handle_key(
                event.clone(),
                tab,
                &mut ap,
                &mut ks,
                rt_keys,
                &mut ws,
                rt_watch,
                &mut ps,
                &mut prt,
                &dir,
                dry_run,
            );
            if let Some(m) = outcome.message {
                *msg = m;
            }
            if outcome.exit {
                exit();
            }
            EventResult::Consumed
        });
    }

    // allow_pre 的翻转走单独的轻量 handler(p 只在无 pop 框时有意义,框架层
    // 先于上面的主 handler 会被同一层吞并,这里保持旧版顺序即可)。
    let _ = allow_pre;

    let tab_val = tab.get();
    let demo_note = if props.demo { " · DEMO" } else { "" };
    element!(
        View(flex_direction: Direction::Vertical, height: Constraint::Fill(1)) {
            View(height: Constraint::Length(1)) {
                Text(text: tab_line(tab_val))
            }
            View(height: Constraint::Fill(1)) {
                match tab_val {
                    Tab::Keyboard => {
                        DevicePage(..DevicePageProps { state: keys })
                    }
                    Tab::Watch => {
                        DevicePage(..DevicePageProps { state: watch })
                    }
                    Tab::Packages => {
                        PackagesPage(..PackagesPageProps { state: pkg, allow_pre: allow_pre.get() })
                    }
                }
            }
            View(height: Constraint::Length(2)) {
                Text(text: Line::from(format!(
                    " vibeup {} · prerelease: {}{}",
                    env!("CARGO_PKG_VERSION"),
                    if allow_pre.get() { "on" } else { "off" },
                    demo_note,
                )))
                Text(text: Line::from(format!(
                    "{}  ·  q quit · Tab tabs · r refresh",
                    message.read().clone()
                )))
            }
        }
    )
}

fn tab_line(tab: Tab) -> Line<'static> {
    let spans: Vec<Span> = [Tab::Keyboard, Tab::Watch, Tab::Packages]
        .iter()
        .map(|t| {
            let n = match t {
                Tab::Keyboard => 1,
                Tab::Watch => 2,
                Tab::Packages => 3,
            };
            let name = match t {
                Tab::Keyboard => "Keyboard",
                Tab::Watch => "Watch",
                Tab::Packages => "Packages",
            };
            let text = format!(" {n} {name} ");
            let style = if *t == tab {
                Style::new().light_blue().bold()
            } else {
                Style::new()
            };
            Span::styled(text, style)
        })
        .collect();
    Line::from(spans)
}

// ===== 设备页 =====

#[derive(Props)]
pub struct DevicePageProps {
    pub state: State<DeviceState>,
}

#[component]
pub fn DevicePage(props: &DevicePageProps) -> impl Into<AnyElement<'static>> {
    let st = props.state.read().clone();
    element!(DevicePageInner(..DevicePageInnerProps { snapshot: st }))
}

#[derive(Props)]
pub struct DevicePageInnerProps {
    pub snapshot: DeviceState,
}

#[component]
pub fn DevicePageInner(props: &DevicePageInnerProps) -> impl Into<AnyElement<'static>> {
    let st = &props.snapshot;
    // 连接流程没走完:整页显示流程模态框;Ready:连接指示 + 分节 fieldset + pop 框。
    let body: AnyElement<'static> = if st.conn != Conn::Ready {
        conn_flow(st)
    } else {
        let mut children: Vec<AnyElement<'static>> = Vec::new();
        if let Some(name) = &st.connected_name {
            children.push(element!(View(height: Constraint::Length(2)) {
                Text(text: Line::from(vec![
                    Span::styled(" ● ", Style::new().green()),
                    Span::styled(name.clone(), Style::new().bold()),
                ]))
            }).into());
        }
        for sec in super::state::sections_of(&st.rows) {
            children.push(fieldset(st, sec));
        }
        if let Some(popup) = &st.popup {
            children.push(element!(View {
                PopupModal(popup: popup.clone())
            }).into());
        }
        let vp = ratatui_kit::components::ViewProps {
            flex_direction: Direction::Vertical,
            children,
            ..Default::default()
        };
        element!(View(..vp)).into()
    };
    let vp = ratatui_kit::components::ViewProps {
        flex_direction: Direction::Vertical,
        children: vec![body],
        ..Default::default()
    };
    let el: AnyElement<'static> = element!(View(..vp)).into();
    el
}

fn fieldset(st: &DeviceState, sec: super::state::Section) -> AnyElement<'static> {
    let mut lines: Vec<AnyElement<'static>> = Vec::new();
    for i in sec.start..sec.start + sec.len {
        let row = &st.rows[i];
        let selected = i == st.sel;
        let editing = st
            .editing
            .as_ref()
            .and_then(|(idx, input)| (*idx == i).then_some(input));
        lines.push(element!(Text(
            text: row_line(row, selected, editing)
        )).into());
    }
    let bp = ratatui_kit::components::BorderProps {
        borders: Borders::TOP,
        top_title: Some(Line::from(format!(" {} ", sec.name)).style(Style::new().bold())),
        children: lines,
        ..Default::default()
    };
    element!(Border(..bp)).into()
}

/// 设备页一行:「动作 + 当前值」;选中反色,编辑态原地变输入框 + 块状光标。
fn row_line(
    row: &super::state::DevRow,
    selected: bool,
    editing: Option<&super::state::Input>,
) -> Line<'static> {
    use super::state::RowAction;
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(row.label.clone(), Style::new().bold()),
    ];
    if let Some(input) = editing {
        let edit_style = Style::new().light_blue().bold();
        spans.push(Span::raw("  "));
        spans.extend(input.spans(true, edit_style));
        let hint = match &row.action {
            RowAction::Edit(field) => super::state::field_hint(*field),
            _ => "",
        };
        if !hint.is_empty() {
            spans.push(Span::styled(format!("  ({hint})"), dim()));
        }
    } else if !row.value.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(row.value.clone(), Style::new().gray()));
    }
    let line = Line::from(spans);
    if selected {
        line.style(Style::new().dark_gray())
    } else {
        line
    }
}

/// 连接流程模态框(内容居中):入口菜单 / 网页配置 / 扫描转圈 / 候选设备
/// 列表 / 连接转圈 / 失败提示 / 断连弹窗。告警态(失败/断连)红框。
fn conn_flow(st: &DeviceState) -> AnyElement<'static> {
    let frame = SPINNER[st.spin % SPINNER.len()];
    let (title, mut lines, alert, content_w) = match &st.conn {
        Conn::Ready => return element!(Fragment { }).into(),
        Conn::Fresh => (
            " scan ",
            vec![
                Line::from(" Not scanned yet"),
                Line::from(Span::styled(" Enter/r to scan", dim())),
            ],
            false,
            20,
        ),
        Conn::Menu => {
            let mut lines: Vec<Line> = ["Scan devices", "Configure on the web"]
                .into_iter()
                .enumerate()
                .map(|(i, label)| option_line(format!("{}. {}", i + 1, label), i == st.menu_sel))
                .collect();
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                " ↑↓/1/2 select · Enter confirm · Tab switch",
                dim(),
            )));
            (" setup ", lines, false, 40)
        }
        Conn::WebSetup => {
            let url = st.web_url.unwrap_or("");
            (
                " web setup ",
                vec![
                    Line::from(" Configure via web Bluetooth — open in your browser:"),
                    Line::from(Span::styled(url, Style::new().light_blue())),
                    Line::from(""),
                    Line::from(Span::styled(" Esc back", dim())),
                ],
                false,
                60,
            )
        }
        Conn::Scanning => (
            " scanning ",
            vec![
                Line::from(format!(" {frame} Scanning for devices…")),
                Line::from(Span::styled(" Esc cancel", dim())),
            ],
            false,
            30,
        ),
        Conn::Pick => {
            let mut lines: Vec<Line> = vec![Line::from(format!(
                " Found {} device(s) — pick one to configure:",
                st.pick.len()
            ))];
            let w = st
                .pick
                .iter()
                .map(|d| d.name.len())
                .max()
                .unwrap_or(10)
                + 34;
            for (i, d) in st.pick.iter().enumerate() {
                lines.push(option_line(d.name.clone(), i == st.pick_sel));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                " ↑↓ select · Enter connect · r rescan",
                dim(),
            )));
            (" devices ", lines, false, w)
        }
        Conn::Connecting(name) => (
            " connecting ",
            vec![
                Line::from(format!(" {frame} Connecting to {name}…")),
                Line::from(Span::styled(" Esc back to list", dim())),
            ],
            false,
            name.len() + 34,
        ),
        Conn::Failed(e) => (
            " error ",
            vec![
                Line::from(Span::styled(format!(" ✗ {e}"), err_style())),
                Line::from(Span::styled(" Enter/r retry", dim())),
            ],
            true,
            40,
        ),
        Conn::Disconnected => (
            " disconnected ",
            vec![
                Line::from(" Device disconnected"),
                Line::from(Span::styled(" Enter to rescan", dim())),
            ],
            true,
            30,
        ),
    };
    if st.demo
        && matches!(
            st.conn,
            Conn::Scanning | Conn::Pick | Conn::Connecting(_)
        )
    {
        lines.push(Line::from(Span::styled(" (demo fake devices)", dim())));
    }
    modal(title, lines, alert, content_w)
}

// ===== 公共渲染小件 =====

/// 居中模态框:四边边框 + 标题,宽度按内容预估,红框告警态。
fn modal(title: &str, lines: Vec<Line<'static>>, alert: bool, content_w: usize) -> AnyElement<'static> {
    let h = lines.len() as u16 + 2;
    let w = (content_w as u16 + 4).clamp(22, 120);
    let border_style = if alert { err_style() } else { Style::new() };
    let title_line = Line::from(Span::styled(
        format!(" {title} "),
        border_style.add_modifier(Modifier::BOLD),
    ));
    element!(Center(width: Constraint::Length(w), height: Constraint::Length(h)) {
        Border(
            borders: Borders::ALL,
            border_style: border_style,
            top_title: title_line,
        ) {
            for line in lines {
                Text(text: line)
            }
        }
    })
    .into()
}

/// 列表行(❯ 标记选中)。
fn option_line(text: String, selected: bool) -> Line<'static> {
    let marker = if selected { "❯" } else { " " };
    let line = Line::from(format!(" {marker} {text}"));
    if selected {
        line.style(Style::new().dark_gray().bold())
    } else {
        line
    }
}

// ===== 包页 =====

#[derive(Props)]
pub struct PackagesPageProps {
    pub state: State<PkgState>,
    pub allow_pre: bool,
}

#[component]
pub fn PackagesPage(props: &PackagesPageProps) -> impl Into<AnyElement<'static>> {
    let st = props.state.read().clone();
    let header = Line::from(vec![
        Span::styled(format!("{:<12}", "package"), Style::new().bold()),
        Span::styled(format!("{:<8}", "inst"), Style::new().bold()),
        Span::styled(format!("{:<10}", "latest"), Style::new().bold()),
        Span::styled(format!("{:<12}", "pre"), Style::new().bold()),
        Span::styled("status", Style::new().bold()),
    ]);
    let mut lines: Vec<Line<'static>> = vec![header, Line::from("")];
    for (i, e) in st.entries.iter().enumerate() {
        let installed = e.installed.clone().unwrap_or_else(|| "not installed".into());
        let status = e.status(props.allow_pre);
        let selected = i == st.sel;
        let line = Line::from(vec![
            Span::raw(format!("{:<12}", e.pkg.name)),
            Span::raw(format!("{:<8}", short(installed.as_str()))),
            Span::raw(format!("{:<10}", e.latest.clone().unwrap_or_else(|| "?".into()))),
            Span::raw(format!("{:<12}", e.any.clone().unwrap_or_else(|| "-".into()))),
            Span::raw(short(status).to_string()),
        ]);
        lines.push(if selected {
            line.style(Style::new().dark_gray())
        } else {
            line
        });
    }

    let body: AnyElement<'static> = if let Some(m) = &st.menu {
        let e = st.entries.get(m.pkg);
        let (latest, any, title) = match e {
            Some(e) => (
                e.latest.as_deref(),
                e.any.as_deref(),
                match &e.installed {
                    Some(v) => format!(" {} ({v}) ", e.pkg.name),
                    None => format!(" {} ", e.pkg.name),
                },
            ),
            None => (None, None, " packages ".into()),
        };
        let items = [
            ("Install / update latest", latest),
            ("Install prerelease", any),
            ("Uninstall", None),
        ];
        let mut menu_lines: Vec<Line> = items
            .iter()
            .enumerate()
            .map(|(i, (label, ver))| {
                let text = match ver {
                    Some(v) => format!("{}. {} ({v})", i + 1, label),
                    None => format!("{}. {}", i + 1, label),
                };
                option_line(text, i == m.sel)
            })
            .collect();
        menu_lines.push(Line::from(""));
        menu_lines.push(Line::from(Span::styled(
            " ↑↓ select · Enter confirm · Esc back",
            dim(),
        )));
        element!(View(flex_direction: Direction::Vertical) {
            View(height: Constraint::Fill(1)) {
                Text(text: Line::from(""))
            }
            PackagesMenuModal(title: title, lines: menu_lines)
        }).into()
    } else if st.running {
        let frame = SPINNER[st.spin % SPINNER.len()];
        let mb = |b: u64| format!("{:.1} MB", b as f64 / 1024.0 / 1024.0);
        let progress_line = match st.progress {
            Some((done, Some(total))) => {
                let pct = done.checked_mul(100).map_or(0, |n| n / total.max(1));
                format!(" {frame} {}…  {} / {} ({pct}%)", st.job_label, mb(done), mb(total))
            }
            Some((done, None)) => {
                format!(" {frame} {}…  {} downloaded", st.job_label, mb(done))
            }
            None => format!(" {frame} {}…", st.job_label),
        };
        element!(View(flex_direction: Direction::Vertical) {
            View(height: Constraint::Fill(1)) {
                Text(text: Line::from(""))
            }
            PackagesWorkingModal(line: progress_line)
        }).into()
    } else {
        let mut children: Vec<AnyElement<'static>> = Vec::new();
        for line in lines {
            children.push(element!(View(height: Constraint::Length(1)) {
                Text(text: line)
            }).into());
        }
        let vp = ratatui_kit::components::ViewProps {
            flex_direction: Direction::Vertical,
            children,
            ..Default::default()
        };
        element!(View(..vp)).into()
    };
    let vp = ratatui_kit::components::ViewProps {
        flex_direction: Direction::Vertical,
        children: vec![body],
        ..Default::default()
    };
    let el: AnyElement<'static> = element!(View(..vp)).into();
    el
}

fn short(s: &str) -> &str {
    super::state::short_status(s)
}

// ===== 模态框组件(占满父区域,内容居中)=====

#[derive(Props)]
pub struct PopupModalProps {
    pub popup: super::state::PopupState,
}

impl Default for PopupModalProps {
    fn default() -> Self {
        Self {
            popup: super::state::PopupState::new(
                super::state::PopupKind::WifiAdd,
                Vec::new(),
            ),
        }
    }
}

#[component]
pub fn PopupModal(props: &PopupModalProps) -> impl Into<AnyElement<'static>> {
    let popup = &props.popup;
    let mut lines: Vec<Line> = Vec::new();
    for (i, label) in popup.labels.iter().enumerate() {
        let active = i == popup.cur;
        let style = if active {
            Style::new().light_blue().bold()
        } else {
            Style::new()
        };
        let mut line_spans = vec![
            Span::raw(" "),
            Span::styled(format!("{label:>6}: "), Style::new().bold()),
        ];
        line_spans.extend(popup.inputs[i].spans(i == popup.cur, style));
        lines.push(Line::from(line_spans));
    }
    lines.push(Line::from(Span::styled(
        " Tab/↑↓ field · Enter save · Esc cancel ",
        dim(),
    )));
    let content_w = 56u16;
    let h = popup.labels.len() as u16 + 4;
    element!(Center(width: Constraint::Length(content_w), height: Constraint::Length(h)) {
        Border(
            borders: Borders::ALL,
            top_title: Line::from(popup.title.clone()),
        ) {
            for line in lines {
                Text(text: line)
            }
        }
    })
}

#[derive(Props)]
#[derive(Default)]
pub struct PackagesMenuModalProps {
    pub title: String,
    pub lines: Vec<Line<'static>>,
}


#[component]
pub fn PackagesMenuModal(props: &PackagesMenuModalProps) -> impl Into<AnyElement<'static>> {
    let h = props.lines.len() as u16 + 2;
    let w = (props
        .lines
        .iter()
        .map(|l| l.width())
        .max()
        .unwrap_or(20) as u16
        + 4)
        .clamp(22, 120);
    element!(Center(width: Constraint::Length(w), height: Constraint::Length(h)) {
        Border(
            borders: Borders::ALL,
            top_title: Line::from(props.title.clone()).style(Style::new().bold()),
        ) {
            for line in props.lines.clone() {
                Text(text: line)
            }
        }
    })
}

#[derive(Props)]
pub struct PackagesWorkingModalProps {
    pub line: Line<'static>,
}

impl Default for PackagesWorkingModalProps {
    fn default() -> Self {
        Self {
            line: Line::from(""),
        }
    }
}

#[component]
pub fn PackagesWorkingModal(props: &PackagesWorkingModalProps) -> impl Into<AnyElement<'static>> {
    let lines = vec![
        props.line.clone(),
        Line::from(Span::styled(" downloading — hang on", dim())),
    ];
    let h = lines.len() as u16 + 2;
    let w = (lines.iter().map(|l| l.width()).max().unwrap_or(20) as u16 + 4).clamp(22, 120);
    element!(Center(width: Constraint::Length(w), height: Constraint::Length(h)) {
        Border(
            borders: Borders::ALL,
            top_title: Line::from(" working ").style(Style::new().bold()),
        ) {
            for line in lines {
                Text(text: line)
            }
        }
    })
}
