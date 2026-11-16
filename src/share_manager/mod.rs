//! share —— 管理 herdr 里的 vibetty 会话。
//!
//! 全部通过 shell 出去调现成命令,不碰 MQTT:
//! - 列表:`vibetty share-herdr --list`(vibetty 0.5.0+,输出 JSON 数组
//!   `[{"pane_id":...,"target":...}]`;真相源是 share pane 的终端标题前缀
//!   `vibetty-share:`,pane 没了标题就没了,天然自清理)
//! - 新建:`vibetty share-herdr --target <agent-pane-id>`(在 target 下方开一个
//!   share pane 并缩到一行)
//! - 关闭:`herdr pane close <pane_id>`
//!
//! 输出解析([`parse_list`])与命令执行分离,前者可单测。

pub mod herdr;

use anyhow::{Context, Result};
use serde::Deserialize;

/// 一个被 share 的 vibetty 会话。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Session {
    /// share pane 的 herdr pane id(关闭会话用它)。
    pub pane_id: String,
    /// 被分享的 agent pane id(vibetty share-herdr 的 --target)。
    pub target: String,
}

/// 列出当前所有被 share 的会话。
pub fn list() -> Result<Vec<Session>> {
    let out = vibetty(&["share-herdr", "--list"])?;
    parse_list(&out)
}

/// 为 agent pane `target` 开一个 share 会话,返回新 share pane 的 id。
/// `share-herdr --target` 成功时 stdout 为空(拿不到 pane id),
/// 所以执行后用 `--list` 反查 target 对应的 share pane。
pub fn share(target: &str) -> Result<String> {
    vibetty(&["share-herdr", "--target", target])?;
    let sessions = list()?;
    sessions
        .iter()
        .find(|s| s.target == target)
        .map(|s| s.pane_id.clone())
        .ok_or_else(|| anyhow::anyhow!("share-herdr ran but {target} is not in the share list"))
}

/// 关闭一个 share 会话(kill 它的 share pane)。
pub fn close(pane_id: &str) -> Result<()> {
    let out = std::process::Command::new("herdr")
        .args(["pane", "close", pane_id])
        .output()
        .context("failed to spawn `herdr pane close`")?;
    if !out.status.success() {
        anyhow::bail!(
            "`herdr pane close {pane_id}` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn vibetty(args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("vibetty")
        .args(args)
        .output()
        .with_context(|| format!("failed to spawn `vibetty {}`", args.join(" ")))?;
    if !out.status.success() {
        anyhow::bail!(
            "`vibetty {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// 解析 `vibetty share-herdr --list` 的 JSON 数组输出。
fn parse_list(s: &str) -> Result<Vec<Session>> {
    #[derive(Deserialize)]
    struct Raw {
        pane_id: String,
        target: String,
    }
    let raw: Vec<Raw> =
        serde_json::from_str(s).with_context(|| format!("parse share list: {s}"))?;
    Ok(raw
        .into_iter()
        .map(|r| Session {
            pane_id: r.pane_id,
            target: r.target,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_list_plain_array() {
        let s = r#"[
  {
    "pane_id": "w8:p2X",
    "target": "w8:p2"
  },
  {
    "pane_id": "w1:p9",
    "target": "w1:p1"
  }
]"#;
        let v = parse_list(s).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].pane_id, "w8:p2X");
        assert_eq!(v[0].target, "w8:p2");
        assert_eq!(v[1].pane_id, "w1:p9");
    }

    #[test]
    fn parse_list_empty() {
        assert!(parse_list("[]").unwrap().is_empty());
    }

    #[test]
    fn parse_list_garbage_is_error() {
        assert!(parse_list("not json").is_err());
    }
}
