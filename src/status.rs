//! `vibeup status` / 裸跑:每个包的已装版本 vs 最新稳定版 / 最新 prerelease。

use crate::pkg::{self, PKGS};
use anyhow::Result;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// 单个包的版本快照;`--json` 的输出单元。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PkgStatus {
    pub name: String,
    /// None = 未安装。
    pub installed: Option<String>,
    /// None = 查询失败。
    pub latest: Option<String>,
    /// 与 latest 不同的最新 prerelease;无则 None。
    pub latest_pre: Option<String>,
    pub update_available: bool,
}

fn collect(dir: &Path) -> Vec<PkgStatus> {
    let mut rows = Vec::new();
    for p in PKGS {
        let inst = pkg::installed_version(p, dir);
        let (latest, latest_pre) = match pkg::releases(p.repo) {
            Ok((latest, any)) => {
                let pre = any.filter(|pre| pkg::normalize(pre) != pkg::normalize(&latest));
                (Some(latest), pre)
            }
            Err(e) => {
                eprintln!("{}: lookup failed: {e:#}", p.name);
                (None, None)
            }
        };
        let update_available = match (&inst, &latest) {
            (Some(v), Some(latest)) => pkg::normalize(v) != pkg::normalize(latest),
            _ => false,
        };
        rows.push(PkgStatus {
            name: p.name.to_string(),
            installed: inst,
            latest,
            latest_pre,
            update_available,
        });
    }
    rows
}

pub fn run(dir: &Option<PathBuf>, json: bool) -> Result<()> {
    let dir = pkg::install_dir(dir.as_deref());
    let rows = collect(&dir);

    if json {
        println!("{}", serde_json::to_string(&rows)?);
        return Ok(());
    }

    // 人类可读表格(列宽自适应;* = 有更新)。
    let disp_installed = |r: &PkgStatus| r.installed.clone().unwrap_or_else(|| "not installed".into());
    let disp_latest = |r: &PkgStatus| r.latest.clone().unwrap_or_else(|| "?".into());
    let disp_pre = |r: &PkgStatus| match (&r.latest_pre, &r.latest) {
        (Some(pre), _) => pre.clone(),
        (None, Some(_)) => "-".into(),
        (None, None) => "?".into(),
    };
    let disp_name = |r: &PkgStatus| {
        if r.update_available {
            format!("{}  *", r.name)
        } else {
            r.name.clone()
        }
    };

    let w0 = rows.iter().map(|r| disp_name(r).len()).max().unwrap_or(0);
    let w1 = rows.iter().map(|r| disp_installed(r).len()).max().unwrap_or(0);
    let w2 = rows.iter().map(|r| disp_latest(r).len()).max().unwrap_or(0);
    println!(
        "{:<w0$}  {:<w1$}  {:<w2$}  latest-pre",
        "package", "installed", "latest",
    );
    for r in &rows {
        println!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {}",
            disp_name(r),
            disp_installed(r),
            disp_latest(r),
            disp_pre(r)
        );
    }
    println!("\ninstall dir: {}  (* update available)", dir.display());
    Ok(())
}
