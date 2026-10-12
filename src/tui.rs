//! `vibeup tui` —— ratatui 面板:包状态一览,键位触发安装/更新。
//!
//! 网络与安装动作都是同步的,量级只有两个包,不值得上后台任务;执行动作时
//! 先退出 TUI 模式让 install 的 stdout 直接打到正常屏幕,结束后再回 TUI。

use crate::install;
use crate::pkg::{self, PKGS, Pkg};
use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Cell, Paragraph, Row as TRow, Table};
use ratatui::{DefaultTerminal, Frame};
use std::path::PathBuf;

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

pub fn run(dir: Option<PathBuf>, dry_run: bool) -> Result<()> {
    let dir = pkg::install_dir(dir.as_deref());
    let mut entries: Vec<Entry> = PKGS.iter().map(Entry::new).collect();
    let mut sel = 0usize;
    let mut allow_pre = false;
    let mut message =
        String::from("q quit · u update · U update all · p toggle prerelease · r refresh");
    fetch(&mut entries, &dir);

    let mut terminal = ratatui::init();
    loop {
        terminal.draw(|f| ui(f, &entries, sel, allow_pre, &message))?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => break,
            KeyCode::Char('c')
                if key
                    .modifiers
                    .contains(ratatui::crossterm::event::KeyModifiers::CONTROL) =>
            {
                break;
            }
            KeyCode::Char('j') | KeyCode::Down => sel = (sel + 1) % entries.len(),
            KeyCode::Char('k') | KeyCode::Up => sel = (sel + entries.len() - 1) % entries.len(),
            KeyCode::Char('p') => {
                allow_pre = !allow_pre;
                message = format!("prerelease: {}", on_off(allow_pre));
            }
            KeyCode::Char('r') => {
                message = "refreshing…".into();
                terminal.draw(|f| ui(f, &entries, sel, allow_pre, &message))?;
                fetch(&mut entries, &dir);
                message = "refreshed".into();
            }
            KeyCode::Char('u') => {
                let pkg = entries[sel].pkg;
                let target = entries[sel].target(allow_pre);
                let (msg, changed) = run_action(&mut terminal, dry_run, || {
                    install::install(pkg, target.clone(), allow_pre, &dir, dry_run)
                });
                if changed {
                    fetch(&mut entries, &dir);
                }
                message = format!("{}: {msg}", pkg.name);
            }
            KeyCode::Char('U') => {
                let all: Vec<(&Pkg, Option<String>)> = entries
                    .iter()
                    .map(|e| (e.pkg, e.target(allow_pre)))
                    .collect();
                let (_, changed) = run_action(&mut terminal, dry_run, || {
                    all.iter()
                        .map(|&(p, ref t)| install::install(p, t.clone(), allow_pre, &dir, dry_run))
                        .collect::<Result<Vec<_>>>()
                        .map(|_| ())
                });
                if changed {
                    fetch(&mut entries, &dir);
                }
                message = "updated all".into();
            }
            _ => {}
        }
    }
    ratatui::restore();
    Ok(())
}

/// 退出 TUI 模式执行动作(install 要往正常屏幕打 stdout),完事重建 TUI。
/// 返回 (结果摘要, 是否可能改动了安装)。
fn run_action(
    terminal: &mut DefaultTerminal,
    dry_run: bool,
    action: impl FnOnce() -> Result<()>,
) -> (String, bool) {
    ratatui::restore();
    let res = action();
    *terminal = ratatui::init();
    match res {
        Ok(()) if dry_run => ("dry-run ok".into(), false),
        Ok(()) => ("done".into(), true),
        Err(e) => (format!("{e:#}"), false),
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

fn ui(f: &mut Frame, entries: &[Entry], sel: usize, allow_pre: bool, message: &str) {
    let [area, footer] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(2)]).areas(f.area());

    // 窄屏(herdr 分栏常见 59 列)切紧凑文案,宽屏用完整句子。
    let wide = f.area().width >= 78;

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

    // 列宽按「内容 + 左右各 1 格 padding」给;紧凑档按 59 列的 herdr 分栏设计。
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

    let title = format!(
        " vibeup {} · prerelease: {} ",
        env!("CARGO_PKG_VERSION"),
        on_off(allow_pre)
    );
    let help = Paragraph::new(vec![
        ratatui::text::Line::from(title),
        ratatui::text::Line::from(format!(
            "{message}  ·  q quit · u update · U all · p pre · r refresh"
        )),
    ]);
    f.render_widget(help, footer);
}
