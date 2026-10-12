//! vibeup —— 安装/更新 vibe 家族的主机工具(vibetty、vibekeys,…)。
//!
//! 从各仓库的 GitHub Releases 下载对应平台的预编译二进制,装成
//! `<bin>-<版本>` + symlink(与各仓库 install.sh 约定一致)。

mod install;
mod pkg;
mod status;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "vibeup",
    version,
    about = "Install and update the vibe family host tools from GitHub Releases"
)]
struct Cli {
    /// 打印将要做的事,不实际下载/写盘
    #[arg(long, global = true)]
    dry_run: bool,
    /// 安装目录(默认 ~/.cargo/bin,与各仓库 install.sh 一致)
    #[arg(long, global = true, value_name = "PATH")]
    dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// 查看已装版本与最新版本(无子命令时的默认行为)
    Status,
    /// 安装一个包;TAG 缺省 = 最新稳定版,也可指定如 v0.4.3-rc.1
    Install { name: String, tag: Option<String> },
    /// 更新到最新版(缺省全部包)
    Update {
        /// 只更新这一个包
        name: Option<String>,
        /// 更新到最新 prerelease
        #[arg(long)]
        pre: bool,
    },
    /// 卸载一个包(删入口和全部版本化文件)
    Uninstall { name: String },
    /// 打开 TUI 面板
    Tui,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd.unwrap_or(Cmd::Status) {
        Cmd::Status => status::run(&cli.dir),
        Cmd::Install { name, tag } => {
            let p = pkg::find(&name)?;
            install::install(
                p,
                tag,
                false,
                &pkg::install_dir(cli.dir.as_deref()),
                cli.dry_run,
            )
        }
        Cmd::Update { name, pre } => update(name.as_deref(), pre, &cli.dir, cli.dry_run),
        Cmd::Uninstall { name } => {
            let p = pkg::find(&name)?;
            install::uninstall(p, &pkg::install_dir(cli.dir.as_deref()))
        }
        Cmd::Tui => tui::run(cli.dir, cli.dry_run),
    }
}

fn update(name: Option<&str>, pre: bool, dir: &Option<PathBuf>, dry_run: bool) -> Result<()> {
    let dir = pkg::install_dir(dir.as_deref());
    let targets: Vec<&'static pkg::Pkg> = match name {
        Some(n) => vec![pkg::find(n)?],
        None => pkg::PKGS.iter().collect(),
    };
    let mut failed = false;
    for p in targets {
        if let Err(e) = install::install(p, None, pre, &dir, dry_run) {
            eprintln!("{}: {e:#}", p.name);
            failed = true;
        }
    }
    if failed {
        std::process::exit(1);
    }
    Ok(())
}
