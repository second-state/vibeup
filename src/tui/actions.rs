//! 业务动作与轮询:自由函数,操作 `(&mut DeviceState, &mut DeviceRuntime)` 或
//! `(&mut PkgState, &mut PkgRuntime)`。阻塞动作(下载、fetch)不在这里同步跑:
//! 要么丢后台线程(Job/PkgJob),要么由调用方 `tokio::spawn` + spawn_blocking。

use crate::ble::{self, DeviceKind, FoundDevice, SessionEvent, Target};
use btleplug::api::{Central as _, Manager as _, Peripheral as _};
use crate::config_cmd;
use crate::install;
use crate::pkg::{self, Pkg, PKGS, SELF};
use anyhow::{bail, Result};
use ratatui_kit::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::path::Path;
use std::sync::atomic::Ordering;

use super::demo;
use super::state::{
    Conn, DeviceRuntime, DeviceState, Entry, Field, next_in_cycle, KEYMAP_KEYS, keymap_binding, PkgMenu, PkgRuntime,
    PkgState, PopupKind, PopupState, RowAction, SharedProgress, DownloadProgress, PKG_MENU_ITEMS,
};

// ===== 后台任务基建(线程 + channel)=====

/// 后台 BLE 任务:扫描 2 秒上下,放线程里跑,结果经 channel 回来。
pub struct Job {
    pub done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub rx: std::sync::mpsc::Receiver<JobOut>,
}

pub enum JobOut {
    Scan(anyhow::Result<Vec<FoundDevice>>),
    Config(anyhow::Result<serde_json::Value>),
}

pub fn spawn_job(f: impl FnOnce() -> JobOut + Send + 'static) -> Job {
    let (tx, rx) = std::sync::mpsc::channel();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = done.clone();
    std::thread::spawn(move || {
        let _ = tx.send(f());
        flag.store(true, Ordering::Relaxed);
    });
    Job { done, rx }
}

/// 后台包任务(下载几十 MB 要几十秒)。progress 与线程共享,轮询侧读。
pub struct PkgJob {
    pub done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub rx: std::sync::mpsc::Receiver<PkgJobOut>,
    pub progress: SharedProgress,
}

pub enum PkgJobOut {
    /// 动作结果 + 刷新后的包表(线程里 fetch,UI 线程零阻塞)。
    Finished(anyhow::Result<()>, Vec<Entry>),
    /// plugin 安装结果行(Claude Code plugin installed / … failed)。
    PluginsDone(Vec<String>),
}

impl PkgJob {
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }

    pub fn try_recv(&self) -> Option<PkgJobOut> {
        self.rx.try_recv().ok()
    }
}

pub fn spawn_pkg_job(
    progress: SharedProgress,
    f: impl FnOnce() -> PkgJobOut + Send + 'static,
) -> PkgJob {
    let (tx, rx) = std::sync::mpsc::channel();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = done.clone();
    std::thread::spawn(move || {
        let _ = tx.send(f());
        flag.store(true, Ordering::Relaxed);
    });
    PkgJob { done, rx, progress }
}

/// 阻塞执行一个设备动作(dispatch)。返回 (结果, 新配置)。
fn no_device_error(kind: DeviceKind, seen: &[String]) -> anyhow::Error {
    if seen.is_empty() {
        anyhow::anyhow!(
            "no {} in provisioning mode — put it there first ({})",
            kind.name(),
            match kind {
                DeviceKind::VibeKeys => "boot into Keyboard mode, press ACCEPT to re-advertise",
                DeviceKind::Watch => "Setting → Enable BLE",
            }
        )
    } else {
        anyhow::anyhow!(
            "found other device(s) but no {}: {} — check the device and retry",
            kind.name(),
            seen.join(", ")
        )
    }
}

/// 扫描并列出全部匹配设备。只认真在广播/已连接的设备——BlueZ 会把连过一次的
/// 设备永久缓存,关机的幽灵也在 peripherals() 列表里。活性规则:已连接(开机
/// 自动重连)或 rssi 有值(正在广播),两者皆无即缓存幽灵。
async fn collect_scan(
    adapter: &btleplug::platform::Adapter,
    kind: DeviceKind,
) -> anyhow::Result<(Vec<FoundDevice>, Vec<String>)> {
    let mut filter = btleplug::api::ScanFilter::default();
    filter.services.push(ble::SERVICE_ID);
    let mut seen: Vec<String> = Vec::new();
    let mut found: Vec<FoundDevice> = Vec::new();
    adapter.start_scan(filter.clone()).await?;
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let peripherals = adapter.peripherals().await?;
        if peripherals.is_empty() {
            continue;
        }
        let _ = adapter.stop_scan().await;
        for p in &peripherals {
            let connected = p.is_connected().await.unwrap_or(false);
            let rssi = p.properties().await.ok().flatten().and_then(|pr| pr.rssi);
            if !connected && rssi.is_none() {
                continue;
            }
            let Ok(Some(props)) = p.properties().await else {
                continue;
            };
            if !props.services.contains(&ble::SERVICE_ID) {
                continue;
            }
            let name = props.local_name.clone().unwrap_or_default();
            if name.starts_with(kind.name_prefix()) {
                let id = p.id().to_string();
                if !found.iter().any(|f| f.id == id) {
                    found.push(FoundDevice { name, id });
                }
            } else if !name.is_empty() && !seen.contains(&name) {
                seen.push(name);
            }
        }
        if !found.is_empty() {
            break;
        }
        adapter.start_scan(filter.clone()).await?;
    }
    let _ = adapter.stop_scan().await;
    Ok((found, seen))
}

/// 后台线程体:真实扫描(自带 runtime)。
pub(crate) fn scan_devices_thread(kind: DeviceKind) -> anyhow::Result<Vec<FoundDevice>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let manager = btleplug::platform::Manager::new().await?;
        let adapter = manager
            .adapters()
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("No Bluetooth adapter"))?;
        let (found, seen) = collect_scan(&adapter, kind).await?;
        if found.is_empty() {
            return Err(no_device_error(kind, &seen));
        }
        Ok(found)
    })
}

fn describe<T>(res: &Result<T>, dry_run: bool) -> String {
    match res {
        Ok(_) if dry_run => "dry-run ok".into(),
        Ok(_) => "done".into(),
        Err(e) => format!("{e:#}"),
    }
}

// ===== 包页动作 =====

/// 从注册表构建全新的包条目并刷新(阻塞网络,后台线程调用)。
fn fresh_entries(dir: &Path) -> Vec<Entry> {
    let mut entries: Vec<Entry> = std::iter::once(Entry::new(&SELF))
        .chain(PKGS.iter().map(Entry::new))
        .collect();
    super::state::fetch_entries(&mut entries, dir);
    entries
}

/// 后台执行勾选的 plugin 安装(marketplace add + plugin install)。
/// 结果行经 PluginsDone 回到 footer。
fn start_plugin_install(
    state: &mut PkgState,
    rt: &mut PkgRuntime,
    picks: Vec<(&'static str, &'static str, &'static str)>,
) {
    state.job_label = "installing plugins".into();
    state.running = true;
    state.progress = None;
    rt.job = Some(spawn_pkg_job(
        std::sync::Arc::new(std::sync::Mutex::new(DownloadProgress::default())),
        move || {
            let results: Vec<String> = picks
                .iter()
                .map(|(cli, action, label)| {
                    install_plugin_cli(cli, action, label)
                })
                .collect();
            PkgJobOut::PluginsDone(results)
        },
    ));
}

/// 装 (cli, label) 的 agent plugin:marketplace add + plugin install。
/// 返回总结行(footer 显示)。
fn install_plugin_cli(cli: &str, action: &str, label: &str) -> String {
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

/// 安装/更新 `entries[idx]`。已最新直接写 footer;真要下载丢后台线程。
pub fn install_pkg(
    state: &mut PkgState,
    rt: &mut PkgRuntime,
    dir: &Path,
    dry_run: bool,
    idx: usize,
    pre: bool,
) {
    let Some(e) = state.entries.get(idx) else {
        return;
    };
    let target = e.target(pre);
    let pkg = e.pkg;
    if let Some(t) = &target
        && let Some(v) = pkg::installed_version(pkg, dir)
        && pkg::normalize(&v) == pkg::normalize(t)
    {
        state.message = format!("{} {v}: up to date", pkg.name);
        return;
    }
    let (dir2, dry2) = (dir.to_path_buf(), dry_run);
    state.job_label = format!("installing {}", pkg.name);
    state.running = true;
    state.vibekeys_pending = pkg.name == "vibekeys";
    state.progress = None;
    let progress: SharedProgress = std::sync::Arc::new(std::sync::Mutex::new(DownloadProgress::default()));
    let tick_progress = progress.clone();
    let job_progress = progress.clone();
    rt.job = Some(spawn_pkg_job(job_progress, move || {
        let tick = move |d: u64, t: Option<u64>| {
            *tick_progress.lock().unwrap() = DownloadProgress {
                downloaded: d,
                total: t,
            };
        };
        let res = install::install(pkg, target.clone(), pre, &dir2, dry2, false, &tick);
        let entries = fresh_entries(&dir2);
        PkgJobOut::Finished(res.map(|_| ()), entries)
    }));
    rt.progress = progress;
    state.message = format!("{}: installing…", pkg.name);
}

/// 卸载 `entries[idx]`:纯本地文件操作,瞬时完成。
pub fn uninstall_pkg(state: &mut PkgState, dir: &Path, dry_run: bool, idx: usize) {
    let Some(e) = state.entries.get(idx) else {
        return;
    };
    let pkg = e.pkg;
    if pkg::installed_version(pkg, dir).is_none() {
        state.message = format!("{}: not installed", pkg.name);
        return;
    }
    let res = install::uninstall(pkg, dir, dry_run);
    state.message = format!("{}: {}", pkg.name, describe(&res, dry_run));
    // 包表刷新走 ticker 的 spawn_blocking——这里在 tokio UI 线程上,
    // 直接调 reqwest blocking 会在 async 上下文里 drop 它的内部
    // runtime,触发 "Cannot drop a runtime" panic。
    state.refresh_pending = true;
}

/// 全部更新(后台线程)。
pub fn update_all(state: &mut PkgState, rt: &mut PkgRuntime, dir: &Path, dry_run: bool, pre: bool) {
    let all: Vec<(&Pkg, Option<String>)> = state
        .entries
        .iter()
        .map(|e| (e.pkg, e.target(pre)))
        .collect();
    let all_fresh = all.iter().all(|(p, t)| match t {
        Some(t) => matches!(
            pkg::installed_version(p, dir),
            Some(v) if pkg::normalize(&v) == pkg::normalize(t)
        ),
        None => false,
    });
    if all_fresh {
        state.message = "all packages up to date".into();
        return;
    }
    let (dir2, dry2) = (dir.to_path_buf(), dry_run);
    state.job_label = "update all".into();
    state.running = true;
    state.progress = None;
    let progress: SharedProgress = std::sync::Arc::new(std::sync::Mutex::new(DownloadProgress::default()));
    let tick_progress = progress.clone();
    let job_progress = progress.clone();
    rt.job = Some(spawn_pkg_job(job_progress, move || {
        let tick = move |d: u64, t: Option<u64>| {
            *tick_progress.lock().unwrap() = DownloadProgress {
                downloaded: d,
                total: t,
            };
        };
        let res = all
            .iter()
            .map(|&(p, ref t)| install::install(p, t.clone(), pre, &dir2, dry2, false, &tick))
            .collect::<Result<Vec<_>>>()
            .map(|_| ());
        let entries = fresh_entries(&dir2);
        PkgJobOut::Finished(res, entries)
    }));
    rt.progress = progress;
    state.message = "update all: running…".into();
}

/// 本机是否有该 CLI。
fn cli_on_path(cli: &str) -> bool {
    std::process::Command::new(cli)
        .arg("--version")
        .output()
        .is_ok()
}

/// 轮询包任务;完成则写 footer 消息并换上新表。
pub fn poll_pkg_job(
    state: &mut PkgState,
    rt: &mut PkgRuntime,
    dir: &Path,
    dry_run: bool,
) -> Option<String> {
    let job = rt.job.as_ref()?;
    if !job.is_done() {
        // 进度快照刷新 + 转圈。
        if let Ok(p) = job.progress.lock() {
            state.progress = Some((p.downloaded, p.total));
        }
        state.spin = state.spin.wrapping_add(1);
        return None;
    }
    let out = job.try_recv();
    rt.job = None;
    rt.progress = std::sync::Arc::new(std::sync::Mutex::new(super::state::DownloadProgress::default()));
    state.running = false;
    state.progress = None;
    match out {
        Some(PkgJobOut::Finished(res, entries)) => {
            state.entries = entries;
            state.sel = state.sel.min(state.entries.len().saturating_sub(1));
            let mut msg = format!("{}: {}", state.job_label, describe(&res, dry_run));
            // vibekeys 装完:自动跑 install-script,并按本机 CLI 情况弹 plugin 询问。
            if state.vibekeys_pending {
                state.vibekeys_pending = false;
                if res.is_ok() && !dry_run {
                    let bin = dir.join("vibekeys");
                    match std::process::Command::new(&bin)
                        .arg("install-script")
                        .output()
                    {
                        Ok(o) if o.status.success() => {
                            msg.push_str(" · jump script installed");
                        }
                        Ok(o) => msg.push_str(&format!(
                            " · install-script failed: {}",
                            String::from_utf8_lossy(&o.stderr).trim()
                        )),
                        Err(e) => msg.push_str(&format!(" · install-script failed: {e}")),
                    }
                    let mut clis: Vec<(&'static str, &'static str, &'static str)> = Vec::new();
                    if cli_on_path("claude") {
                        clis.push(("claude", "install", "Claude Code"));
                    }
                    if cli_on_path("codex") {
                        clis.push(("codex", "add", "Codex"));
                    }
                    if !clis.is_empty() {
                        let n = clis.len();
                        state.plugin_ask = Some(super::state::PluginAsk {
                            clis,
                            checked: vec![true; n],
                            sel: 0,
                        });
                    }
                }
            }
            Some(msg)
        }
        Some(PkgJobOut::PluginsDone(lines)) => {
            Some(format!("{}: {}", state.job_label, lines.join(" · ")))
        }
        _ => Some(format!("{}: background task died", state.job_label)),
    }
}

// ===== 设备页动作 =====

/// 进入设备页:先弹入口菜单,不自动扫描。
pub fn device_enter(state: &mut DeviceState) {
    if state.conn == Conn::Fresh {
        state.conn = Conn::Menu;
    }
}

/// 网页配置:切到地址弹窗。
pub fn show_web_setup(state: &mut DeviceState) {
    state.web_url = Some(state.kind.setup_url());
    state.conn = Conn::WebSetup;
}

pub fn start_scan(state: &mut DeviceState, rt: &mut DeviceRuntime, demo: bool) {
    rt.job = None;
    state.conn = Conn::Scanning;
    let kind = state.kind;
    rt.job = Some(match demo {
        true => demo::demo_scan_job(kind),
        false => spawn_job(move || JobOut::Scan(scan_devices_thread(kind))),
    });
}

fn start_connect(state: &mut DeviceState, rt: &mut DeviceRuntime, demo: bool, dev: FoundDevice) {
    rt.job = None;
    drop(rt.session.take());
    state.pending_name = Some(dev.name.clone());
    let target = Target::picked(state.kind, dev.id.clone());
    rt.pending_target = Some(target.clone());
    state.conn = Conn::Connecting(dev.name);
    let kind = state.kind;
    match demo {
        // demo:假任务 0.4s 后回样例配置。
        true => rt.job = Some(demo::demo_connect_job(kind)),
        // 真机:常驻会话,Ready/Disconnected 事件驱动。
        false => rt.session = Some(ble::Session::open(target)),
    }
    let _ = kind;
}

pub fn pick_connect(state: &mut DeviceState, rt: &mut DeviceRuntime, demo: bool) {
    if let Some(dev) = state.pick.get(state.pick_sel).cloned() {
        start_connect(state, rt, demo, dev);
    }
}

/// 轮询后台扫描任务。
pub fn poll_job(state: &mut DeviceState, rt: &mut DeviceRuntime) -> Option<String> {
    let job = rt.job.as_ref()?;
    if !job.done.load(Ordering::Relaxed) {
        return None;
    }
    let job = rt.job.take()?;
    match job.rx.try_recv().ok() {
        Some(JobOut::Scan(res)) => Some(on_scan_result(state, res)),
        Some(JobOut::Config(res)) => Some(on_connect_result(state, rt, res)),
        None => {
            state.conn = Conn::Failed("background task died".into());
            Some("background task died".into())
        }
    }
}

fn on_scan_result(state: &mut DeviceState, res: anyhow::Result<Vec<FoundDevice>>) -> String {
    match res {
        Ok(devs) if !devs.is_empty() => {
            state.pick = devs;
            state.pick_sel = 0;
            state.conn = Conn::Pick;
            format!("found {} device(s) — pick one", state.pick.len())
        }
        Ok(_) => {
            state.conn = Conn::Failed("no devices found".into());
            "no devices found".into()
        }
        Err(e) => {
            let msg = format!("{e:#}");
            state.conn = Conn::Failed(msg.clone());
            msg
        }
    }
}

fn on_connect_result(
    state: &mut DeviceState,
    rt: &mut DeviceRuntime,
    res: anyhow::Result<serde_json::Value>,
) -> String {
    match res {
        Ok(cfg) => {
            state.config = Some(cfg);
            state.connected_name = Some(
                state
                    .pending_name
                    .take()
                    .unwrap_or_else(|| state.kind.name().into()),
            );
            rt.pending_target = None;
            state.rebuild_rows();
            state.conn = Conn::Ready;
            "connected".into()
        }
        Err(e) => {
            let msg = format!("{e:#}");
            state.conn = Conn::Failed(msg.clone());
            msg
        }
    }
}

/// 轮询常驻会话事件;断连按阶段分流(Ready → 弹窗,连接中 → 失败屏)。
pub fn poll_session(state: &mut DeviceState, rt: &mut DeviceRuntime) -> Option<String> {
    let ev = rt.session.as_ref()?.try_event()?;
    Some(on_session_event(state, rt, ev))
}

fn on_session_event(
    state: &mut DeviceState,
    rt: &mut DeviceRuntime,
    ev: SessionEvent,
) -> String {
    match ev {
        SessionEvent::Ready(res) => on_connect_result(state, rt, res),
        SessionEvent::Disconnected(e) => {
            drop(rt.session.take());
            if state.conn == Conn::Ready {
                state.editing = None;
                state.popup = None;
                state.conn = Conn::Disconnected;
                format!("device disconnected: {e:#}")
            } else {
                on_connect_result(state, rt, Err(e))
            }
        }
    }
}

/// 重读配置(r 键、动作成功后)。demo 读假数据;真机走常驻会话(瞬时)。
/// 失败保留旧快照;断连由会话事件驱动,不走这里。
pub fn load_config(demo: bool, state: &mut DeviceState, rt: &mut DeviceRuntime) -> Option<String> {
    if demo {
        state.config = Some(rt.mock.clone());
        state.rebuild_rows();
        return None;
    }
    let session = rt.session.as_ref()?;
    match session.read_config() {
        Ok(cfg) => {
            state.config = Some(cfg);
            state.rebuild_rows();
            None
        }
        Err(e) => Some(format!("{e:#}")),
    }
}

/// 写单个键位绑定(demo 落假配置,真机走常驻会话)。
pub fn do_keymap(demo: bool, rt: &mut DeviceRuntime, key: &str, binding: &str) -> Result<()> {
    if demo {
        demo::mock_keymap(&mut rt.mock, key, binding);
        return Ok(());
    }
    let payload = config_cmd::build_keymap_config(key, binding);
    rt.session
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("device session is closed"))?
        .write_keymap(&payload)
}

/// keymap 弹窗按键。返回 Some(message) = 已消费。
pub fn keymap_popup_key(
    state: &mut DeviceState,
    rt: &mut DeviceRuntime,
    demo: bool,
    key: &KeyEvent,
) -> Option<String> {
    let (mut sel, editing) = {
        let km = state.keymap_popup.as_ref()?;
        (km.sel, km.editing.clone())
    };

    // 编辑模式:输入/保存/取消。
    if let Some((idx, input)) = &editing {
        std::fs::write("/tmp/tui_key.log", format!("{key:?}\n")).ok();
        match key.code {
            KeyCode::Esc => {
                if let Some(km) = state.keymap_popup.as_mut() {
                    km.editing = None;
                }
                return Some("edit cancelled".into());
            }
            KeyCode::Enter => {
                let binding = input.buf.trim().to_string();
                if binding.is_empty() {
                    return Some(
                        "binding must not be empty — type one or Esc to cancel".into(),
                    );
                }
                let keyname = KEYMAP_KEYS[*idx];
                let res = do_keymap(demo, rt, keyname, &binding);
                if let Some(km) = state.keymap_popup.as_mut() {
                    km.editing = None;
                }
                let mut m = format!("{keyname}: {}", describe(&res, demo));
                if res.is_ok()
                    && let Some(err) = load_config(demo, state, rt)
                {
                    m.push_str(&format!(" · read failed: {err}"));
                }
                return Some(m);
            }
            KeyCode::Backspace => {
                if let Some(km) = state.keymap_popup.as_mut()
                    && let Some((_, input)) = km.editing.as_mut() {
                        input.backspace();
                    }
                return None;
            }
            KeyCode::Delete => {
                if let Some(km) = state.keymap_popup.as_mut()
                    && let Some((_, input)) = km.editing.as_mut() {
                        input.delete();
                    }
                return None;
            }
            KeyCode::Left => {
                if let Some(km) = state.keymap_popup.as_mut()
                    && let Some((_, input)) = km.editing.as_mut() {
                        input.left();
                    }
                return None;
            }
            KeyCode::Right => {
                if let Some(km) = state.keymap_popup.as_mut()
                    && let Some((_, input)) = km.editing.as_mut() {
                        input.right();
                    }
                return None;
            }
            KeyCode::Home => {
                if let Some(km) = state.keymap_popup.as_mut()
                    && let Some((_, input)) = km.editing.as_mut() {
                        input.home();
                    }
                return None;
            }
            KeyCode::End => {
                if let Some(km) = state.keymap_popup.as_mut()
                    && let Some((_, input)) = km.editing.as_mut() {
                        input.end();
                    }
                return None;
            }
            KeyCode::Char(c) => {
                // 键位绑定只要 ASCII:中文等字符直接忽略。
                if !c.is_ascii() {
                    return None;
                }
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                let alt = key.modifiers.contains(KeyModifiers::ALT);
                if ctrl || alt {
                    // 修饰键组合:捕捉按键本身,整体替换输入(Ctrl+C → "Ctrl+C")。
                    let mut combo = String::new();
                    if ctrl {
                        combo.push_str("Ctrl+");
                    }
                    if alt {
                        combo.push_str("Alt+");
                    }
                    if key.modifiers.contains(KeyModifiers::SHIFT) {
                        combo.push_str("Shift+");
                    }
                    combo.push(c.to_ascii_uppercase());
                    if let Some(km) = state.keymap_popup.as_mut()
                        && let Some((_, input)) = km.editing.as_mut()
                    {
                        input.buf = combo;
                        input.pos = input.buf.chars().count();
                    }
                    return None;
                }
                // 无 Ctrl/Alt(含单独 Shift)= 普通打字。
                if let Some(km) = state.keymap_popup.as_mut()
                    && let Some((_, input)) = km.editing.as_mut()
                {
                    input.insert(c);
                }
                return None;
            }
            _ => return None,
        }
    }

    // 浏览模式:导航 / 进入编辑 / 关闭。
    match key.code {
        KeyCode::Esc => {
            state.keymap_popup = None;
            Some("keymap closed".into())
        }
        KeyCode::Char('j') | KeyCode::Down => {
            sel = (sel + 1).min(KEYMAP_KEYS.len() - 1);
            if let Some(km) = state.keymap_popup.as_mut() {
                km.sel = sel;
            }
            None
        }
        KeyCode::Char('k') | KeyCode::Up => {
            sel = sel.saturating_sub(1);
            if let Some(km) = state.keymap_popup.as_mut() {
                km.sel = sel;
            }
            None
        }
        KeyCode::Enter => {
            let keyname = KEYMAP_KEYS[sel];
            let current = keymap_binding(
                state.config.as_ref().unwrap_or(&serde_json::Value::Null),
                keyname,
            );
            if let Some(km) = state.keymap_popup.as_mut() {
                km.editing = Some((sel, super::state::Input::with_str(&current)));
            }
            None
        }
        _ => None,
    }
}

/// 动作成功后的善后:重读配置,失败顶 footer。
fn after_action(demo: bool, state: &mut DeviceState, rt: &mut DeviceRuntime, ok: bool, message: &mut String) {
    if ok
        && let Some(err) = load_config(demo, state, rt)
    {
        *message = format!("read failed: {err}");
    }
}

// ===== 设备动作(demo 落假配置,真机走常驻会话)=====

fn session_of(rt: &DeviceRuntime) -> Result<&ble::Session> {
    rt.session
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("device session is closed"))
}

pub fn do_field(
    demo: bool,
    rt: &mut DeviceRuntime,
    field: Field,
    val: &str,
) -> Result<()> {
    let val = val.trim();
    let write = |rt: &mut DeviceRuntime, f: &str, v: serde_json::Value| -> Result<()> {
        session_of(rt)?.write_config_field(f, v)
    };
    match field {
        Field::ServerUrl => {
            if demo {
                rt.mock["server_url"] = val.into();
                return Ok(());
            }
            write(rt, "server_url", val.into())
        }
        Field::AsrUri => {
            if demo {
                demo::mock_asr_set(&mut rt.mock, Some(val), None, None);
                return Ok(());
            }
            write(
                rt,
                "asr_config",
                serde_json::json!({
                    "platform": "whisper",
                    "uri": val,
                    "api_key": serde_json::Value::Null,
                    "model": serde_json::Value::Null,
                }),
            )
        }
        Field::AsrApiKey => {
            if demo {
                demo::mock_asr_set(&mut rt.mock, None, Some(val), None);
                return Ok(());
            }
            write(
                rt,
                "asr_config",
                serde_json::json!({
                    "platform": "whisper",
                    "uri": serde_json::Value::Null,
                    "api_key": val,
                    "model": serde_json::Value::Null,
                }),
            )
        }
        Field::AsrModel => {
            if demo {
                demo::mock_asr_set(&mut rt.mock, None, None, Some(val));
                return Ok(());
            }
            write(
                rt,
                "asr_config",
                serde_json::json!({
                    "platform": "whisper",
                    "uri": serde_json::Value::Null,
                    "api_key": serde_json::Value::Null,
                    "model": val,
                }),
            )
        }
        Field::MicModel => {
            let v = match val.to_lowercase().as_str() {
                "ptt" => 0,
                "toggle" => 1,
                other => bail!("invalid mic mode `{other}` — use ptt or toggle"),
            };
            if demo {
                rt.mock["mic_model"] = serde_json::json!(v);
                return Ok(());
            }
            write(rt, "mic_model", serde_json::json!(v))
        }
        Field::PreferAsr => {
            let on = match val.to_lowercase().as_str() {
                "on" | "true" => true,
                "off" | "false" => false,
                other => bail!("invalid value `{other}` — use on or off"),
            };
            if demo {
                rt.mock["prefer_builtin_asr"] = serde_json::json!(on);
                return Ok(());
            }
            write(rt, "prefer_builtin_asr", serde_json::json!(on))
        }
        Field::Tailscale => {
            if demo {
                rt.mock["tailscale_key"] = val.into();
                return Ok(());
            }
            if val.is_empty() {
                bail!("empty tailscale auth key");
            }
            // 手表上这条写是独占的:固件存完立刻跳 Tailscale 页、断连 ——
            // 断连会以 Disconnected 事件到达,弹窗是预期表现。
            write(rt, "tailscale_key", serde_json::Value::String(val.into()))
        }
    }
}

/// WiFi 增改(同 ssid = 更新密码):demo 落假配置,真机读-改-写整个列表。
pub fn do_wifi_add(
    demo: bool,
    rt: &mut DeviceRuntime,
    ssid: &str,
    pass: &str,
) -> Result<()> {
    if ssid.trim().is_empty() {
        bail!("ssid must not be empty");
    }
    if demo {
        demo::mock_wifi_add(&mut rt.mock, ssid.trim(), pass);
        return Ok(());
    }
    let session = session_of(rt)?;
    let list = ble::parse_wifi_list(&serde_json::to_string(&session.read_config()?)?);
    let mut list = list;
    match list.iter_mut().find(|(s, _)| *s == ssid.trim()) {
        Some(e) => e.1 = pass.to_string(),
        None => list.push((ssid.trim().to_string(), pass.to_string())),
    }
    if list.len() > ble::MAX_WIFI_CREDS {
        bail!(
            "wifi list is full ({}/{}): remove one first",
            list.len(),
            ble::MAX_WIFI_CREDS
        );
    }
    session.write_config_field("wifi_list", ble::wifi_list_to_json(&list))
}

/// WiFi 删除。
pub fn do_wifi_remove(demo: bool, rt: &mut DeviceRuntime, ssid: &str) -> Result<()> {
    if demo {
        demo::mock_wifi_remove(&mut rt.mock, ssid);
        return Ok(());
    }
    let session = session_of(rt)?;
    let list = ble::parse_wifi_list(&serde_json::to_string(&session.read_config()?)?);
    let rest: Vec<_> = list.iter().filter(|(s, _)| *s != ssid).cloned().collect();
    if rest.len() == list.len() {
        bail!("wifi `{ssid}' not in the list");
    }
    session.write_config_field("wifi_list", ble::wifi_list_to_json(&rest))
}

/// 清空 ASR 配置(与 vibekeys_app `asr-config --clean` 一致:全空串)。
pub fn do_asr_clear(demo: bool, rt: &mut DeviceRuntime) -> Result<()> {
    if demo {
        rt.mock["asr_config"] =
            serde_json::json!({"platform": "whisper", "uri": "", "api_key": "", "model": ""});
        return Ok(());
    }
    session_of(rt)?.write_config_field(
        "asr_config",
        serde_json::json!({"platform": "whisper", "uri": "", "api_key": "", "model": ""}),
    )
}

/// 设备重启使已存配置生效。重启会掐断连接,断连事件随后到 → 弹窗。
pub fn do_reset(demo: bool, rt: &mut DeviceRuntime) -> Result<()> {
    if demo {
        return Ok(());
    }
    session_of(rt)?.reset()
}

// ===== 按键分发(优先级:退出 > pop 框 > 行内编辑 > 连接流程 > 全局)=====

/// 一次按键的后果。
pub struct KeyOutcome {
    pub message: Option<String>,
    pub exit: bool,
}

impl KeyOutcome {
    fn none() -> Self {
        Self {
            message: None,
            exit: false,
        }
    }
    fn msg(m: String) -> Self {
        Self {
            message: (!m.is_empty()).then_some(m),
            exit: false,
        }
    }
}

/// 全部按键逻辑。与旧版同一个 if 链,语义一致;纯本地/会话写内联完成,
/// 网络动作(下载)丢后台线程,包表刷新通过 `refresh` 交调用方异步做。
#[allow(clippy::too_many_arguments)]
pub fn handle_key(
    ev: Event,
    mut tab: ratatui_kit::State<super::state::Tab>,
    allow_pre: &mut bool,
    keys: &mut DeviceState,
    keys_rt: &mut DeviceRuntime,
    watch: &mut DeviceState,
    watch_rt: &mut DeviceRuntime,
    pkg: &mut PkgState,
    pkg_rt: &mut PkgRuntime,
    dir: &Path,
    dry_run: bool,
    demo: bool,
) -> KeyOutcome {
    let Event::Key(key) = ev else {
        return KeyOutcome::none();
    };
    // 支持长按:终端按住会连发 Press(老协议)或 Repeat(kitty 协议),都收。
    if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
        return KeyOutcome::none();
    }
    let code = key.code;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // 退出优先(任何状态都可退)。
    if code == KeyCode::Char('q') || (code == KeyCode::Char('c') && ctrl) {
        return KeyOutcome {
            message: None,
            exit: true,
        };
    }

    let mut message = String::new();

    // ---- 设备页 ----
    if tab.get() != super::state::Tab::Packages {
        let (state, rt): (&mut DeviceState, &mut DeviceRuntime) =
            if tab.get() == super::state::Tab::Keyboard {
                (keys, keys_rt)
            } else {
                (watch, watch_rt)
            };
        // pop 框独占按键(Tab 仍可切标签)。
        if state.popup.is_some() {
            if code == KeyCode::Tab {
                switch_tab(&mut tab, keys, watch, &mut message);
                return KeyOutcome::msg(std::mem::take(&mut message));
            }
            let state_demo = state.demo;
            if let Some(m) = popup_key(state, rt, state_demo, &key) {
                return KeyOutcome::msg(m);
            }
            return KeyOutcome::msg(std::mem::take(&mut message));
        }
        // keymap 弹窗独占按键(Tab 仍可切标签)。
        if state.keymap_popup.is_some() {
            if code == KeyCode::Tab {
                switch_tab(&mut tab, keys, watch, &mut message);
                return KeyOutcome::msg(std::mem::take(&mut message));
            }
            let state_demo = state.demo;
            if let Some(m) = keymap_popup_key(state, rt, state_demo, &key) {
                return KeyOutcome::msg(m);
            }
            return KeyOutcome::msg(std::mem::take(&mut message));
        }
        // 行内编辑独占按键(Tab 仍可切标签)。
        if state.editing.is_some() {
            if code == KeyCode::Tab {
                switch_tab(&mut tab, keys, watch, &mut message);
                return KeyOutcome::msg(std::mem::take(&mut message));
            }
            let state_demo = state.demo;
            if let Some(m) = editing_key(state, rt, state_demo, &key) {
                return KeyOutcome::msg(m);
            }
            return KeyOutcome::msg(std::mem::take(&mut message));
        }
        // 连接流程(未就绪)独占按键;Tab 切标签保持可用。
        if state.conn != Conn::Ready {
            if code == KeyCode::Tab {
                switch_tab(&mut tab, keys, watch, &mut message);
                return KeyOutcome::msg(std::mem::take(&mut message));
            }
            let demo = state.demo;
            device_flow_key(state, rt, demo, code, &mut message);
            return KeyOutcome::msg(std::mem::take(&mut message));
        }
        // 就绪页:切标签键优先(与旧版全局键一致),然后行导航/动作。
        match code {
            KeyCode::Char('1') => {
                switch_to(&mut tab, super::state::Tab::Keyboard, keys, watch);
                return KeyOutcome::msg(tab_message(super::state::Tab::Keyboard));
            }
            KeyCode::Char('2') => {
                switch_to(&mut tab, super::state::Tab::Watch, keys, watch);
                return KeyOutcome::msg(tab_message(super::state::Tab::Watch));
            }
            KeyCode::Char('3') => {
                switch_to(&mut tab, super::state::Tab::Packages, keys, watch);
                return KeyOutcome::msg(tab_message(super::state::Tab::Packages));
            }
            KeyCode::Tab => {
                let next = tab.get().next();
                switch_to(&mut tab, next, keys, watch);
                return KeyOutcome::msg(tab_message(next));
            }
            _ => {}
        }
        match code {
            KeyCode::Char('j') | KeyCode::Down => {
                state.sel = (state.sel + 1).min(state.rows.len() - 1);
            }
            KeyCode::Char('k') | KeyCode::Up => state.sel = state.sel.saturating_sub(1),
            KeyCode::Char('d') => wifi_remove_action(state, rt, demo, &mut message),
            KeyCode::Char('r') => {
                if let Some(err) = load_config(demo, state, rt) {
                    message = format!("read failed: {err}");
                }
            }
            KeyCode::Enter => row_action(state, rt, demo, &mut message),
            _ => {}
        }
        return KeyOutcome::msg(std::mem::take(&mut message));
    }

    // ---- Packages 页 ----
    if let Some(ask) = pkg.plugin_ask.as_mut() {
        if code == KeyCode::Tab {
            return KeyOutcome::none();
        }
        match code {
            KeyCode::Esc => pkg.plugin_ask = None,
            KeyCode::Char('j') | KeyCode::Down => {
                ask.sel = (ask.sel + 1).min(ask.clis.len() - 1);
            }
            KeyCode::Char('k') | KeyCode::Up => ask.sel = ask.sel.saturating_sub(1),
            KeyCode::Char(' ') => {
                if let Some(c) = ask.checked.get_mut(ask.sel) {
                    *c = !*c;
                }
            }
            KeyCode::Enter => {
                let ask = pkg.plugin_ask.take().expect("checked above");
                let picks: Vec<_> = ask
                    .clis
                    .iter()
                    .zip(&ask.checked)
                    .filter(|(_, c)| **c)
                    .map(|(c, _)| *c)
                    .collect();
                if picks.is_empty() {
                    return KeyOutcome::msg("no plugin selected".into());
                }
                start_plugin_install(pkg, pkg_rt, picks);
                if pkg.running {
                    return KeyOutcome::none();
                }
                return KeyOutcome::msg(std::mem::take(&mut pkg.message));
            }
            _ => {}
        }
        return KeyOutcome::none();
    }
    if pkg.menu.is_some() {
        if code == KeyCode::Tab {
            return KeyOutcome::none();
        }
        match code {
            KeyCode::Esc => pkg.menu = None,
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(m) = pkg.menu.as_mut() {
                    m.sel = (m.sel + 1).min(PKG_MENU_ITEMS.len() - 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(m) = pkg.menu.as_mut() {
                    m.sel = m.sel.saturating_sub(1);
                }
            }
            KeyCode::Enter => {
                if let Some(m) = pkg.menu.take() {
                    match m.sel {
                        0 => install_pkg(pkg, pkg_rt, dir, dry_run, m.pkg, false),
                        1 => install_pkg(pkg, pkg_rt, dir, dry_run, m.pkg, true),
                        _ => uninstall_pkg(pkg, dir, dry_run, m.pkg),
                    }
                }
                // install/uninstall 写了 pkg.message;uninstall 还触发了刷新需求。
                return KeyOutcome::msg(std::mem::take(&mut pkg.message));
            }
            _ => {}
        }
        return KeyOutcome::msg(std::mem::take(&mut pkg.message));
    }
    if pkg.running {
        // 下载中:忽略其余按键(转圈框有提示),等收尾。
        return KeyOutcome::none();
    }

    // ---- 全局键 ----
    match code {
        KeyCode::Char('1') => {
            switch_to(&mut tab, super::state::Tab::Packages, keys, watch);
            return KeyOutcome::msg(tab_message(super::state::Tab::Packages));
        }
        KeyCode::Char('2') => {
            switch_to(&mut tab, super::state::Tab::Keyboard, keys, watch);
            return KeyOutcome::msg(tab_message(super::state::Tab::Keyboard));
        }
        KeyCode::Char('3') => {
            switch_to(&mut tab, super::state::Tab::Watch, keys, watch);
            return KeyOutcome::msg(tab_message(super::state::Tab::Watch));
        }
        KeyCode::Tab => {
            let next = tab.get().next();
            switch_to(&mut tab, next, keys, watch);
            return KeyOutcome::msg(tab_message(next));
        }
        KeyCode::Char('j') | KeyCode::Down => {
            pkg.sel = (pkg.sel + 1).min(pkg.entries.len().saturating_sub(1));
        }
        KeyCode::Char('k') | KeyCode::Up => pkg.sel = pkg.sel.saturating_sub(1),
        KeyCode::Char('p') => {
            *allow_pre = !*allow_pre;
            return KeyOutcome::msg(format!(
                "prerelease: {}",
                if *allow_pre { "on" } else { "off" }
            ));
        }
        KeyCode::Char('r') => {
            pkg.refresh_pending = true;
            pkg.message = "refreshing…".into();
            return KeyOutcome::msg(std::mem::take(&mut pkg.message));
        }
        KeyCode::Enter => {
            pkg.menu = Some(PkgMenu {
                pkg: pkg.sel,
                sel: 0,
            });
        }
        KeyCode::Char('u') => {
            let idx = pkg.sel;
            let pre = *allow_pre;
            install_pkg(pkg, pkg_rt, dir, dry_run, idx, pre);
            if pkg.running {
                return KeyOutcome::none();
            }
            return KeyOutcome::msg(std::mem::take(&mut pkg.message));
        }
        KeyCode::Char('U') => {
            let pre = *allow_pre;
            update_all(pkg, pkg_rt, dir, dry_run, pre);
            if pkg.running {
                return KeyOutcome::none();
            }
            return KeyOutcome::msg(std::mem::take(&mut pkg.message));
        }
        _ => {}
    }
    KeyOutcome::msg(std::mem::take(&mut pkg.message))
}

fn tab_message(t: super::state::Tab) -> String {
    match t {
        super::state::Tab::Keyboard => "vibekeys — Enter run/edit · r re-read config".into(),
        super::state::Tab::Watch => "watch — Enter run/edit · r re-read config".into(),
        super::state::Tab::Packages => {
            "packages — Enter actions · u update · U all · p prerelease".into()
        }
    }
}

fn switch_tab(
    tab: &mut ratatui_kit::State<super::state::Tab>,
    keys: &mut DeviceState,
    watch: &mut DeviceState,
    message: &mut String,
) {
    let next = tab.get().next();
    switch_to(tab, next, keys, watch);
    *message = tab_message(next);
}

fn switch_to(
    tab: &mut ratatui_kit::State<super::state::Tab>,
    next: super::state::Tab,
    keys: &mut DeviceState,
    watch: &mut DeviceState,
) {
    tab.set_no_update(next);
    match next {
        super::state::Tab::Keyboard => device_enter(keys),
        super::state::Tab::Watch => device_enter(watch),
        super::state::Tab::Packages => {}
    }
}

/// pop 框按键。返回 Some(message) = 已消费并给出提示;回车提交直接在此完成
/// (会话写是毫秒级,不冻结)。
fn popup_key(
    state: &mut DeviceState,
    rt: &mut DeviceRuntime,
    demo: bool,
    key: &KeyEvent,
) -> Option<String> {
    let popup = state.popup.as_mut()?;
    match key.code {
        KeyCode::Esc => {
            state.popup = None;
            Some("cancelled".into())
        }
        KeyCode::Tab | KeyCode::Down => {
            popup.cur = (popup.cur + 1) % popup.labels.len();
            None
        }
        KeyCode::Up => {
            popup.cur = (popup.cur + popup.labels.len() - 1) % popup.labels.len();
            None
        }
        KeyCode::Backspace => {
            popup.inputs[popup.cur].backspace();
            None
        }
        KeyCode::Delete => {
            popup.inputs[popup.cur].delete();
            None
        }
        KeyCode::Left => {
            popup.inputs[popup.cur].left();
            None
        }
        KeyCode::Right => {
            popup.inputs[popup.cur].right();
            None
        }
        KeyCode::Home => {
            popup.inputs[popup.cur].home();
            None
        }
        KeyCode::End => {
            popup.inputs[popup.cur].end();
            None
        }
        KeyCode::Enter => {
            if popup.cur + 1 < popup.labels.len() {
                popup.cur += 1;
                return None;
            }
            let vals: Vec<String> = popup.inputs.iter().map(|i| i.buf.clone()).collect();
            let state_kind = state.popup.take()?.kind;
            let title = state_kind.spec().0.trim().to_string();
            let res = match &state_kind {
                PopupKind::WifiAdd => do_wifi_add(
                    demo,
                    rt,
                    vals.first().map(String::as_str).unwrap_or(""),
                    vals.get(1).map(String::as_str).unwrap_or(""),
                ),
                PopupKind::WifiPass { ssid } => do_wifi_add(
                    demo,
                    rt,
                    ssid,
                    vals.first().map(String::as_str).unwrap_or(""),
                ),
            };
            let mut m = format!("{title}: {}", describe(&res, demo));
            after_action(demo, state, rt, res.is_ok(), &mut m);
            Some(m)
        }
        KeyCode::Char(c) => {
            popup.inputs[popup.cur].insert(c);
            None
        }
        _ => None,
    }
}

/// 行内编辑按键。返回 Some(message) = 已消费;回车提交直接在此完成。
fn editing_key(
    state: &mut DeviceState,
    rt: &mut DeviceRuntime,
    demo: bool,
    key: &KeyEvent,
) -> Option<String> {
    let (_, input) = state.editing.as_mut()?;
    match key.code {
        KeyCode::Esc => {
            state.editing = None;
            Some("edit cancelled".into())
        }
        KeyCode::Enter => {
            let (idx, val) = state.editing.take().unwrap();
            let field = match &state.rows[idx].action {
                RowAction::Edit(f) => *f,
                _ => unreachable!(),
            };
            let label = state.rows[idx].label.clone();
            let res = do_field(demo, rt, field, &val.buf);
            let mut m = format!("{label}: {}", describe(&res, demo));
            after_action(demo, state, rt, res.is_ok(), &mut m);
            Some(m)
        }
        KeyCode::Backspace => {
            input.backspace();
            None
        }
        KeyCode::Delete => {
            input.delete();
            None
        }
        KeyCode::Left => {
            input.left();
            None
        }
        KeyCode::Right => {
            input.right();
            None
        }
        KeyCode::Home => {
            input.home();
            None
        }
        KeyCode::End => {
            input.end();
            None
        }
        KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            input.left();
            None
        }
        KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            input.right();
            None
        }
        KeyCode::Char(c) => {
            input.insert(c);
            None
        }
        _ => None,
    }
}

fn device_flow_key(
    state: &mut DeviceState,
    rt: &mut DeviceRuntime,
    demo: bool,
    code: KeyCode,
    message: &mut String,
) {
    #[derive(Clone, Copy)]
    enum Flow {
        Fresh,
        Menu,
        WebSetup,
        Scanning,
        Pick,
        Connecting,
        Failed,
        Disconnected,
    }
    let flow = match state.conn {
        Conn::Fresh => Flow::Fresh,
        Conn::Menu => Flow::Menu,
        Conn::WebSetup => Flow::WebSetup,
        Conn::Scanning => Flow::Scanning,
        Conn::Pick => Flow::Pick,
        Conn::Connecting(_) => Flow::Connecting,
        Conn::Ready => return,
        Conn::Failed(_) => Flow::Failed,
        Conn::Disconnected => Flow::Disconnected,
    };
    match (flow, code) {
        (Flow::Fresh, KeyCode::Enter | KeyCode::Char('r')) => start_scan(state, rt, demo),
        (Flow::Menu, KeyCode::Char('j') | KeyCode::Down) => {
            state.menu_sel = (state.menu_sel + 1).min(1);
        }
        (Flow::Menu, KeyCode::Char('k') | KeyCode::Up) => {
            state.menu_sel = state.menu_sel.saturating_sub(1);
        }
        (Flow::Menu, KeyCode::Enter) => match state.menu_sel {
            0 => start_scan(state, rt, demo),
            _ => show_web_setup(state),
        },
        // 数字直选(此时全局 1/2/3 切标签让位给菜单)。
        (Flow::Menu, KeyCode::Char('1')) => start_scan(state, rt, demo),
        (Flow::Menu, KeyCode::Char('2')) => show_web_setup(state),
        (Flow::WebSetup, KeyCode::Esc | KeyCode::Enter) => state.conn = Conn::Menu,
        (Flow::Scanning, KeyCode::Char('r')) => start_scan(state, rt, demo),
        (Flow::Scanning, KeyCode::Esc) => {
            rt.job = None;
            state.conn = Conn::Menu;
            *message = "scan cancelled".into();
        }
        (Flow::Pick, KeyCode::Char('j') | KeyCode::Down) => {
            state.pick_sel = (state.pick_sel + 1).min(state.pick.len().saturating_sub(1));
        }
        (Flow::Pick, KeyCode::Char('k') | KeyCode::Up) => {
            state.pick_sel = state.pick_sel.saturating_sub(1);
        }
        (Flow::Pick, KeyCode::Enter) => pick_connect(state, rt, demo),
        (Flow::Pick, KeyCode::Char('r') | KeyCode::Esc) => start_scan(state, rt, demo),
        (Flow::Connecting, KeyCode::Esc) => {
            // 弃掉连接任务/会话,回列表;设备也许还在广播,重连再试即可。
            rt.job = None;
            drop(rt.session.take());
            state.conn = Conn::Pick;
            *message = "connect cancelled".into();
        }
        (Flow::Failed, KeyCode::Enter | KeyCode::Char('r')) => start_scan(state, rt, demo),
        // 断连弹窗:按回车确认,回扫描页(用户要求只认回车)。
        (Flow::Disconnected, KeyCode::Enter) => start_scan(state, rt, demo),
        _ => {}
    }
}

fn wifi_remove_action(
    state: &mut DeviceState,
    rt: &mut DeviceRuntime,
    demo: bool,
    message: &mut String,
) {
    let ssid = match &state.rows[state.sel].action {
        RowAction::WifiEntry { ssid } => ssid.clone(),
        _ => return,
    };
    let res = do_wifi_remove(demo, rt, &ssid);
    *message = format!("wifi {ssid}: {}", describe(&res, demo));
    after_action(demo, state, rt, res.is_ok(), message);
}

fn row_action(
    state: &mut DeviceState,
    rt: &mut DeviceRuntime,
    demo: bool,
    message: &mut String,
) {
    // 克隆动作,免得借用 rows 卡住后面的 &mut 方法。
    let action = state.rows[state.sel].action.clone();
    match action {
        RowAction::AsrClear | RowAction::ResetDevice => {
            let label = state.rows[state.sel].label.clone();
            let res = match action {
                RowAction::AsrClear => do_asr_clear(demo, rt),
                _ => do_reset(demo, rt),
            };
            *message = format!("{label}: {}", describe(&res, demo));
            after_action(demo, state, rt, res.is_ok(), message);
        }
        RowAction::Edit(_) => {
            // 预填当前值:方便只改一部分。
            let value = state.rows[state.sel].value.clone();
            state.editing = Some((state.sel, super::state::Input::with_str(&value)));
            *message = "editing — Enter save · Esc cancel".into();
        }
        RowAction::Cycle { field, values } => {
            let current = state.rows[state.sel].value.trim().to_lowercase();
            let next = next_in_cycle(&values, &current);
            let label = state.rows[state.sel].label.clone();
            let res = do_field(demo, rt, field, next);
            *message = format!("{label}: {}", describe(&res, demo));
            after_action(demo, state, rt, res.is_ok(), message);
        }
        RowAction::Popup(kind) => {
            state.popup = Some(PopupState::new(kind, Vec::new()));
            *message = "Tab/↑↓ field · Enter save · Esc cancel".into();
        }
        RowAction::KeymapPopup => {
            state.keymap_popup = Some(super::state::KeymapPopupState::default());
            *message = "↑↓ key · Enter edit · Esc close".into();
        }
        RowAction::WifiEntry { ssid } => {
            // 从快照里拿当前密码,弹框改;提交 = 同 ssid 覆盖。
            let pass = state
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
            state.popup = Some(PopupState::new(PopupKind::WifiPass { ssid }, vec![pass]));
            *message = "Enter save · Esc cancel".into();
        }
    }
}
