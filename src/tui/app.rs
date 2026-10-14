//! App:全部 TUI 状态 + 事件循环 + 按键分发。绘制是纯的
//! `view::root(&app, 宽度) -> Element`,本层只负责改状态;设备动作走
//! `DeviceTab::do_*`(常驻会话,瞬时),包安装/更新走后台线程 + footer 转圈。

use super::device::{Conn, DeviceTab, device_flow_key};
use super::fields::{PopupKind, PopupState, RowAction, next_in_cycle};
use super::input::Input;
use super::pkgs::{Entry, PkgJob, PkgJobOut, PkgMenu, fetch, spawn_pkg_job};
use super::view::{self, on_off};
use crate::install;
use crate::pkg::{self, PKGS, Pkg};
use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;
use std::path::PathBuf;

/// 三个标签:键盘 / 手表(BLE 设备配置)+ 管理(vibetty / vibekeys 安装更新)。
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Tab {
    Keys,
    Watch,
    Pkgs,
}

impl Tab {
    pub(super) fn all() -> [Tab; 3] {
        [Tab::Keys, Tab::Watch, Tab::Pkgs]
    }

    pub(super) fn title(self) -> &'static str {
        match self {
            Tab::Keys => "Keyboard",
            Tab::Watch => "Watch",
            Tab::Pkgs => "Packages",
        }
    }

    pub(super) fn next(self) -> Self {
        let all = Self::all();
        let i = all.iter().position(|t| *t == self).unwrap_or(0);
        all[(i + 1) % all.len()]
    }
}

pub(super) struct App {
    pub(super) tab: Tab,
    pub(super) keys: DeviceTab,
    pub(super) watch: DeviceTab,
    pub(super) pkgs: Vec<Entry>,
    pub(super) pkg_sel: usize,
    /// Packages 页回车弹出的动作菜单。
    pub(super) pkg_menu: Option<PkgMenu>,
    /// 进行中的包任务(安装/更新,后台线程)。
    pub(super) pkg_job: Option<PkgJob>,
    /// 包任务期间 footer 的提示文案。
    pub(super) pkg_job_label: String,
    /// 包任务转圈帧计数。
    pub(super) pkg_spin: usize,
    /// 当前下载进度(后台线程写,view 读)。
    pub(super) pkg_progress: Option<super::pkgs::SharedProgress>,
    pub(super) allow_pre: bool,
    pub(super) message: String,
    /// 管理页的安装目录。
    dir: PathBuf,
    dry_run: bool,
    pub(super) demo: bool,
}

impl App {
    pub(super) fn new(dir: Option<PathBuf>, dry_run: bool, demo: bool) -> Self {
        let dir = pkg::install_dir(dir.as_deref());
        let mut pkgs: Vec<Entry> = PKGS.iter().map(Entry::new).collect();
        // 启动就查一次(阻塞无所谓,还没进 TUI 模式)。
        fetch(&mut pkgs, &dir);
        Self {
            tab: Tab::Pkgs,
            keys: DeviceTab::new(crate::ble::DeviceKind::VibeKeys, demo),
            watch: DeviceTab::new(crate::ble::DeviceKind::Watch, demo),
            pkgs,
            pkg_sel: 0,
            pkg_menu: None,
            pkg_job: None,
            pkg_job_label: String::new(),
            pkg_spin: 0,
            pkg_progress: None,
            allow_pre: false,
            // 默认停在「管理」:不碰蓝牙,启动零延迟;设备页进了才连。
            message: String::from("Tab tabs · ↑↓ select · Enter run/edit · r refresh · q quit"),
            dir,
            dry_run,
            demo,
        }
    }

    pub(super) fn run(&mut self) -> Result<()> {
        let mut terminal = ratatui::init();
        // 启动前排空 tty 里残留的旧按键(比如上一次会话漏下的 `q`),否则一进
        // 主循环就被当成快捷键直接退出。
        while event::poll(std::time::Duration::ZERO).unwrap_or(false) {
            let _ = event::read();
        }
        loop {
            self.draw(&mut terminal)?;
            let ev = if self.busy() {
                // 后台扫描/连接进行中,或常驻会话活着(要收断连事件):限时等键 +
                // 轮询;转圈动画靠空转重画推进。
                if event::poll(std::time::Duration::from_millis(80))? {
                    event::read()?
                } else {
                    self.poll_background();
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
            if self.on_key(key, &mut terminal)? {
                break;
            }
        }
        ratatui::restore();
        Ok(())
    }

    fn draw(&self, terminal: &mut DefaultTerminal) -> Result<()> {
        terminal.draw(|f| {
            let el = view::root(self, f.area().width);
            el.render(f, f.area());
        })?;
        Ok(())
    }

    /// 有后台任务或常驻会话活着 → 需要轮询。
    fn busy(&self) -> bool {
        self.keys.job.is_some()
            || self.watch.job.is_some()
            || self.keys.session.is_some()
            || self.watch.session.is_some()
            || self.pkg_job.is_some()
    }

    /// 空转一轮:收后台任务/会话事件、推进转圈动画。
    fn poll_background(&mut self) {
        if let Some(m) = self.keys.poll_job() {
            self.message = m;
        }
        if let Some(m) = self.watch.poll_job() {
            self.message = m;
        }
        if let Some(m) = self.keys.poll_session() {
            self.message = m;
        }
        if let Some(m) = self.watch.poll_session() {
            self.message = m;
        }
        // 包任务收尾:结果进 footer,换上线程里 fetch 好的新表。
        if let Some(job) = &self.pkg_job
            && job.is_done()
        {
            if let Some(PkgJobOut::Finished(res, entries)) = job.try_recv() {
                self.message = format!("{}: {}", self.pkg_job_label, describe(&res, self.dry_run));
                self.pkgs = entries;
                self.pkg_sel = self.pkg_sel.min(self.pkgs.len() - 1);
            }
            self.pkg_job = None;
            self.pkg_progress = None;
        }
        if self.pkg_job.is_some() {
            self.pkg_spin += 1;
        }
        for t in [&mut self.keys, &mut self.watch] {
            if t.job.is_some() || matches!(t.conn, Conn::Connecting(_)) {
                t.spin += 1;
            }
        }
    }

    /// 按键分发,优先级:退出 > pop 框 > 行内编辑 > 连接流程 > 全局键。
    /// 返回 true = 退出。
    fn on_key(&mut self, key: KeyEvent, terminal: &mut DefaultTerminal) -> Result<bool> {
        let is_quit = key.code == KeyCode::Char('q')
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL));
        if is_quit {
            return Ok(true);
        }

        // pop 框独占按键(只会在当前设备页打开;Keys 优先与原实现一致)。
        if self.keys.popup.is_some() || self.watch.popup.is_some() {
            if self.keys.popup.is_some() {
                popup_key(&mut self.keys, key, self.dry_run, &mut self.message);
            } else {
                popup_key(&mut self.watch, key, self.dry_run, &mut self.message);
            }
            return Ok(false);
        }
        // 行内编辑独占按键。
        if self.keys.editing.is_some() || self.watch.editing.is_some() {
            if self.keys.editing.is_some() {
                editing_key(&mut self.keys, key, self.dry_run, &mut self.message);
            } else {
                editing_key(&mut self.watch, key, self.dry_run, &mut self.message);
            }
            return Ok(false);
        }
        // 设备连接流程(未就绪)独占按键;Tab 切标签、退出保持可用。
        if key.code != KeyCode::Tab && self.tab != Tab::Pkgs {
            let t = match self.tab {
                Tab::Keys => &mut self.keys,
                Tab::Watch => &mut self.watch,
                Tab::Pkgs => unreachable!(),
            };
            if !matches!(t.conn, Conn::Ready) {
                let mut msg = String::new();
                device_flow_key(t, key.code, &mut msg);
                if !msg.is_empty() {
                    self.message = msg;
                }
                return Ok(false);
            }
        }
        // Packages 页的动作菜单。
        if self.pkg_menu.is_some() {
            if key.code == KeyCode::Tab {
                self.switch_tab(self.tab.next());
            } else {
                self.pkg_menu_key(key)?;
            }
            return Ok(false);
        }
        // 包任务进行中:忽略其余按键(footer 有提示),等收尾。
        if self.pkg_job.is_some() {
            return Ok(false);
        }

        self.global_key(key, terminal)?;
        Ok(false)
    }

    /// Packages 页动作菜单的按键:导航 + 回车执行。
    fn pkg_menu_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => self.pkg_menu = None,
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(m) = self.pkg_menu.as_mut() {
                    m.sel = (m.sel + 1).min(super::pkgs::PKG_MENU_ITEMS.len() - 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(m) = self.pkg_menu.as_mut() {
                    m.sel = m.sel.saturating_sub(1);
                }
            }
            KeyCode::Enter => {
                if let Some(m) = self.pkg_menu.take() {
                    match m.sel {
                        0 => self.install_pkg(m.pkg, false),
                        1 => self.install_pkg(m.pkg, true),
                        _ => self.uninstall_pkg(m.pkg),
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn global_key(&mut self, key: KeyEvent, terminal: &mut DefaultTerminal) -> Result<()> {
        match key.code {
            KeyCode::Tab => self.switch_tab(self.tab.next()),
            KeyCode::Char('1') => self.switch_tab(Tab::all()[0]),
            KeyCode::Char('2') => self.switch_tab(Tab::all()[1]),
            KeyCode::Char('3') => self.switch_tab(Tab::all()[2]),
            KeyCode::Char('j') | KeyCode::Down => match self.tab {
                Tab::Keys => self.keys.sel = (self.keys.sel + 1).min(self.keys.rows.len() - 1),
                Tab::Watch => self.watch.sel = (self.watch.sel + 1).min(self.watch.rows.len() - 1),
                Tab::Pkgs => self.pkg_sel = (self.pkg_sel + 1) % self.pkgs.len(),
            },
            KeyCode::Char('k') | KeyCode::Up => match self.tab {
                Tab::Keys => self.keys.sel = self.keys.sel.saturating_sub(1),
                Tab::Watch => self.watch.sel = self.watch.sel.saturating_sub(1),
                Tab::Pkgs => self.pkg_sel = (self.pkg_sel + self.pkgs.len() - 1) % self.pkgs.len(),
            },
            KeyCode::Char('p') => {
                self.allow_pre = !self.allow_pre;
                self.message = format!("prerelease: {}", on_off(self.allow_pre));
            }
            KeyCode::Char('r') => match self.tab {
                Tab::Keys => refresh_device(&mut self.keys, &mut self.message),
                Tab::Watch => refresh_device(&mut self.watch, &mut self.message),
                Tab::Pkgs => {
                    // 阻塞刷新:先画一帧"refreshing…",查完再回来。
                    self.message = "refreshing…".into();
                    self.draw(terminal)?;
                    fetch(&mut self.pkgs, &self.dir);
                    self.message = "refreshed".into();
                }
            },
            KeyCode::Char('d') if self.tab != Tab::Pkgs => {
                let msg = &mut self.message;
                match self.tab {
                    Tab::Keys => wifi_remove(&mut self.keys, self.dry_run, msg),
                    Tab::Watch => wifi_remove(&mut self.watch, self.dry_run, msg),
                    Tab::Pkgs => unreachable!(),
                }
            }
            KeyCode::Enter => match self.tab {
                Tab::Keys => row_action(&mut self.keys, self.dry_run, &mut self.message),
                Tab::Watch => row_action(&mut self.watch, self.dry_run, &mut self.message),
                Tab::Pkgs => {
                    self.pkg_menu = Some(PkgMenu {
                        pkg: self.pkg_sel,
                        sel: 0,
                    });
                }
            },
            KeyCode::Char('u') if self.tab == Tab::Pkgs => self.install_selected(),
            KeyCode::Char('U') if self.tab == Tab::Pkgs => {
                let all: Vec<(&Pkg, Option<String>)> = self
                    .pkgs
                    .iter()
                    .map(|e| (e.pkg, e.target(self.allow_pre)))
                    .collect();
                // 全部已最新就不动(和 install_pkg 的本地预检同理)。
                let all_fresh = all.iter().all(|(p, t)| match t {
                    Some(t) => {
                        matches!(
                            pkg::installed_version(p, &self.dir),
                            Some(v) if pkg::normalize(&v) == pkg::normalize(t)
                        )
                    }
                    None => false,
                });
                if all_fresh {
                    self.message = "all packages up to date".into();
                    return Ok(());
                }
                let (dir, allow_pre, dry_run) = (self.dir.clone(), self.allow_pre, self.dry_run);
                self.pkg_job_label = "update all".into();
                self.pkg_job = Some(spawn_pkg_job(move || {
                    let res = all
                        .iter()
                        .map(|&(p, ref t)| {
                            install::install(p, t.clone(), allow_pre, &dir, dry_run, false, &|_, _| {})
                        })
                        .collect::<Result<Vec<_>>>()
                        .map(|_| ());
                    let mut entries: Vec<Entry> = crate::pkg::PKGS.iter().map(Entry::new).collect();
                    fetch(&mut entries, &dir);
                    PkgJobOut::Finished(res, entries)
                }));
                self.message = "update all: running…".into();
            }
            _ => {}
        }
        Ok(())
    }

    fn switch_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.message = default_message(tab);
        match tab {
            Tab::Keys => self.keys.enter(),
            Tab::Watch => self.watch.enter(),
            Tab::Pkgs => {}
        }
    }

    /// 管理页:安装/更新选中包(u 快捷键,跟随 p 的 prerelease 开关)。
    fn install_selected(&mut self) {
        let idx = self.pkg_sel;
        let pre = self.allow_pre;
        self.install_pkg(idx, pre);
    }

    /// 安装/更新 `pkgs[idx]`。已最新(纯本地检查,毫秒级)直接写 footer;
    /// 真要下载就丢到后台线程,footer 转圈提示,完成后自动刷新表格。
    fn install_pkg(&mut self, idx: usize, pre: bool) {
        let e = &self.pkgs[idx];
        let target = e.target(pre);
        let pkg = e.pkg;
        if let Some(t) = &target
            && let Some(v) = pkg::installed_version(pkg, &self.dir)
            && pkg::normalize(&v) == pkg::normalize(t)
        {
            self.message = format!("{} {v}: up to date", pkg.name);
            return;
        }
        let (dir, dry_run) = (self.dir.clone(), self.dry_run);
        self.pkg_job_label = format!("installing {}", pkg.name);
        let progress: super::pkgs::SharedProgress =
            std::sync::Arc::new(std::sync::Mutex::new(Default::default()));
        self.pkg_progress = Some(progress.clone());
        self.pkg_job = Some(spawn_pkg_job(move || {
            let tick = move |downloaded: u64, total: Option<u64>| {
                *progress.lock().unwrap() = super::pkgs::DownloadProgress { downloaded, total };
            };
            let res = install::install(pkg, target.clone(), pre, &dir, dry_run, false, &tick);
            let mut entries: Vec<Entry> = crate::pkg::PKGS.iter().map(Entry::new).collect();
            fetch(&mut entries, &dir);
            PkgJobOut::Finished(res.map(|_| ()), entries)
        }));
        self.message = format!("{}: installing…", pkg.name);
    }

    /// 卸载 `pkgs[idx]`:纯本地文件操作且不再打印,毫秒级完成,不挂屏。
    fn uninstall_pkg(&mut self, idx: usize) {
        let pkg = self.pkgs[idx].pkg;
        if pkg::installed_version(pkg, &self.dir).is_none() {
            self.message = format!("{}: not installed", pkg.name);
            return;
        }
        let dir = self.dir.clone();
        let res = install::uninstall(pkg, &dir);
        self.message = format!("{}: {}", pkg.name, describe(&res, self.dry_run));
        fetch(&mut self.pkgs, &self.dir);
    }
}

// ===== 按键 handler(自由函数:借 t / message 两个不相交的字段)=====

/// 动作成功且非 dry-run 后重读配置,失败则把错误顶到 footer。
fn after_action(t: &mut DeviceTab, ok: bool, dry_run: bool, message: &mut String) {
    if ok && !dry_run && let Some(err) = t.load_config() {
        *message = format!("read failed: {err}");
    }
}

/// pop 框独占按键:字段间跳转 + 输入,最后一个字段回车提交。
fn popup_key(t: &mut DeviceTab, key: KeyEvent, dry_run: bool, message: &mut String) {
    let Some(popup) = t.popup.as_mut() else {
        unreachable!()
    };
    match key.code {
        KeyCode::Esc => {
            t.popup = None;
            *message = "cancelled".into();
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
                let vals: Vec<String> = state.inputs.iter().map(|i| i.buf.clone()).collect();
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
                *message = format!("{title}: {}", describe(&res, dry_run));
                after_action(t, res.is_ok(), dry_run, message);
            }
        }
        KeyCode::Char(c) => popup.inputs[popup.cur].insert(c),
        _ => {}
    }
}

/// 行内编辑独占按键:输入 + 回车提交 / Esc 取消。
fn editing_key(t: &mut DeviceTab, key: KeyEvent, dry_run: bool, message: &mut String) {
    let Some((_, input)) = t.editing.as_mut() else {
        unreachable!()
    };
    match key.code {
        KeyCode::Esc => {
            t.editing = None;
            *message = "edit cancelled".into();
        }
        KeyCode::Enter => {
            let (idx, val) = t.editing.take().unwrap();
            let field = match &t.rows[idx].action {
                RowAction::Edit(f) => *f,
                _ => unreachable!(),
            };
            let label = t.rows[idx].label.clone();
            let res = t.do_field(field, &val.buf);
            *message = format!("{label}: {}", describe(&res, dry_run));
            after_action(t, res.is_ok(), dry_run, message);
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
}

/// r 键重读设备配置。
fn refresh_device(t: &mut DeviceTab, message: &mut String) {
    if let Some(err) = t.load_config() {
        *message = format!("read failed: {err}");
    }
}

/// d 键删除选中的 WiFi 条目。
fn wifi_remove(t: &mut DeviceTab, dry_run: bool, message: &mut String) {
    if let RowAction::WifiEntry { ssid } = &t.rows[t.sel].action {
        let ssid = ssid.clone();
        let res = t.do_wifi_remove(&ssid);
        *message = format!("wifi {ssid}: {}", describe(&res, dry_run));
        after_action(t, res.is_ok(), dry_run, message);
    }
}

/// 设备页回车:分发选中行的动作。行是纯数据,动作在这里落到 `do_*` 方法。
fn row_action(t: &mut DeviceTab, dry_run: bool, message: &mut String) {
    // 克隆动作,免得借用 rows 卡住后面的 &mut 方法。
    let action = t.rows[t.sel].action.clone();
    match action {
        RowAction::AsrClear | RowAction::ResetDevice => {
            let label = t.rows[t.sel].label.clone();
            let res = match action {
                RowAction::AsrClear => t.do_asr_clear(),
                _ => t.do_reset(),
            };
            *message = format!("{label}: {}", describe(&res, dry_run));
            after_action(t, res.is_ok(), dry_run, message);
        }
        RowAction::Edit(_) => {
            // 预填当前值:方便只改一部分。
            let value = t.rows[t.sel].value.clone();
            t.editing = Some((t.sel, Input::with_str(&value)));
            *message = "editing — Enter save · Esc cancel".into();
        }
        RowAction::Cycle { field, values } => {
            // 枚举选项:回车直接切到下一个值。
            let current = t.rows[t.sel].value.trim().to_lowercase();
            let next = next_in_cycle(&values, &current);
            let label = t.rows[t.sel].label.clone();
            let res = t.do_field(field, next);
            *message = format!("{label}: {}", describe(&res, dry_run));
            after_action(t, res.is_ok(), dry_run, message);
        }
        RowAction::Popup(kind) => {
            t.popup = Some(PopupState::new(kind, Vec::new()));
            *message = "Tab/↑↓ field · Enter save · Esc cancel".into();
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
            t.popup = Some(PopupState::new(PopupKind::WifiPass { ssid }, vec![pass]));
            *message = "Enter save · Esc cancel".into();
        }
    }
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
        Tab::Pkgs => "packages — Enter install · u update · U all · p prerelease".into(),
    }
}
