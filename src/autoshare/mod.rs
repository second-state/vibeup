//! `vibeup autoshare` — 后台 daemon:自动把 herdr 里的全部 agent share 到 vibetty,
//! 并订阅 `pane.agent_detected` 事件实时跟上新增。unix(macOS/Linux)专属。
//!
//! 结构:本模块(跨平台,只有路径与 clap 枚举)+ 四个 `#[cfg(unix)]` 子模块——
//! `herdr_client`(herdr socket 协议)、`control`(控制 socket 协议与 stop/status
//! 客户端)、`broker`(`vibetty broker` 子进程)、`daemon`(线程与生命周期)。

use anyhow::Result;
use clap::Subcommand;
use std::path::PathBuf;

#[derive(Subcommand, Debug, Clone)]
pub enum AutoshareCmd {
    /// Stop the running daemon (also kills its `vibetty broker` child)
    Stop,
    /// Show whether the daemon is running, what it shared, and broker state
    Status,
}

/// `~/.vibetty`(daemon 的 socket 与日志都放这,挨着 vibetty 本体的 logs/)。
fn vibetty_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".vibetty"))
}

/// 控制 socket:`~/.vibetty/autoshare.sock` —— 既是探活标记也是 stop/status 通道。
#[cfg_attr(windows, allow(dead_code))]
pub fn socket_path() -> Option<PathBuf> {
    vibetty_dir().map(|d| d.join("autoshare.sock"))
}

/// daemon 日志(追加):`~/.vibetty/logs/vibeup-autoshare.log`。
#[cfg_attr(windows, allow(dead_code))]
pub fn log_path() -> Option<PathBuf> {
    vibetty_dir().map(|d| d.join("logs").join("vibeup-autoshare.log"))
}

/// vibetty 本体的配置(daemon 抠 broker 端口用)。
#[cfg_attr(windows, allow(dead_code))]
pub fn vibetty_config_path() -> Option<PathBuf> {
    vibetty_dir().map(|d| d.join("config.toml"))
}

#[cfg(unix)]
pub(crate) mod broker;
#[cfg(unix)]
pub(crate) mod control;
#[cfg(unix)]
pub(crate) mod daemon;
#[cfg(unix)]
pub(crate) mod herdr_client;

#[cfg(unix)]
pub fn dispatch(detach: bool, cmd: Option<AutoshareCmd>, dry_run: bool) -> Result<()> {
    match cmd {
        Some(_) if detach => anyhow::bail!("-d cannot be combined with a subcommand"),
        Some(AutoshareCmd::Stop) => control::stop(),
        Some(AutoshareCmd::Status) => control::status(),
        // dry_run 只对启动有意义:打印将要做的事,不碰 socket / 进程。
        None if dry_run => {
            println!(
                "[dry-run] would {} the autoshare daemon (socket: {})",
                if detach {
                    "start detached"
                } else {
                    "run in foreground"
                },
                crate::autoshare::socket_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "~/.vibetty/autoshare.sock".into()),
            );
            Ok(())
        }
        None if detach => {
            println!("{}", daemon::spawn_detached()?);
            Ok(())
        }
        None => daemon::run(),
    }
}

#[cfg(windows)]
pub fn dispatch(_detach: bool, _cmd: Option<AutoshareCmd>, _dry_run: bool) -> Result<()> {
    anyhow::bail!("`vibeup autoshare` needs herdr + unix sockets — macOS/Linux only")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_live_under_vibetty_dir() {
        let sock = socket_path().expect("home directory exists");
        assert!(sock.ends_with("autoshare.sock"), "{sock:?}");
        assert!(sock.starts_with(dirs::home_dir().unwrap()));
        let log = log_path().expect("home directory exists");
        assert!(log.ends_with("logs/vibeup-autoshare.log"), "{log:?}");
    }
}
