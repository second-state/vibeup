//! Agent CLI plugin(Claude Code / Codex)安装与命令流式输出。
//!
//! 从 wizard 抽出的纯同步逻辑,TUI 与 webui 共用:跑 `cli plugin
//! marketplace add second-state/marketplace` + `plugin install/add
//! vibekeys@second-state-tools`,输出逐行进共享缓冲。

use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

/// 后台任务输出行(实时流,UI 每 100ms 快照一次)。
pub(crate) type SharedLines = Arc<Mutex<Vec<String>>>;

/// agent plugin 的两个固定参数(marketplace 名、插件 id)。唯一的真相源:
/// 安装(TUI 包页阻塞版 / wizard·webui 流式版)、卸载、补救文案全部引用这里,
/// 改一次全跟着变。
pub(crate) const PLUGIN_MARKETPLACE: &str = "second-state/marketplace";
pub(crate) const PLUGIN_ID: &str = "vibekeys@second-state-tools";

/// 装失败的补救提示(两个安装入口共用同一份文案)。
pub(crate) fn plugin_install_failed(label: &str, cli: &str, action: &str) -> String {
    format!(
        "{label} plugin install failed — later: `{cli} plugin marketplace add {PLUGIN_MARKETPLACE}` + `{cli} plugin {action} {PLUGIN_ID}`"
    )
}

/// 正在跑的 plugin 安装子进程 PID(网络卡死时的中断逃生通道)。
static ACTIVE_PLUGIN_PID: Mutex<Option<u32>> = Mutex::new(None);

/// cli 是否在本机 PATH 上(`--version` 能跑通)。
pub(crate) fn cli_on_path(cli: &str) -> bool {
    std::process::Command::new(cli)
        .arg("--version")
        .output()
        .is_ok()
}

/// 过滤 ANSI 转义序列与回车(claude/codex CLI 的进度条输出在 TUI 里是乱码)。
pub(crate) fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                // CSI 序列:跳到结束字节(@..~)。
                if let Some('[') = chars.next() {
                    for c2 in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c2) {
                            break;
                        }
                    }
                }
            }
            '\r' => {}
            other => out.push(other),
        }
    }
    out
}

/// 往共享输出缓冲推一行(限长,保留最近 80 行)。
pub(crate) fn push_line(sink: &SharedLines, line: &str) {
    let clean = strip_ansi(line);
    if clean.trim().is_empty() {
        return;
    }
    let mut g = sink.lock().unwrap();
    g.push(format!(
        "  │ {}",
        clean.chars().take(90).collect::<String>()
    ));
    let len = g.len();
    if len > 80 {
        g.drain(..len - 80);
    }
}

/// 中断正在跑的 plugin 安装子进程(网络卡死的逃生通道)。返回是否杀掉了进程。
pub(crate) fn kill_active_plugin() -> bool {
    let pid = ACTIVE_PLUGIN_PID.lock().unwrap().take();
    let Some(pid) = pid else { return false };
    let st = if cfg!(windows) {
        std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .status()
    } else {
        std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status()
    };
    st.map(|s| s.success()).unwrap_or(false)
}

/// 跑一条命令,stdout/stderr 逐行实时推给 sink。返回是否成功退出。
/// `raw = true` 时保留 ANSI 转义、不加 "  │ " 前缀
/// (给 webui 的 xterm.js 终端视图渲染原始彩色输出)。
pub(crate) fn stream_cmd_to(cli: &str, args: &[&str], sink: &SharedLines, raw: bool) -> bool {
    if raw {
        let mut g = sink.lock().unwrap();
        g.push(format!("\x1b[2m$ {cli} {}\x1b[0m", args.join(" ")));
    } else {
        push_line(sink, &format!("$ {cli} {}", args.join(" ")));
    }
    let Ok(mut child) = std::process::Command::new(cli)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    else {
        *ACTIVE_PLUGIN_PID.lock().unwrap() = None;
        push_line(sink, "  failed to spawn");
        return false;
    };
    *ACTIVE_PLUGIN_PID.lock().unwrap() = Some(child.id());
    // stderr 单独线程读,stdout 本线程读,防管道写满互相卡死。
    let stderr = child.stderr.take();
    let sink2 = sink.clone();
    let err_reader = std::thread::spawn(move || {
        if let Some(s) = stderr {
            for line in BufReader::new(s).lines().map_while(Result::ok) {
                if raw {
                    push_raw(&sink2, &line);
                } else {
                    push_line(&sink2, &line);
                }
            }
        }
    });
    if let Some(out) = child.stdout.take() {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if raw {
                push_raw(sink, &line);
            } else {
                push_line(sink, &line);
            }
        }
    }
    let status = child.wait();
    *ACTIVE_PLUGIN_PID.lock().unwrap() = None;
    let _ = err_reader.join();
    match status {
        Ok(s) if s.success() => true,
        Ok(s) => {
            if raw {
                push_raw(
                    sink,
                    &format!("\x1b[31m✗ exit {}\x1b[0m", s.code().unwrap_or(-1)),
                );
            } else {
                push_line(sink, &format!("  ✗ exit {}", s.code().unwrap_or(-1)));
            }
            false
        }
        Err(e) => {
            if raw {
                push_raw(sink, &format!("\x1b[31m✗ {e}\x1b[0m"));
            } else {
                push_line(sink, &format!("  ✗ {e}"));
            }
            false
        }
    }
}

/// raw 模式:保留原样(ANSI 彩色),只裁掉行尾空白;同样限长/限行数。
fn push_raw(sink: &SharedLines, line: &str) {
    let clean = line.trim_end();
    if clean.is_empty() {
        return;
    }
    let mut g = sink.lock().unwrap();
    g.push(clean.chars().take(200).collect::<String>());
    let len = g.len();
    if len > 80 {
        g.drain(..len - 80);
    }
}

/// 装 (cli, label) 的 agent plugin:marketplace add + plugin install。
/// hook 配置都在 plugin 里,装上即生效。输出实时流到 sink。
/// 返回总结页要显示的行。
pub(crate) fn install_plugin(cli: &str, action: &str, label: &str, sink: &SharedLines) -> String {
    install_plugin_to(cli, action, label, sink, false)
}

/// [`install_plugin`] 的可调变体:`raw = true` 输出保留 ANSI(webui 的 xterm)。
pub(crate) fn install_plugin_to(
    cli: &str,
    action: &str,
    label: &str,
    sink: &SharedLines,
    raw: bool,
) -> String {
    let ok1 = stream_cmd_to(
        cli,
        &["plugin", "marketplace", "add", PLUGIN_MARKETPLACE],
        sink,
        raw,
    );
    let ok2 = stream_cmd_to(cli, &["plugin", action, PLUGIN_ID], sink, raw);
    if ok1 && ok2 {
        format!("{label} plugin installed")
    } else {
        plugin_install_failed(label, cli, action)
    }
}

/// demo 假装装 plugin(输出也走流)。
pub(crate) fn demo_install_plugin(label: &str, sink: &SharedLines) -> String {
    push_line(sink, "(demo) adding marketplace…");
    std::thread::sleep(std::time::Duration::from_millis(500));
    push_line(sink, "(demo) marketplace added");
    std::thread::sleep(std::time::Duration::from_millis(500));
    push_line(sink, "(demo) plugin installed");
    std::thread::sleep(std::time::Duration::from_millis(400));
    format!("{label} plugin installed (demo)")
}

/// demo:Codex plugin 固定失败,方便测试失败 → 重试/跳过/Esc 返回的流程。
pub(crate) fn demo_install_codex_fails(
    cli: &str,
    action: &str,
    label: &str,
    sink: &SharedLines,
) -> String {
    push_line(sink, "(demo) adding marketplace…");
    std::thread::sleep(std::time::Duration::from_millis(500));
    push_line(sink, "(demo) marketplace added");
    std::thread::sleep(std::time::Duration::from_millis(500));
    push_line(sink, "(demo) plugin install failed");
    std::thread::sleep(std::time::Duration::from_millis(400));
    plugin_install_failed(label, cli, action)
}
