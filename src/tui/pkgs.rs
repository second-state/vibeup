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
