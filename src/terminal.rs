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

/// 识别出的终端(多路复用器不算——见模块注释)。unix 专属:windows 只支持 wt。
#[cfg(unix)]
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
    Apple,
    ITerm,
    /// VSCode / JetBrains 内置终端:没有可靠的命令行拉起方式,当 Unknown 处理。
    Embedded,
    Unknown,
}

/// 终端名(小写,给 UI 展示用)。
#[cfg(unix)]
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
            Self::Apple => "terminal.app",
            Self::ITerm => "iterm",
            Self::Embedded => "embedded",
            Self::Unknown => "unknown",
        }
    }
}

/// 当前所在的多路复用器(zellij / tmux / screen)。
#[cfg(unix)]
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
#[cfg(unix)]
pub fn detect() -> Terminal {
    match std::env::var("TERM_PROGRAM").as_deref() {
        Ok("ghostty") => return Terminal::Ghostty,
        Ok("WezTerm") => return Terminal::WezTerm,
        Ok("Apple_Terminal") => return Terminal::Apple,
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
    if std::env::var_os("VTE_VERSION").is_some()
        || std::env::var_os("GNOME_TERMINAL_SCREEN").is_some()
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
#[cfg(unix)]
pub fn open_terminal(cmd: &str) -> anyhow::Result<()> {
    let detected = detect();
    let shell = user_shell();
    let mut tried: Vec<String> = Vec::new();
    // 检测到的在最前,再按 FALLBACKS 声明顺序兜底;保持顺序去重(检测到的可能
    // 又在兜底链里)。不能用 HashSet——那会让尝试顺序随机。
    let mut candidates = vec![detected];
    for t in FALLBACKS.iter().copied() {
        if !candidates.contains(&t) {
            candidates.push(t);
        }
    }
    for t in candidates {
        let Some((bin, args)) = launch_for(t, cmd, &shell) else {
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
            vec![
                "-e".to_string(),
                "sh".to_string(),
                "-c".to_string(),
                cmd.to_string(),
            ],
        ),
    ] {
        if on_path(bin) {
            spawn_detached(bin, &args)?;
            return Ok(());
        }
        tried.push(bin.to_string());
    }
    anyhow::bail!(
        "no terminal emulator found to open (tried: {})",
        tried.join(", ")
    )
}

/// windows:只支持 Windows Terminal。wt.exe 的应用执行别名在
/// `%LOCALAPPDATA%\Microsoft\WindowsApps`(默认在用户 PATH)。
///
/// 新标签页里用**父进程的 shell** 跑 `cmd`,命令解析成绝对路径传入,
/// PATH 环境无关紧要;命令退出即关标签。
///
/// wt 的 new-tab 解析会把 `-` 开头的参数当成自己的选项(pwsh 的 flag 拆开
/// 传必挂 "arguments were not expected"),带引号的整串又会被它再引一层
/// (变成 `""…""`,CreateProcess 拿到空程序名,0x80070057)。所以 PowerShell
/// 路径用 **cmd /c 外兜**:外层 cmd 无破折号,wt 解析器放行;内层引号按
/// CommandLineToArgvW 规则 \" 转义。
#[cfg(windows)]
pub fn open_terminal(cmd: &str) -> anyhow::Result<()> {
    let bin = "wt.exe";
    if !on_path(bin) {
        anyhow::bail!("Windows Terminal (wt.exe) not found on PATH");
    }
    let full = resolve_on_path(cmd)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| cmd.to_string());

    // PowerShell 会话:cmd /c 链进 pwsh(PS7)→ powershell。
    if parent_is_pwsh() {
        let quoted = format!("\"cmd /c pwsh.exe -NoLogo -Command \\\"{full}\\\"\"");
        return spawn_detached(bin, &["new-tab".into(), quoted]);
    }
    // 非 PS(cmd 或未知):cmd /C 直接跑,wt 解析器对 /C 开头无感。
    spawn_detached(
        bin,
        &[
            "new-tab".into(),
            "cmd".into(),
            "/C".into(),
            format!("\"{full}\""),
        ],
    )
}

/// 父进程是不是 PowerShell?env 启发式(PSMODULEPATH)不可靠——cmd 下也会
/// 从环境继承到它。直接查控制台进程列表里本进程的父进程映像名。
#[cfg(windows)]
fn parent_is_pwsh() -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Console::GetConsoleProcessList;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };

    let mut ids = [0u32; 32];
    let n = unsafe { GetConsoleProcessList(ids.as_mut_ptr(), 32) } as usize;
    if n < 2 {
        return false;
    }
    // 列表大致按「先来的在前」:父 shell 在我们前面,取倒数第一个非自身。
    let me = std::process::id();
    let Some(&pid) = ids[..n].iter().rev().find(|&&p| p != me) else {
        return false;
    };
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let mut buf = [0u16; 512];
    let mut len = buf.len() as u32;
    let ok = unsafe {
        QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len)
    };
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        return false;
    }
    let name = String::from_utf16_lossy(&buf[..len as usize]).to_lowercase();
    name.ends_with("\\pwsh.exe") || name.ends_with("\\powershell.exe")
}

/// 在当前进程的 PATH 里解析命令的绝对路径(含不带/带 `.exe` 两种形态);
/// 找不到返回 None,交由目标 shell 自己的 PATH 逻辑。
#[cfg(windows)]
fn resolve_on_path(cmd: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(cmd);
        if candidate.is_file() {
            return Some(candidate);
        }
        let with_exe = dir.join(format!("{cmd}.exe"));
        if with_exe.is_file() {
            return Some(with_exe);
        }
    }
    None
}

/// Share 页边框标题用的终端名:unix = 检测到的;windows = wt。
#[cfg(unix)]
pub fn active_name() -> &'static str {
    detect().name()
}

#[cfg(windows)]
pub fn active_name() -> &'static str {
    "wt"
}

/// 候选兜底链(检测不到 / 检测到的没装时按序试)。
/// Windows Terminal 不在链里:windows 平台单独做。
#[cfg(unix)]
const FALLBACKS: &[Terminal] = &[
    Terminal::Kitty,
    Terminal::Ghostty,
    Terminal::Alacritty,
    Terminal::Foot,
    Terminal::Vte,
    Terminal::Konsole,
    Terminal::WezTerm,
];

/// 用户当前 shell:`$SHELL`(缺省 /bin/sh)。
#[cfg(unix)]
fn user_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

/// 用 `shell` 跑 `cmd` 的参数。zsh/bash/fish 以 **登录 + 交互** 模式启动:
/// `-c` 的裸 sh 不读 rc 文件,`~/.cargo/bin`、homebrew 这些写在 .zshrc/.zprofile
/// 里的 PATH 全会丢,命令就找不到了;登录+交互 shell 和用户平时打开的终端环境
/// 一致。其他/未知 shell 退回 `sh -c`。
#[cfg(unix)]
fn shell_args(shell: &str, cmd: &str) -> Vec<String> {
    let name = std::path::Path::new(shell)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    match name {
        "zsh" | "bash" | "fish" => {
            vec![
                shell.to_string(),
                "-l".into(),
                "-i".into(),
                "-c".into(),
                cmd.into(),
            ]
        }
        _ => vec!["sh".into(), "-c".into(), cmd.into()],
    }
}

/// 各终端的拉起参数(`cmd` 是 shell 命令行)。返回 None = 没有可靠的拉起方式。
#[cfg(unix)]
fn launch_for(t: Terminal, cmd: &str, shell: &str) -> Option<(String, Vec<String>)> {
    let sh = |bin: &str, extra: &[&str]| {
        let mut args: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
        args.extend(shell_args(shell, cmd));
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
        Terminal::Apple => {
            // AppleScript 双引号转义后塞进 do script。
            let esc = cmd.replace('\\', "\\\\").replace('"', "\\\"");
            Some((
                "osascript".into(),
                vec![
                    "-e".into(),
                    format!("tell application \"Terminal\" to do script \"{esc}\""),
                ],
            ))
        }
        Terminal::ITerm => {
            // iTerm 的 command 参数直接 execvp:不搜 PATH、也不经过 shell,
            // 裸命令名(如 vibetty)必挂 "execvp failed"。给绝对路径的登录+
            // 交互 shell,让它读 rc 找回 PATH。注意 iTerm 对这个串只做空白
            // 切分、不解析引号,所以 cmd 里不能带空格(调用方都是单词命令)。
            let shell = user_shell();
            let esc = cmd.replace('\\', "\\\\").replace('"', "\\\"");
            Some((
                "osascript".into(),
                vec![
                    "-e".into(),
                    format!(
                        "tell application \"iTerm\" to create window with default profile command \"{shell} -l -i -c {esc}\""
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn launch_args_wrap_user_shell() {
        // zsh/bash/fish 走登录+交互模式(读 rc,保住 PATH)。
        let (bin, args) = launch_for(Terminal::Ghostty, "echo hi", "/bin/zsh").unwrap();
        assert_eq!(bin, "ghostty");
        assert_eq!(args, vec!["-e", "/bin/zsh", "-l", "-i", "-c", "echo hi"]);

        let (bin, args) = launch_for(Terminal::Kitty, "echo hi", "/bin/bash").unwrap();
        assert_eq!(bin, "kitty");
        assert_eq!(
            args,
            vec!["--detach", "/bin/bash", "-l", "-i", "-c", "echo hi"]
        );

        let (bin, args) = launch_for(Terminal::Foot, "echo hi", "/usr/bin/fish").unwrap();
        assert_eq!(bin, "foot");
        assert_eq!(args, vec!["/usr/bin/fish", "-l", "-i", "-c", "echo hi"]);

        // 未知 shell 退回 sh -c。
        let (_, args) = launch_for(Terminal::Foot, "echo hi", "/bin/tcsh").unwrap();
        assert_eq!(args, vec!["sh", "-c", "echo hi"]);
    }

    #[test]
    fn embedded_and_unknown_have_no_launcher() {
        assert!(launch_for(Terminal::Embedded, "x", "/bin/zsh").is_none());
        assert!(launch_for(Terminal::Unknown, "x", "/bin/zsh").is_none());
    }

    #[test]
    fn apple_escapes_quotes() {
        let (_, args) = launch_for(Terminal::Apple, "echo \"hi\"", "/bin/zsh").unwrap();
        assert!(args[1].contains("do script \"echo \\\"hi\\\"\""));
    }

    #[test]
    fn iterm_wraps_absolute_login_shell() {
        // iTerm 的 command 走 execvp:不搜 PATH、不经 shell,所以必须给绝对
        // 路径的登录+交互 shell 包住裸命令名。
        let (_, args) = launch_for(Terminal::ITerm, "vibetty", "/bin/zsh").unwrap();
        assert!(
            args[1].contains(
                "create window with default profile command \"/bin/zsh -l -i -c vibetty\""
            )
        );
    }

    #[test]
    fn fallback_chain_has_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for t in FALLBACKS {
            assert!(seen.insert(*t), "duplicate in FALLBACKS: {t:?}");
        }
    }
}
