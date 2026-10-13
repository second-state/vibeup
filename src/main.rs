//! vibeup —— 安装/更新 vibe 家族的主机工具(vibetty、vibekeys,…)。
//!
//! 从各仓库的 GitHub Releases 下载对应平台的预编译二进制,装成
//! `<bin>-<版本>` + symlink(与各仓库 install.sh 约定一致)。

mod ble;
mod config_cmd;
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
    /// 通过 BLE 配置设备(VibeKeys 键盘 / VibeWatch 手表)
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// 打开 TUI 面板
    Tui {
        /// 设备标签用假数据(无真机时演示/开发用)
        #[arg(long)]
        demo: bool,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// 打印设备的 CONFIG 快照
    Show {
        /// 目标设备:vibekeys | watch
        #[arg(value_parser = clap::value_parser!(String))]
        device: String,
    },
    /// 查看 / 修改 WiFi 列表(顺序 = 连接优先级,最多 8 条)
    Wifi {
        device: String,
        #[command(subcommand)]
        cmd: WifiCmd,
    },
    /// 设置 MQTT broker 地址
    ServerUrl {
        device: String,
        url: String,
        /// 写完让设备重启生效
        #[arg(long)]
        reset: bool,
    },
    /// 设置 Whisper ASR 服务
    Asr {
        device: String,
        #[arg(long)]
        uri: Option<String>,
        #[arg(long)]
        api_key: Option<String>,
        #[arg(long)]
        model: Option<String>,
        /// 清空 ASR 配置
        #[arg(long)]
        clean: bool,
        #[arg(long)]
        reset: bool,
    },
    /// 设置麦克风模式(仅 vibekeys)
    MicModel {
        device: String,
        /// ptt = 按住说话,toggle = 点按开始/停止
        mode: String,
        #[arg(long)]
        reset: bool,
    },
    /// 键盘模式下是否优先用内置 ASR(仅 vibekeys)
    PreferAsr {
        device: String,
        /// on | off
        value: String,
        #[arg(long)]
        reset: bool,
    },
    /// 绑定 Tailscale auth key(写完设备自己跳 Tailscale 页)
    Tailscale { device: String, auth_key: String },
    /// 键位重映射(仅 vibekeys;语法同 `vibekeys keymap`)
    Keymap {
        device: String,
        /// 键名:MIC CUSTOM ESC NEXT BACKSPACE SWITCH(YOLO) ACCEPT ROTATE
        key: String,
        /// 绑定:加引号是文本宏("/compact\\n");单字符或键名或 Ctrl+X 是组合键
        binding: String,
    },
    /// 让设备重启(使已存的配置生效)
    Reset { device: String },
}

#[derive(Subcommand)]
enum WifiCmd {
    /// 打印当前 WiFi 列表
    Show,
    /// 追加(或更新同名 SSID 的)一条
    Add {
        ssid: String,
        #[arg(default_value = "")]
        pass: String,
    },
    /// 移除一条
    Remove { ssid: String },
    /// 用完整 JSON 数组整体替换([{ssid, pass}, …],自动去空、截断到 8 条)
    Set { json: String },
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
        Cmd::Config { cmd } => run_config(cmd),
        Cmd::Tui { demo } => tui::run(cli.dir, cli.dry_run, demo),
    }
}

fn run_config(cmd: ConfigCmd) -> Result<()> {
    use config_cmd::{AsrOpts, WifiAction};
    // CLI 直连:不指定具体设备,连该类型扫到的第一台。
    let parse_device = |s: &str| -> Result<ble::Target> {
        Ok(ble::Target::auto(ble::DeviceKind::parse(s)?))
    };
    match cmd {
        ConfigCmd::Show { device } => config_cmd::show(parse_device(&device)?),
        ConfigCmd::Wifi { device, cmd } => {
            let kind = parse_device(&device)?;
            let action = match cmd {
                WifiCmd::Show => WifiAction::Show,
                WifiCmd::Add { ssid, pass } => WifiAction::Add { ssid, pass },
                WifiCmd::Remove { ssid } => WifiAction::Remove { ssid },
                WifiCmd::Set { json } => WifiAction::Set { json },
            };
            config_cmd::wifi(kind, action, false)
        }
        ConfigCmd::ServerUrl { device, url, reset } => {
            config_cmd::server_url(parse_device(&device)?, url, reset)
        }
        ConfigCmd::Asr {
            device,
            uri,
            api_key,
            model,
            clean,
            reset,
        } => config_cmd::asr(
            parse_device(&device)?,
            AsrOpts {
                uri,
                api_key,
                model,
                clean,
            },
            reset,
        ),
        ConfigCmd::MicModel {
            device,
            mode,
            reset,
        } => config_cmd::mic_model(parse_device(&device)?, mode, reset),
        ConfigCmd::PreferAsr {
            device,
            value,
            reset,
        } => {
            let on = match value.to_lowercase().as_str() {
                "on" | "true" => true,
                "off" | "false" => false,
                other => anyhow::bail!("invalid value `{other}` — use on or off"),
            };
            config_cmd::prefer_asr(parse_device(&device)?, on, reset)
        }
        ConfigCmd::Tailscale { device, auth_key } => {
            config_cmd::tailscale(parse_device(&device)?, auth_key)
        }
        ConfigCmd::Keymap {
            device,
            key,
            binding,
        } => config_cmd::keymap(parse_device(&device)?, key, binding),
        ConfigCmd::Reset { device } => config_cmd::reset(parse_device(&device)?),
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
