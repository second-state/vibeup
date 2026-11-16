//! `vibeup webui` —— 本地 WebSocket server,配对 GitHub Pages 上的浏览器向导页。
//!
//! 职责分工:页面(https)负责 UI、按键捕捉与 **Web Bluetooth 直连设备**
//! (读写 CONFIG/KEYMAP 特征值);本 server 只做需要本机权限的事:
//! 包安装、agent 插件安装(CLI 命令)、vibetty broker 落盘、宿主信息查询。
//!
//! 安全:仅监听 127.0.0.1;启动生成一次性 token,首帧 `hello` 校验,
//! 防任意网页驱动安装。单客户端接替制:新连接顶掉旧连接(刷新友好)。

use crate::hostinfo::{
    broker_url_valid, local_ips, set_vibetty_broker, tailscale_ip, vibetty_broker,
};
use crate::install;
use crate::pkg::{self, Pkg};
use crate::plugins::{SharedLines, cli_on_path, install_plugin_to};
use crate::status;
use anyhow::{Context, Result};
use axum::Router;
use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// 单个事件/响应出站帧(tx 端由每连接的 writer 任务消费)。
type EventTx = mpsc::UnboundedSender<Value>;

#[derive(Clone)]
struct AppState {
    token: String,
    dir: Option<PathBuf>,
    dry_run: bool,
    /// 当前活着的页面连接(单客户端,新顶旧)。
    client: Arc<Mutex<Option<EventTx>>>,
    /// 长操作进行中标志(安装/插件/status 查询)。AtomicBool 简单挡并发。
    busy: Arc<AtomicBool>,
}

pub(crate) fn run(
    dir: Option<PathBuf>,
    dry_run: bool,
    port: Option<u16>,
    pages_url: &str,
) -> Result<()> {
    let token = uuid::Uuid::new_v4().to_string();
    let state = AppState {
        token,
        dir,
        dry_run,
        client: Arc::new(Mutex::new(None)),
        busy: Arc::new(AtomicBool::new(false)),
    };

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port.unwrap_or(0)))
            .await
            .context("failed to bind 127.0.0.1")?;
        let addr = listener.local_addr()?;

        let pages = pages_url.trim_end_matches('/');
        println!("vibeup webui — local setup server");
        println!(
            "  pairing URL : {pages}?port={}&token={}",
            addr.port(),
            state.token
        );
        println!("  raw endpoint: ws://127.0.0.1:{}/ws", addr.port());
        println!("  Ctrl-C to stop.");

        let app = Router::new()
            .route("/", get(root_hint))
            .route("/ws", get(ws_upgrade))
            .with_state(state);
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
                println!("shutting down…");
            })
            .await?;
        Ok::<(), anyhow::Error>(())
    })
}

async fn root_hint() -> impl IntoResponse {
    "vibeup webui is running. Open the pairing URL printed by `vibeup webui`."
}

async fn ws_upgrade(State(state): State<AppState>, upgrade: WebSocketUpgrade) -> impl IntoResponse {
    upgrade.on_upgrade(move |socket| handle_socket(state, socket))
}

async fn handle_socket(state: AppState, mut socket: WebSocket) {
    println!("· socket connected, waiting for hello…");
    // 握手:10s 内首帧必须 hello + 正确 token。
    let first = tokio::time::timeout(std::time::Duration::from_secs(10), socket.recv()).await;
    let Ok(Some(Ok(Message::Text(text)))) = first else {
        let _ = socket
            .send(Message::Close(Some(CloseFrame {
                code: 4002,
                reason: "timed out waiting for hello".into(),
            })))
            .await;
        return;
    };
    let hello: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            let _ = socket
                .send(Message::Close(Some(CloseFrame {
                    code: 1002,
                    reason: "bad hello".into(),
                })))
                .await;
            return;
        }
    };
    if hello.get("type").and_then(|t| t.as_str()) != Some("hello")
        || hello.get("token").and_then(|t| t.as_str()) != Some(state.token.as_str())
    {
        println!("✗ rejected a connection with a bad token");
        let _ = socket
            .send(Message::Close(Some(CloseFrame {
                code: 4003,
                reason: "bad token".into(),
            })))
            .await;
        return;
    }

    // 出站通道:reader 任务收命令,worker 任务借 tx 发事件,writer 消费。
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    // 单客户端接替:旧连接收 superseded 后 writer 退出 → socket 关闭。
    if let Some(old) = state.client.lock().unwrap().replace(tx.clone()) {
        let _ = old.send(json!({
            "type": "event",
            "event": "superseded",
            "id": null,
            "data": {},
        }));
    }
    hello_ok(&tx, &state);

    // 拆成读写两半:writer 消费出站队列,reader 消费页面命令。
    let (mut ws_tx, mut ws_rx) = socket.split();

    // writer:把出站帧写回 socket;superseded → 发完即断。
    let writer = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            let superseded = frame.get("event").and_then(|e| e.as_str()) == Some("superseded");
            if ws_tx
                .send(Message::Text(frame.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
            if superseded {
                let _ = ws_tx
                    .send(Message::Close(Some(CloseFrame {
                        code: 4000,
                        reason: "superseded".into(),
                    })))
                    .await;
                break;
            }
        }
    });

    // reader:逐帧取命令并分发(长操作 spawn,不阻塞后续帧)。
    while let Some(Ok(msg)) = ws_rx.next().await {
        let Message::Text(text) = msg else {
            continue;
        };
        let cmd: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                let _ = tx.send(resp(None, false, "bad_request", &format!("bad json: {e}")));
                continue;
            }
        };
        let id = cmd.get("id").and_then(|v| v.as_str()).map(String::from);
        let name = cmd
            .get("cmd")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        println!(
            "→ {} {}",
            name,
            cmd.get("params").map(|p| p.to_string()).unwrap_or_default()
        );
        dispatch(
            state.clone(),
            tx.clone(),
            id,
            name,
            cmd.get("params").cloned().unwrap_or(json!({})),
        );
    }
    // reader 结束(页面断开):让出 writer 收尾。
    let _ = writer.await;
    // 若还是登记的客户端,摘掉。
    // (比较不了 sender 本身,直接清空;新连接已顶掉时这里也无害。)
}

fn hello_ok(tx: &EventTx, state: &AppState) {
    let _ = tx.send(json!({
        "type": "resp",
        "id": "hello",
        "ok": true,
        "result": {
            "version": env!("CARGO_PKG_VERSION"),
            "dryRun": state.dry_run,
            "installDir": pkg::install_dir(state.dir.as_deref()),
        },
    }));
}

fn resp(id: Option<&str>, ok: bool, code_or_err: &str, msg: &str) -> Value {
    if ok {
        json!({
            "type": "resp",
            "id": id,
            "ok": true,
            "result": serde_json::from_str::<Value>(msg)
                .unwrap_or_else(|_| Value::String(msg.to_string())),
        })
    } else {
        json!({
            "type": "resp",
            "id": id,
            "ok": false,
            "error": msg,
            "code": code_or_err,
        })
    }
}

/// 分发命令:轻量命令就地执行,长操作 spawn(借 guard 防 busy 并发)。
fn dispatch(state: AppState, tx: EventTx, id: Option<String>, cmd: String, params: Value) {
    macro_rules! send {
        ($v:expr) => {{
            let _ = tx.send($v);
        }};
    }
    match cmd.as_str() {
        "ping" => send!(json!({
            "type": "resp", "id": id, "ok": true, "result": {"pong": true},
        })),
        "host.tailscaleIp" => send!(json!({
            "type": "resp", "id": id, "ok": true,
            "result": {"ip": tailscale_ip(), "installed": crate::hostinfo::tailscale_installed()},
        })),
        "host.lanIps" => send!(json!({
            "type": "resp", "id": id, "ok": true,
            "result": {"ips": local_ips()},
        })),
        "vibetty.broker" => send!(json!({
            "type": "resp", "id": id, "ok": true,
            "result": {"broker": vibetty_broker()},
        })),
        "vibetty.setBroker" => {
            let url = params
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !broker_url_valid(&url) {
                send!(resp(
                    id.as_deref(),
                    false,
                    "bad_request",
                    "not a valid broker URL — mqtt:// mqtts:// ws:// or wss:// with a host"
                ));
                return;
            }
            match set_vibetty_broker(&url) {
                Ok(()) => send!(json!({
                    "type": "resp", "id": id, "ok": true,
                    "result": {"broker": url},
                })),
                Err(e) => send!(resp(
                    id.as_deref(),
                    false,
                    "internal",
                    &format!("vibetty config: {e:#}")
                )),
            }
        }
        "keymap.build" => {
            // 纯函数:页面拿 JSON payload 自己经 Web Bluetooth 写 KEYMAP。
            let key = params.get("key").and_then(|v| v.as_str()).unwrap_or("MIC");
            let binding = params.get("binding").and_then(|v| v.as_str()).unwrap_or("");
            if binding.is_empty() {
                send!(resp(
                    id.as_deref(),
                    false,
                    "bad_request",
                    "binding must not be empty"
                ));
                return;
            }
            send!(json!({
                "type": "resp", "id": id, "ok": true,
                "result": {"payload": crate::config_cmd::build_keymap_config(key, binding)},
            }));
        }
        "plugins.detect" => send!(json!({
            "type": "resp", "id": id, "ok": true,
            "result": {"claude": cli_on_path("claude"), "codex": cli_on_path("codex")},
        })),
        "plugin.cancel" => {
            // 不占 busy 锁:就是要在长操作卡死时杀它
            let killed = crate::plugins::kill_active_plugin();
            println!("→ plugin.cancel (killed={killed})");
            send!(json!({
                "type": "resp", "id": id, "ok": true,
                "result": {"cancelled": killed},
            }));
        }
        "status.get" | "pkg.install" | "pkg.uninstall" | "pkg.installScript" | "plugin.install" => {
            if state.busy.swap(true, Ordering::SeqCst) {
                send!(resp(
                    id.as_deref(),
                    false,
                    "busy",
                    "another operation is still running"
                ));
                return;
            }
            let state2 = state.clone();
            let id2 = id.clone();
            tokio::spawn(async move {
                let r = run_long_op(&state2, &tx, id2.as_deref(), &cmd, &params).await;
                state2.busy.store(false, Ordering::SeqCst);
                match r {
                    Ok(result) => send!(json!({
                        "type": "resp", "id": id, "ok": true, "result": result,
                    })),
                    Err(e) => send!(resp(id.as_deref(), false, "internal", &format!("{e:#}"))),
                }
            });
        }
        other => send!(resp(
            id.as_deref(),
            false,
            "bad_request",
            &format!("unknown command `{other}`"),
        )),
    }
}

/// 长操作:status 查询(网络)、包安装/卸载(下载)、插件安装(CLI 命令)。
async fn run_long_op(
    state: &AppState,
    tx: &EventTx,
    id: Option<&str>,
    cmd: &str,
    params: &Value,
) -> Result<Value> {
    let dir = pkg::install_dir(state.dir.as_deref());
    match cmd {
        "status.get" => {
            let rows = tokio::task::spawn_blocking(move || status::collect(&dir))
                .await
                .unwrap_or_default();
            Ok(json!({ "packages": rows }))
        }
        "pkg.install" | "pkg.uninstall" => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let p = pkg::find(name)?;
            let state = state.clone();
            let tx2 = tx.clone();
            let id2 = id.clone().map(String::from).unwrap_or_default();
            let tag = params.get("tag").and_then(|v| v.as_str()).map(String::from);
            let pre = params.get("pre").and_then(|v| v.as_bool()).unwrap_or(false);
            let uninstall = cmd == "pkg.uninstall";
            let dry = state.dry_run;
            let dir2 = dir.clone();
            let log = tokio::task::spawn_blocking(move || -> Result<String> {
                match uninstall {
                    false => {
                        let tx3 = tx2.clone();
                        let id3 = id2.clone();
                        let tick = move |d: u64, t: Option<u64>| {
                            let _ = tx3.send(json!({
                                "type": "event", "event": "progress",
                                "id": id3, "data": {"downloaded": d, "total": t},
                            }));
                        };
                        let mut log = install::install(p, tag, pre, &dir2, dry, false, &tick)?;
                        // vibekeys 装完跑 install-script(jump 脚本),demo/dry-run 不跑。
                        if p.name == "vibekeys" && !dry {
                            let bin = dir2.join(p.link_name());
                            install::run_install_script(&bin, &mut log);
                        }
                        Ok(log)
                    }
                    true => install::uninstall(p, &dir2, dry),
                }
            })
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))??;
            for line in log.lines() {
                let _ = tx.send(json!({
                    "type": "event", "event": "log",
                    "id": id, "data": {"line": line},
                }));
            }
            Ok(json!({ "log": log }))
        }
        "pkg.installScript" => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let p: &Pkg = pkg::find(name)?;
            let bin = dir.join(p.link_name());
            let mut log = String::new();
            install::run_install_script(&bin, &mut log);
            Ok(json!({ "log": log }))
        }
        "plugin.install" => {
            let cli = params.get("cli").and_then(|v| v.as_str()).unwrap_or("");
            let (action, label) = match cli {
                "claude" => ("install", "Claude Code"),
                "codex" => ("add", "Codex"),
                other => anyhow::bail!("unsupported cli `{other}` — use claude or codex"),
            };
            let sink: SharedLines = Arc::new(Mutex::new(Vec::new()));
            let tx2 = tx.clone();
            let id2 = id.map(String::from).unwrap_or_default();
            // 转发线程:sink 每 150ms 快照一次,新行作为 raw log 事件推出
            // (保留 ANSI,页面用 xterm.js 渲染彩色终端)。
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let forward = {
                let sink = sink.clone();
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut sent = 0usize;
                    loop {
                        if stop.load(std::sync::atomic::Ordering::Relaxed) {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(150));
                        let lines = sink.lock().unwrap();
                        while sent < lines.len() {
                            let _ = tx2.send(json!({
                                "type": "event", "event": "log",
                                "id": id2, "data": {"line": lines[sent], "raw": true},
                            }));
                            sent += 1;
                        }
                    }
                })
            };
            let cli = cli.to_string();
            let summary = tokio::task::spawn_blocking(move || {
                install_plugin_to(&cli, action, label, &sink, true)
            })
            .await
            .unwrap_or_else(|e| format!("plugin install failed: {e}"));
            // 收尾:最后一批行推完再停转发线程。
            std::thread::sleep(std::time::Duration::from_millis(200));
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            forward.join().ok();
            Ok(json!({ "summary": summary }))
        }
        other => anyhow::bail!("unknown long op `{other}`"),
    }
}
