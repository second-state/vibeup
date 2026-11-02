//! herdr 侧:检测安装、枚举 running session、枚举各 session 的 agent pane。
//!
//! 全部走 herdr CLI 的 JSON 输出(当前 session = 默认 socket;多 session
//! 用 `--session <name>` 前缀逐个查询):
//! - `herdr --version`                      探测安装
//! - `herdr session list --json`            `{"sessions":[{"name","running",…}]}`
//! - `herdr --session <s> pane list --json` `{"result":{"panes":[{pane_id,
//!   terminal_title_stripped,agent,agent_status,…}]}}`
//!
//! 只收**有 agent** 的 pane(claude/codex 等;普通 shell pane 不算)。

use anyhow::{Context, Result};
use serde::Deserialize;

/// 一行 agent 列表条目(Share 页的一行)。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AgentRow {
    /// 所属 herdr session 名。
    pub session: String,
    /// pane id(后续 share/关闭操作的目标)。
    pub pane_id: String,
    /// 终端标题(strip 后;agent 一般把标题写成自己的状态行)。
    pub title: String,
    /// agent 类型(claude / codex / …)。
    pub agent: String,
    /// agent 状态(idle / working / …;herdr 侧原样)。
    pub state: String,
}

/// herdr 是否可用(`herdr --version` 能跑通)。
pub fn installed() -> bool {
    std::process::Command::new("herdr")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 所有 running 的 session 名(`herdr session list --json`)。
pub fn sessions() -> Result<Vec<String>> {
    #[derive(Deserialize)]
    struct Out {
        sessions: Vec<Sess>,
    }
    #[derive(Deserialize)]
    struct Sess {
        name: String,
        #[serde(default)]
        running: bool,
    }
    let out = herdr(&["session", "list", "--json"])?;
    let parsed: Out = serde_json::from_str(&out).context("parse `herdr session list --json`")?;
    Ok(parsed
        .sessions
        .into_iter()
        .filter(|s| s.running)
        .map(|s| s.name)
        .collect())
}

/// 一个 session 里所有 agent pane(没有就空)。
pub fn agents(session: &str) -> Result<Vec<AgentRow>> {
    #[derive(Deserialize)]
    struct Envelope {
        result: Body,
    }
    #[derive(Deserialize)]
    struct Body {
        panes: Vec<Pane>,
    }
    #[derive(Deserialize)]
    struct Pane {
        pane_id: String,
        #[serde(default)]
        terminal_title_stripped: Option<String>,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        agent: Option<String>,
        #[serde(default)]
        agent_status: Option<String>,
    }
    let out = herdr(&["--session", session, "pane", "list"])?;
    let env: Envelope = serde_json::from_str(&out)
        .with_context(|| format!("parse `herdr --session {session} pane list`"))?;
    Ok(env
        .result
        .panes
        .into_iter()
        .filter_map(|p| {
            let agent = p.agent?;
            Some(AgentRow {
                session: session.to_string(),
                pane_id: p.pane_id,
                title: p
                    .terminal_title_stripped
                    .or(p.label)
                    .unwrap_or_else(|| "(untitled)".into()),
                agent,
                state: p.agent_status.unwrap_or_else(|| "unknown".into()),
            })
        })
        .collect())
}

/// 便捷:所有 running session 的 agent,session 顺序拼接。
/// 单个 session 查询失败不炸整个扫描(记入 Err 汇总)。
pub fn scan() -> Result<Vec<AgentRow>> {
    let mut rows = Vec::new();
    let mut errs: Vec<String> = Vec::new();
    for s in sessions()? {
        match agents(&s) {
            Ok(mut r) => rows.append(&mut r),
            Err(e) => errs.push(format!("{s}: {e:#}")),
        }
    }
    if rows.is_empty() && !errs.is_empty() {
        anyhow::bail!("{}", errs.join("; "));
    }
    Ok(rows)
}

fn herdr(args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("herdr")
        .args(args)
        .output()
        .with_context(|| format!("failed to spawn `herdr {}`", args.join(" ")))?;
    if !out.status.success() {
        anyhow::bail!(
            "`herdr {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_json_parses_and_filters_running() {
        // 真实输出形态:{"sessions":[{"default":true,"name":"default","running":true,…}]}
        let s = r#"{"sessions":[
            {"default":true,"name":"default","running":true,"session_dir":"/x","socket_path":"/x.sock"},
            {"default":false,"name":"work2","running":false,"session_dir":"/y","socket_path":"/y.sock"}
        ]}"#;
        // sessions() 走进程不可单测,这里内联等价解析断言。
        #[derive(Deserialize)]
        struct Out {
            sessions: Vec<Sess>,
        }
        #[derive(Deserialize)]
        struct Sess {
            name: String,
            #[serde(default)]
            running: bool,
        }
        let parsed: Out = serde_json::from_str(s).unwrap();
        let names: Vec<_> = parsed
            .sessions
            .into_iter()
            .filter(|x| x.running)
            .map(|x| x.name)
            .collect();
        assert_eq!(names, vec!["default".to_string()]);
    }

    #[test]
    fn agent_rows_need_agent_field() {
        // 真实 pane list 形态;无 agent 的 shell pane 要被滤掉。
        let s = r#"{"result":{"panes":[
            {"pane_id":"w4:p1","terminal_title_stripped":"dev-gui","agent":"claude","agent_status":"idle"},
            {"pane_id":"w4:p15","terminal_title_stripped":"csh@omarchy:~/x"}
        ]}}"#;
        #[derive(Deserialize)]
        struct Envelope {
            result: Body,
        }
        #[derive(Deserialize)]
        struct Body {
            panes: Vec<Pane>,
        }
        #[derive(Deserialize)]
        struct Pane {
            pane_id: String,
            #[serde(default)]
            terminal_title_stripped: Option<String>,
            #[serde(default)]
            agent: Option<String>,
            #[serde(default)]
            agent_status: Option<String>,
        }
        let env: Envelope = serde_json::from_str(s).unwrap();
        let rows: Vec<_> = env
            .result
            .panes
            .into_iter()
            .filter_map(|p| p.agent.map(|a| (p.pane_id, a, p.agent_status)))
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "w4:p1");
        assert_eq!(rows[0].1, "claude");
    }
}
