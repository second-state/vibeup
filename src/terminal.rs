//! 终端检测与拉起新终端窗口。
//!
//! 检测靠环境变量启发式(没有统一标准;SSH 进来后大部分变量会丢,只能尽力);
//! 拉起按检测结果选二进制,选不出或不在 PATH 上就沿候选链逐个试,
//! 最后兜底 `xdg-terminal-exec` / `x-terminal-emulator`。
//!
//! 常见标记:
//! - `TERM_PROGRAM`:ghostty / WezTerm / iTerm.app / Apple_Terminal / vscode / Hyper / tabby
//! - kitty:`KITTY_WINDOW_ID`;Ghostty 还设 `GHOSTTY_RESOURCES_DIR`
//! - VTE 系(GNOME Terminal 等):`VTE_VERSION` / `GNOME_TERMINAL_SCREEN`
//! - Konsole:`KONSOLE_VERSION`;Alacritty:`ALACRITTY_LOG`
//! - Windows Terminal:`WT_SESSION`;JetBrains:`TERMINAL_EMULATOR`
//! - 多路复用器(`ZELLIJ` / `TMUX` / `STY`)会盖住宿主终端,单独暴露
//!   [`multiplexer()`] 供调用方参考,不参与选择拉起哪个终端。

use std::process::{Command, Stdio};

/// 识别出的终端(多路复用器不算——见模块注释)。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Terminal {
    Kitty,
    Ghostty,
    Alacritty,
    Foot,
    /// VTE 系(GNOME Terminal / Tilix / …),拉起统一走 `gnome-terminal`。
    Vte,
    Konsole,
    WezTerm,
    AppleTerminal,
    ITerm,
    /// VSCode / JetBrains 内置终端:没有可靠的命令行拉起方式,当 Unknown 处理。
    Embedded,
    Unknown,
}

/// 终端名(小写,给 UI 展示用)。
impl Terminal {
    pub fn name(self) -> &'static str {
        match self {
            Self::Kitty => "kitty",
            Self::Ghostty => "ghostty",
            Self::Alacritty => "alacritty",
            Self::Foot => "foot",
            Self::Vte => "vte",
            Self::Konsole => "konsole",
            Self::WezTerm => "wezterm",
            Self::AppleTerminal => "terminal.app",
            Self::ITerm => "iterm",
            Self::Embedded => "embedded",
            Self::Unknown => "unknown",
        }
    }
}

/// 当前所在的多路复用器(zellij / tmux / screen)。
pub fn multiplexer() -> Option<&'static str> {
    if std::env::var_os("ZELLIJ").is_some() {
        Some("zellij")
    } else if std::env::var_os("TMUX").is_some() {
        Some("tmux")
    } else if std::env::var_os("STY").is_some() {
        Some("screen")
    } else {
        None
    }
}

/// 按环境变量猜当前终端。
pub fn detect() -> Terminal {
    match std::env::var("TERM_PROGRAM").as_deref() {
        Ok("ghostty") => return Terminal::Ghostty,
        Ok("WezTerm") => return Terminal::WezTerm,
        Ok("Apple_Terminal") => return Terminal::AppleTerminal,
        Ok("iTerm.app") => return Terminal::ITerm,
        Ok("vscode") | Ok("Hyper") | Ok("tabby") => return Terminal::Embedded,
        _ => {}
    }
    if std::env::var_os("KITTY_WINDOW_ID").is_some() || std::env::var_os("KITTY_PID").is_some() {
        return Terminal::Kitty;
    }
    if std::env::var_os("GHOSTTY_RESOURCES_DIR").is_some() {
        return Terminal::Ghostty;
    }
    if std::env::var_os("ALACRITTY_LOG").is_some() {
        return Terminal::Alacritty;
    }
    if std::env::var_os("KONSOLE_VERSION").is_some() {
        return Terminal::Konsole;
    }
    // `WT_SESSION`(Windows Terminal)不判:wt 的拉起在 windows 平台单独做。
    if std::env::var("TERMINAL_EMULATOR")
        .map(|v| v.contains("JetBrains"))
        .unwrap_or(false)
    {
        return Terminal::Embedded;
    }
    if std::env::var_os("VTE_VERSION").is_some() || std::env::var_os("GNOME_TERMINAL_SCREEN").is_some()
    {
        return Terminal::Vte;
    }
    // TERM 兜底:不少终端直接把名字写进 TERM。
    let term = std::env::var("TERM").unwrap_or_default();
    if term.starts_with("foot") {
        return Terminal::Foot;
    }
    if term.contains("alacritty") {
        return Terminal::Alacritty;
    }
    if term.contains("kitty") {
        return Terminal::Kitty;
    }
    Terminal::Unknown
}

/// 在新终端窗口里跑 `cmd`(shell 命令行,经 `sh -c` 执行)。
/// 检测到的终端优先;二进制不在 PATH 上就沿候选链往下试。
pub fn open_terminal(cmd: &str) -> anyhow::Result<()> {
    let detected = detect();
    let mut tried: Vec<String> = Vec::new();
    for t in [detected]
        .into_iter()
        .chain(FALLBACKS.iter().copied())
        // 去重(检测到的可能又在兜底链里)。
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
    {
        let Some((bin, args)) = launch_for(t, cmd) else {
            continue;
        };
        if !on_path(&bin) {
            tried.push(bin);
            continue;
        }
        spawn_detached(&bin, &args)?;
        return Ok(());
    }
    // 兜底:freedesktop 标准与 debian 别名,它们自己选终端。
    for (bin, args) in [
        (
            "xdg-terminal-exec",
            vec!["sh".to_string(), "-c".to_string(), cmd.to_string()],
        ),
        (
            "x-terminal-emulator",
            vec!["-e".to_string(), "sh".to_string(), "-c".to_string(), cmd.to_string()],
        ),
    ] {
        if on_path(&bin) {
            spawn_detached(&bin, &args)?;
            return Ok(());
        }
        tried.push(bin.to_string());
    }
    anyhow::bail!(
        "no terminal emulator found to open (tried: {})",
        tried.join(", ")
    )
}

/// 候选兜底链(检测不到 / 检测到的没装时按序试)。
/// Windows Terminal 不在链里:windows 平台单独做。
const FALLBACKS: &[Terminal] = &[
    Terminal::Kitty,
    Terminal::Ghostty,
    Terminal::Alacritty,
    Terminal::Foot,
    Terminal::Vte,
    Terminal::Konsole,
    Terminal::WezTerm,
];

/// 各终端的拉起参数(`cmd` 是 shell 命令行)。返回 None = 没有可靠的拉起方式。
fn launch_for(t: Terminal, cmd: &str) -> Option<(String, Vec<String>)> {
    let sh = |bin: &str, extra: &[&str]| {
        let mut args: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
        args.push("sh".into());
        args.push("-c".into());
        args.push(cmd.into());
        Some((bin.to_string(), args))
    };
    match t {
        // kitty 不用 -e,直接跟程序。
        Terminal::Kitty => sh("kitty", &["--detach"]),
        Terminal::Ghostty => sh("ghostty", &["-e"]),
        Terminal::Alacritty => sh("alacritty", &["-e"]),
        // foot 本身就是「foot [command]」。
        Terminal::Foot => sh("foot", &[]),
        Terminal::Vte => sh("gnome-terminal", &["--"]),
        Terminal::Konsole => sh("konsole", &["-e"]),
        Terminal::WezTerm => sh("wezterm", &["start", "--"]),
        Terminal::AppleTerminal => {
            // AppleScript 双引号转义后塞进 do script。
            let esc = cmd.replace('\\', "\\\\").replace('"', "\\\"");
            Some((
                "osascript".into(),
                vec!["-e".into(), format!("tell application \"Terminal\" to do script \"{esc}\"")],
            ))
        }
        Terminal::ITerm => {
            let esc = cmd.replace('\\', "\\\\").replace('"', "\\\"");
            Some((
                "osascript".into(),
                vec![
                    "-e".into(),
                    format!(
                        "tell application \"iTerm\" to create window with default profile command \"{esc}\""
                    ),
                ],
            ))
        }
        Terminal::Embedded | Terminal::Unknown => None,
    }
}

/// 后台拉起,不阻塞、不留僵尸(等子进程的线程收尾)。
fn spawn_detached(bin: &str, args: &[String]) -> anyhow::Result<()> {
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn {bin}: {e}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// bin 是否在 PATH 上。
fn on_path(bin: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|d| d.join(bin).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_args_wrap_shell() {
        let (bin, args) = launch_for(Terminal::Ghostty, "echo hi").unwrap();
        assert_eq!(bin, "ghostty");
        assert_eq!(args, vec!["-e", "sh", "-c", "echo hi"]);

        let (bin, args) = launch_for(Terminal::Kitty, "echo hi").unwrap();
        assert_eq!(bin, "kitty");
        assert_eq!(args, vec!["--detach", "sh", "-c", "echo hi"]);

        let (bin, args) = launch_for(Terminal::Foot, "echo hi").unwrap();
        assert_eq!(bin, "foot");
        assert_eq!(args, vec!["sh", "-c", "echo hi"]);
    }

    #[test]
    fn embedded_and_unknown_have_no_launcher() {
        assert!(launch_for(Terminal::Embedded, "x").is_none());
        assert!(launch_for(Terminal::Unknown, "x").is_none());
    }

    #[test]
    fn apple_escapes_quotes() {
        let (_, args) = launch_for(Terminal::AppleTerminal, "echo \"hi\"").unwrap();
        assert!(args[1].contains("do script \"echo \\\"hi\\\"\""));
    }

    #[test]
    fn fallback_chain_has_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for t in FALLBACKS {
            assert!(seen.insert(*t), "duplicate in FALLBACKS: {t:?}");
        }
    }
}
