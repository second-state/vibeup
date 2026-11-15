//! herdr socket API 的最小 NDJSON 客户端(std 同步版,无 tokio)。
//!
//! 协议事实(herdr 0.9.5 / protocol 22,本机实测):
//! - 默认 socket `~/.config/herdr/herdr.sock`,env `HERDR_SOCKET_PATH` /
//!   `HERDR_CONFIG_ROOT` 可覆盖;newline-delimited JSON
//! - 普通 RPC **一连接一请求**(服务端答完即关);写完请求可以 half-close 写端
//! - `events.subscribe` 是唯一的长连接:发一行订阅请求,ack 之后事件按行推送
//!   —— **不能** half-close 写端(实测 SHUT_WR → ack 后立刻 EOF)
//! - 订阅项是 tagged enum 对象 `{"type":"pane.agent_detected"}`(裸字符串被拒);
//!   该事件也被复用于 agent 释放(多带 `released:true` / `final_status`)
//!
//! 纯函数(`build_request` / `parse_reply` / `parse_ack` / `parse_event` /
//! `agents_from_snapshot` / `is_shareable_status`)都是可单测的协议核心。

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// 订阅的事件类型(agent 出现;释放也走同一事件,多带 `released:true`)。
pub const SUB_AGENT_DETECTED: &str = "pane.agent_detected";

/// 一次性 RPC 的读超时(herdr 答完就关,正常都是毫秒级)。
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

static REQ_ID: AtomicU64 = AtomicU64::new(1);

fn req_id() -> String {
    format!(
        "vibeup-autoshare-{}",
        REQ_ID.fetch_add(1, Ordering::Relaxed)
    )
}

/// herdr socket 路径:`HERDR_SOCKET_PATH` > `$HERDR_CONFIG_ROOT/herdr.sock` >
/// `~/.config/herdr/herdr.sock`。
pub fn default_socket_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("HERDR_SOCKET_PATH") {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    let root = match std::env::var("HERDR_CONFIG_ROOT") {
        Ok(r) if !r.is_empty() => PathBuf::from(r),
        _ => dirs::home_dir()
            .context("no home directory — cannot locate the herdr socket")?
            .join(".config")
            .join("herdr"),
    };
    Ok(root.join("herdr.sock"))
}

/// 构造一行 RPC 请求(含结尾 `\n`)。
pub fn build_request(id: &str, method: &str, params: &Value) -> String {
    json!({"id": id, "method": method, "params": params}).to_string() + "\n"
}

/// 构造一行订阅请求(订阅项是对象形式,不是裸字符串)。
pub fn build_subscribe_request(id: &str, sub_type: &str) -> String {
    json!({
        "id": id,
        "method": "events.subscribe",
        "params": {"subscriptions": [{"type": sub_type}]},
    })
    .to_string()
        + "\n"
}

/// 解析一行应答:错误封套 → Err;正常 → `result` 字段(可能为 null)。
pub fn parse_reply(line: &str, method: &str) -> Result<Value> {
    let v: Value = serde_json::from_str(line.trim())
        .with_context(|| format!("bad JSON reply for {method}: {line:?}"))?;
    if let Some(err) = v.get("error") {
        bail!("herdr {method} error: {err}");
    }
    Ok(v.get("result").cloned().unwrap_or(Value::Null))
}

/// 校验订阅 ack:必须是 `{"result":{"type":"subscription_started"}}`。
pub fn parse_ack(line: &str) -> Result<()> {
    let v = parse_reply(line, "events.subscribe")?;
    if v.get("type").and_then(Value::as_str) == Some("subscription_started") {
        Ok(())
    } else {
        bail!("unexpected subscription ack: {v}")
    }
}

/// `session.snapshot` 里的一条 agent(agents 数组元素,裁剪到需要的字段)。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AgentInfo {
    pub pane_id: String,
    pub agent: String,
    pub agent_status: String,
    pub workspace_id: String,
    pub title: String,
}

/// 从 snapshot 对象里抽 agent 全集(`snapshot.agents[]`)。
pub fn agents_from_snapshot(snapshot: &Value) -> Vec<AgentInfo> {
    let Some(agents) = snapshot.get("agents").and_then(Value::as_array) else {
        return Vec::new();
    };
    agents
        .iter()
        .filter_map(|a| {
            Some(AgentInfo {
                pane_id: a.get("pane_id")?.as_str()?.to_string(),
                agent: a.get("agent")?.as_str()?.to_string(),
                agent_status: a
                    .get("agent_status")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                workspace_id: a
                    .get("workspace_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                title: a
                    .get("terminal_title_stripped")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .collect()
}

/// 这个状态值是否值得 share("" / none / exited / released 都不算)。
pub fn is_shareable_status(status: &str) -> bool {
    let s = status.to_lowercase();
    !(s.is_empty() || s == "none" || s == "exited" || s == "released")
}

/// 一行推送事件。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HerdrEvent {
    /// 新 agent 出现(released 缺省 / false)。
    AgentDetected {
        pane_id: String,
        agent: String,
        workspace_id: String,
    },
    /// agent 释放(released:true)—— share-only 模式只记日志,不回收。
    AgentReleased { pane_id: String },
    /// 其他事件 / 无法解析的行:忽略。
    Other,
}

/// 解析一行推送。解析不出结构 → [`HerdrEvent::Other`]。
pub fn parse_event(line: &str) -> HerdrEvent {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        return HerdrEvent::Other;
    };
    // 推送行是**裸封套** `{"event":"pane_agent_detected","data":{...}}`
    // (实测;不带 RPC 的 result 包装。防御起见 `result` 包装的形态也认)。
    let envelope = v.get("result").unwrap_or(&v);
    let Some(event) = envelope.get("event").and_then(Value::as_str) else {
        return HerdrEvent::Other;
    };
    let data = envelope.get("data").cloned().unwrap_or(Value::Null);
    if event == "pane_agent_detected" {
        let Some(pane_id) = data.get("pane_id").and_then(Value::as_str) else {
            return HerdrEvent::Other;
        };
        let released = data
            .get("released")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if released {
            return HerdrEvent::AgentReleased {
                pane_id: pane_id.to_string(),
            };
        }
        return HerdrEvent::AgentDetected {
            pane_id: pane_id.to_string(),
            agent: data
                .get("agent")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string(),
            workspace_id: data
                .get("workspace_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        };
    }
    HerdrEvent::Other
}

/// herdr socket 客户端(路径不可变,可跨线程共享)。
pub struct HerdrClient {
    socket: PathBuf,
}

impl HerdrClient {
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    /// 一次性 RPC:连接 → 写一行(half-close 写端,服务端收到 EOF 也不影响应答)
    /// → 读一行。一连接一请求是 herdr 的协议事实。
    pub fn call(&self, method: &str, params: &Value) -> Result<Value> {
        let stream = UnixStream::connect(&self.socket)
            .with_context(|| format!("connecting to herdr socket {}", self.socket.display()))?;
        stream.set_read_timeout(Some(CALL_TIMEOUT))?;
        let mut reader = BufReader::new(&stream);
        (&stream).write_all(build_request(&req_id(), method, params).as_bytes())?;
        (&stream)
            .shutdown(std::net::Shutdown::Write)
            .context("half-close herdr request")?;
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            bail!("herdr closed the connection without replying ({method})");
        }
        parse_reply(&line, method)
    }

    /// 当前 agent 全集(`session.snapshot` → `snapshot.agents[]`)。
    pub fn snapshot_agents(&self) -> Result<Vec<AgentInfo>> {
        let result = self.call("session.snapshot", &json!({}))?;
        let snapshot = result
            .get("snapshot")
            .context("session.snapshot reply missing `snapshot`")?;
        Ok(agents_from_snapshot(snapshot))
    }

    /// 打开 `pane.agent_detected` 订阅(长驻连接, [`Subscription::recv_event`]
    /// 逐事件读)。**全程不 half-close 写端** —— 那会让 herdr 立刻断开订阅。
    pub fn subscribe_agent_detected(&self) -> Result<Subscription> {
        let stream = UnixStream::connect(&self.socket)
            .with_context(|| format!("connecting to herdr socket {}", self.socket.display()))?;
        // 三个 dup:读、写、interrupt(stop 时 shutdown 打断阻塞读)。
        let interrupt = stream
            .try_clone()
            .context("dup herdr socket for interrupt")?;
        let mut wr = stream.try_clone().context("dup herdr socket for write")?;
        wr.write_all(build_subscribe_request(&req_id(), SUB_AGENT_DETECTED).as_bytes())?;
        wr.flush()?;
        drop(wr); // 只关这个 dup,底层连接靠 reader/interrupt 撑着

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            bail!("herdr closed the connection before the subscription ack");
        }
        parse_ack(&line)?;
        Ok(Subscription { reader, interrupt })
    }
}

/// 长驻订阅连接。`recv_event` 阻塞读;`shutdown` 用 dup fd 的 shutdown(Both)
/// 让阻塞中的 read_line 立刻返回 0 —— std 下唯一不依赖信号打断阻塞读的办法。
pub struct Subscription {
    reader: BufReader<UnixStream>,
    interrupt: UnixStream,
}

impl Subscription {
    /// 读下一行推送。`Ok(None)` = EOF(herdr 重启 / 我们 shutdown 了 socket)。
    pub fn recv_event(&mut self) -> Result<Option<HerdrEvent>> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(None);
        }
        Ok(Some(parse_event(&line)))
    }

    /// 给 daemon 存一份 interrupt dup(daemon stop 时对它 shutdown)。
    pub fn try_clone_interrupt(&self) -> std::io::Result<UnixStream> {
        self.interrupt.try_clone()
    }

    /// 打断阻塞中的 [`Self::recv_event`](底层 socket 全双工 shutdown)。
    pub fn shutdown(&self) {
        let _ = self.interrupt.shutdown(std::net::Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn subscribe_request_is_object_form_with_newline() {
        // 裸字符串订阅会被 herdr 拒(internally tagged enum),必须锁死对象形式。
        assert_eq!(
            build_subscribe_request("vibeup-1", SUB_AGENT_DETECTED),
            "{\"id\":\"vibeup-1\",\"method\":\"events.subscribe\",\"params\":{\"subscriptions\":[{\"type\":\"pane.agent_detected\"}]}}\n"
        );
    }

    #[test]
    fn parse_reply_unwraps_result_and_maps_errors() {
        let ok = parse_reply(
            "{\"id\":\"x\",\"result\":{\"snapshot\":{\"agents\":[]}}}",
            "session.snapshot",
        )
        .unwrap();
        assert_eq!(ok, json!({"snapshot":{"agents":[]}}));

        let err = parse_reply(
            "{\"id\":\"x\",\"error\":{\"code\":\"invalid_request\",\"message\":\"nope\"}}",
            "session.snapshot",
        )
        .unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");

        assert!(parse_reply("not json", "ping").is_err());
    }

    #[test]
    fn parse_ack_requires_subscription_started() {
        assert!(parse_ack("{\"id\":\"x\",\"result\":{\"type\":\"subscription_started\"}}").is_ok());
        assert!(parse_ack("{\"id\":\"x\",\"result\":{\"type\":\"other\"}}").is_err());
        assert!(parse_ack("{\"id\":\"x\",\"error\":{\"message\":\"bad\"}}").is_err());
    }

    #[test]
    fn parse_event_splits_detected_and_released() {
        // 实测推送行 = **裸封套**(无 result 包装);released 缺省 = 新 agent
        let ev = parse_event(
            "{\"data\":{\"agent\":\"claude\",\"pane_id\":\"w5:pS\",\"type\":\"pane_agent_detected\",\"workspace_id\":\"w5\"},\"event\":\"pane_agent_detected\"}",
        );
        assert_eq!(
            ev,
            HerdrEvent::AgentDetected {
                pane_id: "w5:pS".into(),
                agent: "claude".into(),
                workspace_id: "w5".into(),
            }
        );

        // 防御形态:带 result 包装的同一事件也能解
        let ev = parse_event(
            "{\"result\":{\"event\":\"pane_agent_detected\",\"data\":{\"agent\":\"claude\",\"pane_id\":\"w6:p1\",\"workspace_id\":\"w6\"}}}",
        );
        assert_eq!(
            ev,
            HerdrEvent::AgentDetected {
                pane_id: "w6:p1".into(),
                agent: "claude".into(),
                workspace_id: "w6".into(),
            }
        );

        // 实测释放事件:同一事件类型 + released:true
        let ev = parse_event(
            "{\"data\":{\"agent\":\"claude\",\"pane_id\":\"w5:pS\",\"released\":true,\"final_status\":\"unknown\",\"type\":\"pane_agent_detected\",\"workspace_id\":\"w5\"},\"event\":\"pane_agent_detected\"}",
        );
        assert_eq!(
            ev,
            HerdrEvent::AgentReleased {
                pane_id: "w5:pS".into()
            }
        );

        assert_eq!(parse_event("garbage"), HerdrEvent::Other);
        assert_eq!(
            parse_event("{\"event\":\"pane_created\",\"data\":{}}"),
            HerdrEvent::Other
        );
    }

    #[test]
    fn agents_from_snapshot_takes_the_agents_array() {
        let snapshot = json!({
            "agents": [
                {"agent":"claude","agent_status":"idle","pane_id":"w5:pS","workspace_id":"w5",
                 "terminal_title_stripped":"ota-merge"},
                {"agent":"codex","pane_id":"w8:p2","workspace_id":"w8"}, // 无 status → unknown
            ],
            "panes": [{"pane_id":"w3:p2"}], // 无 agent 字段 → 跳过
        });
        let agents = agents_from_snapshot(&snapshot);
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].pane_id, "w5:pS");
        assert_eq!(agents[0].agent_status, "idle");
        assert_eq!(agents[0].title, "ota-merge");
        assert_eq!(agents[1].agent_status, "unknown");
        assert_eq!(agents_from_snapshot(&json!({})), Vec::new());
    }

    #[test]
    fn shareable_status_filters_non_states() {
        for ok in ["idle", "working", "blocked", "done", "IDLE", "unknown"] {
            // "unknown" 算可 share(herdr 对未识别 agent 标 unknown)
            assert!(is_shareable_status(ok), "{ok}");
        }
        for bad in ["", "none", "exited", "released"] {
            assert!(!is_shareable_status(bad), "{bad}");
        }
    }

    #[test]
    fn default_socket_path_honors_env_override() {
        // edition 2024:set_var 是 unsafe;这里串行改、其他测试不读这个 env。
        let saved = std::env::var("HERDR_SOCKET_PATH").ok();
        unsafe { std::env::set_var("HERDR_SOCKET_PATH", "/tmp/herdr-test.sock") };
        assert_eq!(
            default_socket_path().unwrap(),
            PathBuf::from("/tmp/herdr-test.sock")
        );
        unsafe { std::env::set_var("HERDR_SOCKET_PATH", "") };
        // 空值 → 落回默认路径
        assert!(default_socket_path().unwrap().ends_with("herdr.sock"));
        if let Some(v) = saved {
            unsafe { std::env::set_var("HERDR_SOCKET_PATH", v) };
        } else {
            unsafe { std::env::remove_var("HERDR_SOCKET_PATH") };
        }
    }
}
