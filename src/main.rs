//! vibeup — Install and update the vibe family host tools (vibetty, vibekeys, …).
//!
//! Downloads platform-specific prebuilt binaries from GitHub Releases and
//! installs them as `<bin>-<version>` + symlink (matches each repo's
//! install.sh convention).

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
    /// Print what would be done without actually downloading or writing to disk
    #[arg(long, global = true)]
    dry_run: bool,
    /// Install directory (default: ~/.cargo/bin, matching each repo's install.sh)
    #[arg(long, global = true, value_name = "PATH")]
    dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

/// `config keymap` 的绑定语法速查,附在该子命令 --help 的尾部。
const KEYMAP_BINDING_HELP: &str = r#"
Binding forms:
  "text"    text macro — typed verbatim when the key is pressed
            (escapes: \n \r \t \\ \" \0 \xHH), e.g. '"/compact\n"'
  A, 5      single uppercase letter or digit
  name      one of the key names below, e.g. delete, esc, f5
  Mod+Key   key combo, e.g. Ctrl+C, Alt+Delete, Ctrl+Shift+T
            modifiers: ctrl alt shift option win cmd gui meta

Key names (case-insensitive):
  enter, return              Enter
  space                      Space
  tab                        Tab
  escape, esc                Escape
  backspace                  Backspace
  delete                     Delete
  insert                     Insert
  home, end                  Home / End
  pageup, pagedown           Page Up / Page Down
  up, down, left, right      Arrow keys
  f1 .. f12                  Function keys
  minus                      -
  equal, plus                = (base key of =/+; for a literal + use Shift+Equal)
  semicolon, quote           ; '
  backquote, backslash       ` \
  comma, period, slash       , . /
  bracketleft, bracketright  [ ]
  ctrl, shift, alt, gui, meta, win, cmd
                             modifier keys pressed alone

Physical keys: MIC CUSTOM ESC NEXT BACKSPACE SWITCH ACCEPT ROTATE

Examples:
  vibeup config keymap vibekeys CUSTOM delete       # CUSTOM acts as Delete
  vibeup config keymap vibekeys MIC Ctrl+X          # MIC sends Ctrl+X
  vibeup config keymap vibekeys ESC '"/compact\n"'  # ESC types /compact + Enter
"#;

#[derive(Subcommand)]
enum Cmd {
    /// Open the TUI dashboard
    Tui {
        /// Use fake device data (for demo/development without hardware)
        #[arg(long)]
        demo: bool,
    },
    /// Show installed versions vs latest versions
    Status {
        /// Output machine-readable JSON (for scripts and the omarchy widget)
        #[arg(long)]
        json: bool,
    },
    /// Install a package; TAG defaults to latest stable, e.g. v0.4.3-rc.1
    Install { name: String, tag: Option<String> },
    /// Update to latest version (all packages by default, or vibeup itself with `self`)
    Update {
        /// Only update this package; `self` = vibeup itself
        name: Option<String>,
        /// Update to latest prerelease
        #[arg(long)]
        pre: bool,
    },
    /// Uninstall a package (removes entry point and all versioned files)
    Uninstall { name: String },
    /// Configure devices over BLE (VibeKeys keyboard / VibeWatch)
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the device's CONFIG snapshot
    Show {
        /// Target device: vibekeys | watch
        #[arg(value_parser = clap::value_parser!(String))]
        device: String,
    },
    /// View / modify the WiFi list (order = connection priority, max 8 entries)
    Wifi {
        device: String,
        #[command(subcommand)]
        cmd: WifiCmd,
    },
    /// Set the MQTT broker address
    ServerUrl {
        device: String,
        url: String,
        /// Reboot the device after writing to apply
        #[arg(long)]
        reset: bool,
    },
    /// Set the Whisper ASR service
    Asr {
        device: String,
        #[arg(long)]
        uri: Option<String>,
        #[arg(long)]
        api_key: Option<String>,
        #[arg(long)]
        model: Option<String>,
        /// Clear the ASR configuration
        #[arg(long)]
        clean: bool,
        #[arg(long)]
        reset: bool,
    },
    /// Set the microphone mode (vibekeys only)
    MicModel {
        device: String,
        /// ptt = push to talk, toggle = tap to start/stop
        mode: String,
        #[arg(long)]
        reset: bool,
    },
    /// Prefer built-in ASR in keyboard mode (vibekeys only)
    PreferAsr {
        device: String,
        /// on | off
        value: String,
        #[arg(long)]
        reset: bool,
    },
    /// Bind a Tailscale auth key (device jumps to its Tailscale page after write)
    Tailscale { device: String, auth_key: String },
    /// Key remapping (vibekeys only; same syntax as `vibekeys keymap`)
    #[command(after_help = KEYMAP_BINDING_HELP)]
    Keymap {
        device: String,
        /// Physical key: MIC CUSTOM ESC NEXT BACKSPACE SWITCH(YOLO) ACCEPT ROTATE
        key: String,
        /// Binding: quoted text macro, single letter/digit, key name, or Mod+Key combo (see below)
        binding: String,
    },
    /// Reboot the device (to apply stored configuration)
    Reset { device: String },
}

#[derive(Subcommand)]
enum WifiCmd {
    /// Print the current WiFi list
    Show,
    /// Append a new entry (or update an existing SSID)
    Add {
        ssid: String,
        #[arg(default_value = "")]
        pass: String,
    },
    /// Remove an entry
    Remove { ssid: String },
    /// Replace the entire list with a JSON array ([{ssid, pass}, …], auto-dedup, capped at 8)
    Set { json: String },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd.unwrap_or(Cmd::Tui { demo: false }) {
        Cmd::Tui { demo } => tui::run(cli.dir, cli.dry_run, demo),
        Cmd::Status { json } => status::run(&cli.dir, json),
        Cmd::Install { name, tag } => {
            let p = pkg::find(&name)?;
            let log = install::install(
                p,
                tag,
                false,
                &pkg::install_dir(cli.dir.as_deref()),
                cli.dry_run,
                true,
                &|_, _| {},
            )?;
            print!("{log}");
            Ok(())
        }
        Cmd::Update { name, pre } => update(name.as_deref(), pre, &cli.dir, cli.dry_run),
        Cmd::Uninstall { name } => {
            let p = if name == "self" { &pkg::SELF } else { pkg::find(&name)? };
            let log = install::uninstall(p, &pkg::install_dir(cli.dir.as_deref()))?;
            print!("{log}");
            Ok(())
        }
        Cmd::Config { cmd } => run_config(cmd),
    }
}

fn run_config(cmd: ConfigCmd) -> Result<()> {
    use config_cmd::{AsrOpts, WifiAction};
    // CLI direct connect: no specific device, connect to first of the scanned kind.
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
        Some(n) if n == "self" => vec![&pkg::SELF],
        Some(n) => vec![pkg::find(n)?],
        None => pkg::PKGS.iter().collect(),
    };
    let mut failed = false;
    for p in targets {
        match install::install(p, None, pre, &dir, dry_run, true, &|_, _| {}) {
            Ok(log) => print!("{log}"),
            Err(e) => {
                eprintln!("{}: {e:#}", p.name);
                failed = true;
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
    Ok(())
}
