//! UI 组件层:全部是纯函数 `(state, props) -> Element`,只读状态、拼
//! Element 树,不碰终端。`root` 是根组件,按 tab 组合子组件;绘制统一交给
//! `element::Element::render`。终端宽度作为 prop 自顶向下传,宽窄布局在
//! 组件里切换。

use super::app::{App, Tab};
use super::device::{Conn, DeviceTab};
use super::element::Element;
use super::fields::{DevRow, PopupState, RowAction, Section, field_hint, sections_of};
use super::input::Input;
use super::pkgs::{Entry, short_status};
use ratatui::layout::Constraint;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// 退出 TUI 模式执行阻塞动作(install / BLE 要往正常屏幕打 stdout),完事重建 TUI。
pub(super) fn suspend<T>(terminal: &mut ratatui::DefaultTerminal, f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    ratatui::restore();
    // 动作执行期间(下载几秒、BLE 扫描 2-5 秒)终端在正常屏,底下是 shell。
    // 保持 raw mode:按键不回显、不进 shell 的行缓冲,只静静排队,回来后排掉。
    // 否则挂起窗口里敲的 `q` 之类会漏给 shell 真的执行。
    let _raw = crossterm::terminal::enable_raw_mode();
    let res = f();
    // 排空挂起期间积压的按键,免得恢复后被当成 TUI 快捷键(比如 `q`)。
    while ratatui::crossterm::event::poll(std::time::Duration::ZERO).unwrap_or(false) {
        let _ = ratatui::crossterm::event::read();
    }
    drop(_raw);
    *terminal = ratatui::init();
    res
}

pub(super) fn on_off(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

/// 根组件:整帧 = 标签栏 + 当前页 + footer。
pub(super) fn root(app: &App, width: u16) -> Element {
    Element::column_expanding(1, vec![tab_bar(app), page(app, width), footer(app)])
}

fn page(app: &App, width: u16) -> Element {
    match app.tab {
        Tab::Keys => device_page(&app.keys),
        Tab::Watch => device_page(&app.watch),
        Tab::Pkgs => packages_page(&app.pkgs, app.pkg_sel, app.allow_pre, width),
    }
}

fn tab_bar(app: &App) -> Element {
    let spans: Vec<Span> = Tab::all()
        .iter()
        .map(|t| {
            let text = format!(
                " {} {} ",
                Tab::all().iter().position(|x| x == t).unwrap() + 1,
                t.title()
            );
            let style = if *t == app.tab {
                Style::default()
                    .bg(Color::LightBlue)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Span::styled(text, style)
        })
        .collect();
    Element::lines(vec![Line::from(spans)])
}

fn footer(app: &App) -> Element {
    let title = format!(
        " vibeup {} · prerelease: {}{} ",
        env!("CARGO_PKG_VERSION"),
        on_off(app.allow_pre),
        if app.demo { " · DEMO" } else { "" }
    );
    Element::lines(vec![
        Line::from(title),
        Line::from(format!(
            "{}  ·  q quit · Tab tabs · r refresh",
            app.message
        )),
    ])
}

// ===== 设备页 =====

/// 设备页:连接流程没走完时整页是流程模态框;Ready 后是
/// 「连接指示 + 若干 fieldset + pop 框浮层」。
fn device_page(t: &DeviceTab) -> Element {
    if t.conn != Conn::Ready {
        return conn_flow(t);
    }
    let mut children: Vec<Element> = Vec::new();
    // 顶部一块:当前连着的设备(名字来自扫描列表的广播名)。
    if let Some(c) = &t.connected {
        children.push(Element::lines(vec![
            Line::from(vec![
                Span::styled(" ● ", Style::default().fg(Color::Green)),
                Span::styled(c.name.clone(), Style::default().add_modifier(Modifier::BOLD)),
            ]),
            Line::from(""),
        ]));
    }
    // 行按 section 分组成 fieldset。
    for sec in sections_of(&t.rows) {
        children.push(fieldset(t, &sec));
    }
    // pop 框:居中浮层,画在内容之上。
    if let Some(popup) = &t.popup {
        children.push(Element::Overlay(Box::new(popup_modal(popup))));
    }
    Element::column(children)
}

fn fieldset(t: &DeviceTab, sec: &Section) -> Element {
    let children: Vec<Element> = (sec.start..sec.start + sec.len)
        .map(|i| {
            let row = &t.rows[i];
            let editing = t
                .editing
                .as_ref()
                .and_then(|(idx, input)| (*idx == i).then_some(input));
            Element::lines(vec![row_line(row, i == t.sel, editing)])
        })
        .collect();
    Element::Fieldset {
        title: sec.name.into(),
        children,
    }
}

/// 设备页一行:「动作 + 当前值」;选中反色,编辑态原地变输入框 + 块状光标。
fn row_line(row: &DevRow, selected: bool, editing: Option<&Input>) -> Line<'static> {
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(
            row.label.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ];

    if let Some(input) = editing {
        // 原地编辑框:值的位置变成输入框 + 块状光标。
        let edit_style = Style::default()
            .bg(Color::LightBlue)
            .add_modifier(Modifier::BOLD);
        spans.push(Span::raw("  "));
        spans.extend(input.spans(true, edit_style));
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
    if selected {
        line.style(Style::default().bg(Color::DarkGray))
    } else {
        line
    }
}

/// pop 框组件:字段逐行,当前字段高亮,末尾一条键位提示。
fn popup_modal(popup: &PopupState) -> Element {
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
    Element::Modal {
        title: popup.title.clone(),
        alert: false,
        body: lines,
    }
}

/// 连接流程组件(内容居中模态框):入口菜单 / 网页配置 / 扫描转圈 /
/// 候选设备列表 / 连接转圈 / 失败提示 / 断连弹窗。
fn conn_flow(t: &DeviceTab) -> Element {
    let frame = SPINNER[t.spin % SPINNER.len()];
    let dim = Style::default().fg(Color::DarkGray);
    let err = Style::default().fg(Color::Red);
    /// 列表行(❯ 标记选中)。
    fn option_line(text: String, selected: bool) -> Line<'static> {
        let marker = if selected { "❯" } else { " " };
        let line = Line::from(format!(" {marker} {text}"));
        if selected {
            line.style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            line
        }
    }
    // 各状态的内容;(弹窗标题, 行, 红 = 告警态)。
    let (title, mut lines, alert): (&str, Vec<Line>, bool) = match &t.conn {
        // Ready 不走这里(device_page 已分流),兜底画空。
        Conn::Ready => return Element::Nothing,
        Conn::Fresh => (
            " scan ",
            vec![
                Line::from(" Not scanned yet"),
                Line::from(Span::styled(" Enter/r to scan", dim)),
            ],
            false,
        ),
        Conn::Menu => {
            let mut lines: Vec<Line> = ["Scan devices", "Configure on the web"]
                .into_iter()
                .enumerate()
                .map(|(i, label)| option_line(format!("{}. {}", i + 1, label), i == t.menu_sel))
                .collect();
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
            let mut lines: Vec<Line> = vec![Line::from(format!(
                " Found {} device(s) — pick one to configure:",
                t.pick.len()
            ))];
            for (i, d) in t.pick.iter().enumerate() {
                lines.push(option_line(d.name.clone(), i == t.pick_sel));
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
    // demo 提示只对设备相关的状态有意义(菜单/网页配置页没有"设备"概念)。
    if t.demo
        && matches!(
            t.conn,
            Conn::Scanning | Conn::Pick | Conn::Connecting(_)
        )
    {
        lines.push(Line::from(Span::styled(" (demo fake devices)", dim)));
    }
    Element::Modal {
        title: title.into(),
        alert,
        body: lines,
    }
}

// ===== 管理页 =====

/// 包表格组件:宽屏全名,窄屏缩写。
fn packages_page(entries: &[Entry], sel: usize, allow_pre: bool, width: u16) -> Element {
    let wide = width >= 78;
    let header: Vec<&'static str> = if wide {
        vec!["package", "installed", "latest", "latest-pre", "status"]
    } else {
        vec!["package", "inst", "latest", "pre", "status"]
    };
    let rows: Vec<Vec<String>> = entries
        .iter()
        .map(|e| {
            let installed = e
                .installed
                .clone()
                .unwrap_or_else(|| "not installed".into());
            let status = e.status(allow_pre);
            let status = if wide {
                status.into()
            } else {
                short_status(status).into()
            };
            vec![
                e.pkg.name.into(),
                if wide {
                    installed
                } else {
                    short_status(&installed).into()
                },
                e.latest.clone().unwrap_or_else(|| "?".into()),
                e.any
                    .clone()
                    .filter(|a| e.latest.as_ref().is_some_and(|l| a != l))
                    .unwrap_or_else(|| "-".into()),
                status,
            ]
        })
        .collect();
    let widths = if wide {
        vec![
            Constraint::Length(12),
            Constraint::Length(16),
            Constraint::Length(13),
            Constraint::Length(13),
            Constraint::Min(18),
        ]
    } else {
        vec![
            Constraint::Length(12),
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Length(13),
            Constraint::Min(10),
        ]
    };
    Element::Table {
        header,
        rows,
        widths,
        selected: sel,
    }
}
