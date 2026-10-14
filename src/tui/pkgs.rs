//! 管理标签的包条目:本地已装版本 + GitHub 最新 release 的查询结果。

use crate::pkg::{self, Pkg};
use std::path::Path;

pub(super) struct Entry {
    pub(super) pkg: &'static Pkg,
    pub(super) installed: Option<String>,
    pub(super) latest: Option<String>, // 最新稳定版
    pub(super) any: Option<String>,    // 最新 release(含 prerelease)
    pub(super) note: Option<String>,   // 查询失败原因
}

impl Entry {
    pub(super) fn new(pkg: &'static Pkg) -> Self {
        Self {
            pkg,
            installed: None,
            latest: None,
            any: None,
            note: None,
        }
    }

    pub(super) fn status(&self, allow_pre: bool) -> &'static str {
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
    pub(super) fn target(&self, allow_pre: bool) -> Option<String> {
        match allow_pre {
            true => self.any.clone(),
            false => self.latest.clone(),
        }
    }
}

/// 刷新全部条目(本地版本 + 线上 release,阻塞,调用方负责先画一帧)。
pub(super) fn fetch(entries: &mut [Entry], dir: &Path) {
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

/// 窄屏下的紧凑文案。
pub(super) fn short_status(s: &str) -> &str {
    match s {
        "update available" => "update",
        "not installed" => "-",
        other => other,
    }
}

/// Packages 页回车弹出的动作菜单:打开时锁定的包下标 + 菜单光标。
pub(super) struct PkgMenu {
    /// 打开菜单时选中的包(菜单期间行选择不动,存下标最稳)。
    pub(super) pkg: usize,
    pub(super) sel: usize,
}

/// 菜单选项文案(下标即执行分支)。
pub(super) const PKG_MENU_ITEMS: [&str; 3] = [
    "Install / update latest",
    "Install prerelease",
    "Uninstall",
];

/// 后台包任务(下载几十 MB 要几十秒):线程里跑,结果回 channel,TUI 轮询
/// 不冻结、不闪屏。
pub(super) struct PkgJob {
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    rx: std::sync::mpsc::Receiver<PkgJobOut>,
}

pub(super) enum PkgJobOut {
    /// 动作结果 + 刷新后的包表(线程里 fetch,UI 线程零阻塞)。
    Finished(anyhow::Result<()>, Vec<Entry>),
}

/// 下载进度快照(后台线程写,UI 每 80ms 读一次)。
#[derive(Clone, Copy, Default)]
pub(super) struct DownloadProgress {
    pub downloaded: u64,
    pub total: Option<u64>,
}

pub(super) type SharedProgress = std::sync::Arc<std::sync::Mutex<DownloadProgress>>;

impl PkgJob {
    pub(super) fn is_done(&self) -> bool {
        self.done.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(super) fn try_recv(&self) -> Option<PkgJobOut> {
        self.rx.try_recv().ok()
    }
}

/// 起一个包任务线程。`f` 里做阻塞动作 + fetch 新表。
pub(super) fn spawn_pkg_job(f: impl FnOnce() -> PkgJobOut + Send + 'static) -> PkgJob {
    let (tx, rx) = std::sync::mpsc::channel();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = done.clone();
    std::thread::spawn(move || {
        let _ = tx.send(f());
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    PkgJob { done, rx }
}
