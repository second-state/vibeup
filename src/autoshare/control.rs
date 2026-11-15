//! autoshare 控制 socket(`~/.vibetty/autoshare.sock`)的协议 + stop/status 客户端。
//!
//! daemon 在这条 socket 上逐连接读一行 JSON 请求、回一行 JSON 应答;socket 文件
//! 本身也是探活标记:`connect` 结果三分 —— `Ok` = Running(有 daemon 在)，
//! `NotFound` = Absent(文件不在),`ConnectionRefused` = Stale(SIGKILL 留下的
//! 残留文件,可安全 unlink)。daemon 只在自己的子进程里 bind,所以「能连上」
//! 就等于「活着」,不需要再发探测请求确认。

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// stop/status 客户端的应答超时(daemon 本地应答,毫秒级)。
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    Stop,
    Status,
}

pub fn encode_request(r: Request) -> String {
    let cmd = match r {
        Request::Stop => "stop",
        Request::Status => "status",
    };
    json!({"cmd": cmd}).to_string() + "\n"
}

/// daemon 侧解析请求行;未知 cmd / 坏 JSON → Err(原样把错误回给客户端)。
pub fn parse_request(line: &str) -> Result<Request> {
    let v: Value = serde_json::from_str(line.trim()).context("bad JSON request")?;
    match v.get("cmd").and_then(Value::as_str) {
        Some("stop") => Ok(Request::Stop),
        Some("status") => Ok(Request::Status),
        other => bail!("unknown command {:?}", other.unwrap_or("(missing)")),
    }
}

/// stop 的应答行。
pub fn encode_reply_ok() -> String {
    json!({"ok": true, "stopping": true}).to_string() + "\n"
}

/// daemon 给坏请求的应答行。
pub fn encode_reply_err(msg: &str) -> String {
    json!({"ok": false, "error": msg}).to_string() + "\n"
}

/// status 里的一条 share 记录。
#[derive(Serialize, Clone, Debug)]
pub struct SharedEntry {
    pub target: String,
    pub pane_id: String,
    pub agent: String,
}

/// status 里的一条失败记录。
#[derive(Serialize, Clone, Debug)]
pub struct FailedEntry {
    pub target: String,
    pub error: String,
}

/// `vibetty broker` 子进程状态(best-effort:没起来也正常)。
#[derive(Serialize, Clone, Debug, Default)]
pub struct BrokerState {
    pub pid: Option<u32>,
    pub running: bool,
    pub exit_code: Option<i32>,
    pub port: Option<u16>,
}

/// herdr 订阅连接状态。
#[derive(Serialize, Clone, Debug, Default)]
pub struct HerdrState {
    pub connected: bool,
    pub reconnects: u64,
    pub events_seen: u64,
}

/// `autoshare status` 的完整报告(daemon 侧从内存态拼装,不碰任何子进程)。
#[derive(Serialize, Clone, Debug)]
pub struct StatusReport {
    pub ok: bool,
    pub pid: u32,
    pub version: &'static str,
    pub uptime_secs: u64,
    pub herdr: HerdrState,
    pub broker: BrokerState,
    pub shared: Vec<SharedEntry>,
    pub failed: Vec<FailedEntry>,
    pub share_in_progress: Option<String>,
}

/// status 的应答行:report 包在 `status` 键下,客户端按 `.status` 解。
pub fn encode_status(r: &StatusReport) -> String {
    json!({"ok": true, "status": r}).to_string() + "\n"
}

/// socket 文件探活(connect-only,不读写)。
pub fn probe(path: &Path) -> Result<Probe> {
    match UnixStream::connect(path) {
        Ok(_) => Ok(Probe::Running),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Probe::Absent),
        // 文件在但没人听 = SIGKILL 留下的残留
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => Ok(Probe::Stale),
        Err(e) => Err(e).with_context(|| format!("probing {}", path.display())),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    Running,
    Stale,
    Absent,
}

/// 给 daemon 发一个请求并读应答行。
fn request(path: &Path, r: Request) -> Result<Value> {
    let stream =
        UnixStream::connect(path).with_context(|| format!("connecting to {}", path.display()))?;
    stream.set_read_timeout(Some(REPLY_TIMEOUT))?;
    let mut reader = BufReader::new(&stream);
    (&stream).write_all(encode_request(r).as_bytes())?;
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        bail!("daemon closed the connection without replying");
    }
    serde_json::from_str(line.trim()).context("bad JSON reply from daemon")
}

/// 停 daemon(消息返回式,CLI / TUI 共用)。
pub fn stop_daemon() -> Result<String> {
    let Some(path) = crate::autoshare::socket_path() else {
        bail!("no home directory — cannot locate the control socket");
    };
    match probe(&path)? {
        Probe::Absent => Ok("autoshare: not running".into()),
        Probe::Stale => {
            let _ = std::fs::remove_file(&path);
            Ok("autoshare: not running (stale socket removed)".into())
        }
        Probe::Running => {
            let v = request(&path, Request::Stop)?;
            if v.get("ok").and_then(Value::as_bool) == Some(true) {
                Ok("autoshare: stopped".into())
            } else {
                bail!("unexpected stop reply: {v}");
            }
        }
    }
}

/// `vibeup autoshare stop`。
pub fn stop() -> Result<()> {
    println!("{}", stop_daemon()?);
    Ok(())
}

/// `vibeup autoshare status`。
pub fn status() -> Result<()> {
    let Some(path) = crate::autoshare::socket_path() else {
        bail!("no home directory — cannot locate the control socket");
    };
    match probe(&path)? {
        Probe::Absent => println!("autoshare: not running"),
        Probe::Stale => {
            let _ = std::fs::remove_file(&path);
            println!("autoshare: not running (stale socket removed)");
        }
        Probe::Running => {
            let v = request(&path, Request::Status)?;
            let report = v.get("status").context("status reply missing `status`")?;
            println!("{}", serde_json::to_string_pretty(report)?);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip() {
        assert_eq!(encode_request(Request::Stop), "{\"cmd\":\"stop\"}\n");
        assert_eq!(encode_request(Request::Status), "{\"cmd\":\"status\"}\n");
        assert_eq!(
            parse_request(&encode_request(Request::Stop)).unwrap(),
            Request::Stop
        );
        assert_eq!(
            parse_request(&encode_request(Request::Status)).unwrap(),
            Request::Status
        );
        assert!(parse_request("{\"cmd\":\"pause\"}").is_err());
        assert!(parse_request("{}").is_err());
        assert!(parse_request("not json").is_err());
    }

    #[test]
    fn status_envelope_holds_the_report_under_status_key() {
        let report = StatusReport {
            ok: true,
            pid: 42,
            version: "0.0.0",
            uptime_secs: 7,
            herdr: HerdrState {
                connected: true,
                reconnects: 1,
                events_seen: 3,
            },
            broker: BrokerState {
                pid: Some(43),
                running: true,
                exit_code: None,
                port: Some(8883),
            },
            shared: vec![SharedEntry {
                target: "w5:pS".into(),
                pane_id: "w5:pT".into(),
                agent: "claude".into(),
            }],
            failed: vec![FailedEntry {
                target: "w8:p2".into(),
                error: "boom".into(),
            }],
            share_in_progress: None,
        };
        let line = encode_status(&report);
        assert!(line.ends_with('\n'));
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["ok"], true);
        let s = &v["status"];
        assert_eq!(s["pid"], 42);
        assert_eq!(s["herdr"]["connected"], true);
        assert_eq!(s["broker"]["port"], 8883);
        assert_eq!(s["shared"][0]["target"], "w5:pS");
        assert_eq!(s["failed"][0]["error"], "boom");
    }

    #[test]
    fn reply_helpers() {
        assert_eq!(encode_reply_ok(), "{\"ok\":true,\"stopping\":true}\n");
        // serde_json 的 Map 是 BTreeMap:键按字母序输出
        assert_eq!(
            encode_reply_err("nope"),
            "{\"error\":\"nope\",\"ok\":false}\n"
        );
    }
}
