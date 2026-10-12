//! `vibeup status` / 裸跑:每个包的已装版本 vs 最新稳定版 / 最新 prerelease。

use crate::pkg::{self, PKGS};
use anyhow::Result;
use std::path::PathBuf;

pub fn run(dir: &Option<PathBuf>) -> Result<()> {
    let dir = pkg::install_dir(dir.as_deref());

    let mut rows: Vec<(String, String, String, String)> = Vec::new();
    for p in PKGS {
        let inst = pkg::installed_version(p, &dir);
        let installed = inst.clone().unwrap_or_else(|| "not installed".into());
        let (latest, any) = match pkg::releases(p.repo) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{}: lookup failed: {e:#}", p.name);
                rows.push((p.name.to_string(), installed, "?".into(), "?".into()));
                continue;
            }
        };
        let any = any.unwrap_or_else(|| "-".into());
        let any = if any == latest { "-".into() } else { any };
        let mark = match &inst {
            Some(v) if pkg::normalize(v) != pkg::normalize(&latest) => "  *",
            _ => "",
        };
        rows.push((format!("{}{mark}", p.name), installed, latest, any));
    }

    let (w0, w1, w2) = (
        rows.iter().map(|r| r.0.len()).max().unwrap_or(0),
        rows.iter().map(|r| r.1.len()).max().unwrap_or(0),
        rows.iter().map(|r| r.2.len()).max().unwrap_or(0),
    );
    println!(
        "{:<w0$}  {:<w1$}  {:<w2$}  latest-pre",
        "package", "installed", "latest",
    );
    for r in &rows {
        println!("{:<w0$}  {:<w1$}  {:<w2$}  {}", r.0, r.1, r.2, r.3);
    }
    println!("\ninstall dir: {}  (* update available)", dir.display());
    Ok(())
}
