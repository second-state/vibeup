//! autoshare daemon 主体:控制 socket accept 循环(main)+ herdr 订阅
//! (reader)+ share 执行(worker)+ broker 子进程 watcher,四线程共享
//! [`State`];`-d` 的 detach 启动器也在这(vibegate 式自我重 exec)。
//!
//! 线程间只靠 `State` 上的小粒度锁 + `AtomicBool shutdown` 协调:
//! - 锁只在 push/pop 一瞬间持有,绝不跨 `share_manager::share()`(阻塞子进程)持有
//! - stop 流程:控制连接置 shutdown → shutdown 订阅 socket 的 dup 打断 reader 的
//!   阻塞读 → 回 ack → main 收尾(unlink socket → kill broker)→ 进程退出回收线程
//!   (不做 join:卡在子进程里的 worker 由进程退出兜底,share 本身幂等无害)

use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::broker::{BrokerHandle, wait_for_port};
use super::control::{
    FailedEntry, Probe, Request, SharedEntry, StatusReport, encode_reply_err, encode_reply_ok,
    encode_status, parse_request,
};
use super::herdr_client::{HerdrClient, HerdrEvent, default_socket_path, is_shareable_status};
use super::{log_path, socket_path};

/// detached 模式下父进程给子进程的日志路径标记;子进程据此把日志写文件而非 stdio。
pub const LOG_ENV: &str = "VIBEUP_AUTOSHARE_LOG";

/// accept 轮询间隔(listener 是 nonblocking 的)。
const ACCEPT_POLL: Duration = Duration::from_millis(200);
/// 重连退避封顶。
const BACKOFF_MAX: Duration = Duration::from_secs(15);
/// broker 端口就绪等待上限(超时也不报错,broker 是 best-effort)。
const BROKER_WAIT: Duration = Duration::from_secs(2);

// ===== 日志 =====

/// 追加式日志:detached 模式写文件,前台写 stderr。`write` 一次一行
/// (`O_APPEND` 下单行原子,多线程并发写不撕裂)。
pub struct Log {
    file: Option<std::fs::File>,
}

impl Log {
    pub fn from_env() -> Result<Self> {
        match std::env::var(LOG_ENV) {
            Ok(p) if !p.is_empty() => Self::open(Path::new(&p)),
            _ => Ok(Self { file: None }),
        }
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("open log {}", path.display()))?;
        Ok(Self { file: Some(file) })
    }

    pub fn write(&self, level: &str, msg: &str) {
        let line = format!("[{} {level}] {msg}\n", now_utc_rfc3339_millis());
        match self.file.as_ref() {
            // `Write` 走 `impl Write for &File`:可变绑定的 `&File` 即可拿到
            // `&mut &File` 接收者(edition 2024 的 match 里不能直接 `Some(mut f)`)
            Some(f) => {
                let mut f = f;
                let _ = f.write_all(line.as_bytes());
            }
            None => {
                eprint!("{line}");
            }
        }
    }
}

/// 当前 UTC 时间,RFC3339 毫秒精度(std 没有日期格式化,手写 civil-from-days)。
pub fn now_utc_rfc3339_millis() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format_rfc3339_millis(d)
}

/// unix 纪元起的时长 → `1970-01-01T00:00:00.000Z` 形态。
pub fn format_rfc3339_millis(d: Duration) -> String {
    let secs = d.as_secs();
    let millis = d.subsec_millis();
    let (h, m, s) = ((secs / 3600) % 24, (secs % 3600) / 60, secs % 60);
    let (y, mo, dy) = civil_from_days((secs / 86400) as i64);
    format!("{y:04}-{mo:02}-{dy:02}T{h:02}:{m:02}:{s:02}.{millis:03}Z")
}

/// Howard Hinnant 的 civil_from_days:1970-01-01 = day 0。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ===== 共享状态 =====

/// daemon 共享态:每个锁只包最小数据,锁绝不跨阻塞调用持有。
struct State {
    /// Arc 是为了让各线程(clone 后)都能读到同一个标志。
    shutdown: Arc<AtomicBool>,
    broker: Mutex<Option<Arc<BrokerHandle>>>,
    /// reader 线程存的订阅 socket dup;stop 时对它 shutdown(Both) 打断阻塞读。
    herdr_interrupt: Mutex<Option<UnixStream>>,
    herdr: Mutex<super::control::HerdrState>,
    shared: Mutex<Vec<SharedEntry>>,
    failed: Mutex<Vec<FailedEntry>>,
    share_in_progress: Mutex<Option<String>>,
    /// 本 daemon 已经 share 过的 target(与 `share_manager::list()` 双层去重)。
    mine: Mutex<HashSet<String>>,
    started_at: Instant,
}

type SharedState = Arc<State>;

/// reader → worker 的 share 任务。
struct ShareJob {
    pane_id: String,
    agent: String,
}

/// 双层去重:不在现 share 列表里(可能 TUI 手动 share 过),也不是本轮 share 过的。
pub fn should_share(
    target: &str,
    listed: &[crate::share_manager::Session],
    mine: &HashSet<String>,
) -> bool {
    !listed.iter().any(|s| s.target == target) && !mine.contains(target)
}

/// 重连退避:1s, 2s, 4s, 8s, 15s, 15s, …
pub fn backoff(attempt: u32) -> Duration {
    let secs = 1u64.checked_shl(attempt.min(4)).unwrap_or(16);
    Duration::from_secs(secs.min(BACKOFF_MAX.as_secs()))
}

/// 启动前置检查(父进程 `-d` 时错误直接可见;daemon 内部再跑一遍兜底)。
pub fn precheck() -> Result<()> {
    let sock = default_socket_path()?;
    anyhow::ensure!(
        sock.exists(),
        "herdr socket {} not found — is herdr running?",
        sock.display()
    );
    anyhow::ensure!(
        crate::share_manager::herdr::installed(),
        "herdr CLI not found on PATH"
    );
    anyhow::ensure!(
        crate::share_manager::herdr::plugin_installed().unwrap_or(false),
        "vibetty is not a herdr plugin — install with `herdr plugin install second-state/vibetty`"
    );
    anyhow::ensure!(
        crate::terminal::on_path("vibetty"),
        "vibetty not found on PATH — run `vibeup install vibetty`"
    );
    Ok(())
}

// ===== 入口 =====

/// `vibeup autoshare -d`:探活 → precheck → 重 exec 自己(detached)→ 确认轮询。
pub fn spawn_detached() -> Result<String> {
    use std::os::unix::process::CommandExt;

    let Some(sock) = socket_path() else {
        bail!("no home directory — cannot locate the control socket");
    };
    let Some(log_file) = log_path() else {
        bail!("no home directory — cannot locate the log path");
    };

    match super::control::probe(&sock)? {
        Probe::Running => {
            return Ok("autoshare is already running — try `vibeup autoshare status`".into());
        }
        // SIGKILL 残留:清掉再启动
        Probe::Stale => {
            let _ = std::fs::remove_file(&sock);
        }
        Probe::Absent => {}
    }

    // 错误要在父进程可见:全部 precheck 完才 spawn
    precheck()?;

    if let Some(dir) = log_file.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file)
        .with_context(|| format!("open log {}", log_file.display()))?;
    let err = out.try_clone().context("clone log file handle")?;

    let exe = std::env::current_exe().context("locate current executable")?;
    Command::new(&exe)
        .arg("autoshare")
        .env(LOG_ENV, &log_file)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        // 脱离 shell 的进程组:终端 Ctrl-C / 挂断波及不到 daemon
        .process_group(0)
        .spawn()
        .context("spawn detached daemon")?;

    // 确认轮询:socket 文件出现 = bind 成功 = daemon 真起来了
    let deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < deadline {
        if sock.exists() {
            return Ok(format!(
                "autoshare started in background (log: {})",
                log_file.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(format!(
        "autoshare did not open its control socket yet — check the log at {}",
        log_file.display()
    ))
}

/// daemon 前台主体(`vibeup autoshare`;detached 子进程也走这,只是日志写文件)。
pub fn run() -> Result<()> {
    let log = Arc::new(Log::from_env()?);
    let Some(sock) = socket_path() else {
        bail!("no home directory — cannot locate the control socket");
    };

    // bind = 原子抢占点:`-d` 竞争的输家在这里落败
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // 文件在:活 daemon 还是残留?能连上 = 活的,输掉竞争的是我们
            match super::control::probe(&sock) {
                Ok(Probe::Running) => {
                    log.write(
                        "INFO",
                        "another daemon already owns the control socket — exiting",
                    );
                    return Ok(());
                }
                _ => {
                    let _ = std::fs::remove_file(&sock);
                    UnixListener::bind(&sock).context("bind control socket after stale cleanup")?
                }
            }
        }
        Err(e) => return Err(e).context("bind control socket"),
    };

    log.write(
        "INFO",
        &format!(
            "autoshare daemon started (pid {}, socket {}, log env: {})",
            std::process::id(),
            sock.display(),
            std::env::var(LOG_ENV).is_ok()
        ),
    );

    if let Err(e) = precheck() {
        let _ = std::fs::remove_file(&sock);
        log.write("ERROR", &format!("precheck failed: {e:#}"));
        return Err(e);
    }

    let state = Arc::new(State {
        shutdown: Arc::new(AtomicBool::new(false)),
        broker: Mutex::new(None),
        herdr_interrupt: Mutex::new(None),
        herdr: Mutex::new(Default::default()),
        shared: Mutex::new(Vec::new()),
        failed: Mutex::new(Vec::new()),
        share_in_progress: Mutex::new(None),
        mine: Mutex::new(HashSet::new()),
        started_at: Instant::now(),
    });

    // broker:best-effort,起不来只 WARN,daemon 照常 share
    match BrokerHandle::spawn(&log) {
        Ok(b) => {
            let broker = Arc::new(b);
            if let Some(port) = broker.state().port {
                // 给 rumqttd 一点起服务的时间,只为日志好看;失败无所谓
                let ok = wait_for_port(port, BROKER_WAIT);
                log.write(
                    "INFO",
                    &format!(
                        "[broker] port {port} {}",
                        if ok {
                            "is listening"
                        } else {
                            "not confirmed listening yet"
                        }
                    ),
                );
            }
            let (b2, log2, sd2) = (broker.clone(), log.clone(), state.shutdown.clone());
            std::thread::Builder::new()
                .name("broker-watch".into())
                .spawn(move || broker_watch(b2, log2, sd2))
                .ok();
            *state.broker.lock().unwrap() = Some(broker);
        }
        Err(e) => log.write(
            "WARN",
            &format!("[broker] spawn failed: {e:#} — daemon continues"),
        ),
    }

    // worker:share 执行(队列消费,绝不在锁内跑子进程)
    let (tx, rx) = std::sync::mpsc::channel::<ShareJob>();
    {
        let (state2, log2) = (state.clone(), log.clone());
        std::thread::Builder::new()
            .name("share-worker".into())
            .spawn(move || worker_loop(rx, state2, log2))
            .context("spawn share worker")?;
    }

    // reader:herdr 订阅 + 重连(阻塞读在这里,stop 靠 interrupt dup 打断)
    {
        let (state2, log2, tx2) = (state.clone(), log.clone(), tx.clone());
        std::thread::Builder::new()
            .name("herdr-reader".into())
            .spawn(move || reader_loop(state2, log2, tx2))
            .context("spawn herdr reader")?;
    }
    drop(tx); // 只留 reader 持有发送端;reader 退出后 worker 的 recv 才会 Disconnected

    // main:控制 socket accept 循环
    listener
        .set_nonblocking(true)
        .context("set listener nonblocking")?;
    loop {
        if state.shutdown.load(Ordering::Relaxed) {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                handle_connection(&stream, &state, &log);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => {
                log.write("ERROR", &format!("accept failed: {e}"));
                break;
            }
        }
        std::thread::sleep(ACCEPT_POLL);
    }

    // 收尾:先摘 socket(stop/status 立刻报 not running),再杀 broker。
    // 不 join 其他线程 —— main 返回即进程退出,卡在子进程里的 worker 一并回收。
    let _ = std::fs::remove_file(&sock);
    if let Some(broker) = state.broker.lock().unwrap().clone() {
        broker.kill_and_reap(&log);
    }
    log.write(
        "INFO",
        &format!(
            "autoshare daemon stopped (uptime {}s, shared {})",
            state.started_at.elapsed().as_secs(),
            state.shared.lock().unwrap().len()
        ),
    );
    Ok(())
}

/// broker watcher 线程体。
fn broker_watch(broker: Arc<BrokerHandle>, log: Arc<Log>, shutdown: Arc<AtomicBool>) {
    broker.watch(&shutdown, &log);
}

// ===== 控制连接 =====

/// 读一行请求、回一行应答。stop 在这里置标志并打断 reader。
fn handle_connection(stream: &UnixStream, state: &SharedState, log: &Log) {
    let mut line = String::new();
    // 读超时 2s:探活式空连接(只连不发)会在超时后静默关闭
    let mut reader = BufReader::new(stream);
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let reply = match parse_request(&line) {
        Ok(Request::Stop) => {
            log.write("INFO", "stop requested via control socket");
            state.shutdown.store(true, Ordering::Relaxed);
            // 立刻打断 reader 的阻塞读(否则最多要等一轮 backoff)
            if let Some(s) = state.herdr_interrupt.lock().unwrap().take() {
                let _ = s.shutdown(Shutdown::Both);
            }
            encode_reply_ok()
        }
        Ok(Request::Status) => encode_status(&build_report(state)),
        Err(e) => encode_reply_err(&format!("{e:#}")),
    };
    let _ = (&*stream).write_all(reply.as_bytes());
}

fn build_report(state: &SharedState) -> StatusReport {
    StatusReport {
        ok: true,
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION"),
        uptime_secs: state.started_at.elapsed().as_secs(),
        herdr: state.herdr.lock().unwrap().clone(),
        broker: state
            .broker
            .lock()
            .unwrap()
            .as_ref()
            .map(|b| b.state())
            .unwrap_or_default(),
        shared: state.shared.lock().unwrap().clone(),
        failed: state.failed.lock().unwrap().clone(),
        share_in_progress: state.share_in_progress.lock().unwrap().clone(),
    }
}

// ===== reader 线程 =====

/// herdr 订阅 + 重连。断线(herdr 重启,share pane 全死)后 backoff 重连并
/// 重新枚举;`mine` 不清空 —— 仍活着的 share 由 `list()` 挡住,死掉的重建。
fn reader_loop(state: SharedState, log: Arc<Log>, tx: std::sync::mpsc::Sender<ShareJob>) {
    let client = match default_socket_path() {
        Ok(p) => HerdrClient::new(p),
        Err(e) => {
            log.write("ERROR", &format!("cannot locate herdr socket: {e:#}"));
            return;
        }
    };
    let mut attempt: u32 = 0;
    loop {
        if state.shutdown.load(Ordering::Relaxed) {
            return;
        }
        let mut sub = match client.subscribe_agent_detected() {
            Ok(s) => s,
            Err(e) => {
                if let Ok(mut h) = state.herdr.lock() {
                    h.connected = false;
                    h.reconnects += 1;
                }
                log.write(
                    "WARN",
                    &format!(
                        "herdr connect failed (attempt {attempt}): {e:#} — retrying in {:?}",
                        backoff(attempt)
                    ),
                );
                std::thread::sleep(backoff(attempt));
                attempt += 1;
                continue;
            }
        };
        attempt = 0;

        // 存 interrupt dup;存完立即复查 shutdown(stop 可能在我们连上的瞬间已发生)
        {
            let ih = sub.try_clone_interrupt().ok();
            *state.herdr_interrupt.lock().unwrap() = ih;
            if state.shutdown.load(Ordering::Relaxed) {
                sub.shutdown();
                *state.herdr_interrupt.lock().unwrap() = None;
                return;
            }
            if let Ok(mut h) = state.herdr.lock() {
                h.connected = true;
            }
        }
        log.write("INFO", "subscribed to herdr pane.agent_detected");

        // 每次连上(含重连)都枚举一遍当前 agent
        enumerate(&client, &state, &log, &tx);

        // 推送循环
        loop {
            if state.shutdown.load(Ordering::Relaxed) {
                sub.shutdown();
                break;
            }
            match sub.recv_event() {
                Ok(None) => {
                    log.write("WARN", "herdr closed the subscription stream");
                    break;
                }
                Ok(Some(HerdrEvent::AgentDetected { pane_id, agent, .. })) => {
                    if let Ok(mut h) = state.herdr.lock() {
                        h.events_seen += 1;
                    }
                    log.write(
                        "INFO",
                        &format!("agent appeared: {agent} @ {pane_id} — queueing share"),
                    );
                    let _ = tx.send(ShareJob { pane_id, agent });
                }
                Ok(Some(HerdrEvent::AgentReleased { pane_id })) => {
                    if let Ok(mut h) = state.herdr.lock() {
                        h.events_seen += 1;
                    }
                    // share-only:agent 消失不回收 share pane(用户确认的策略)
                    log.write(
                        "INFO",
                        &format!("agent released @ {pane_id} — share-only, not unsharing"),
                    );
                }
                Ok(Some(HerdrEvent::Other)) => {
                    // 只订了一种事件;不认识的行值得留痕(协议漂移的最早信号)
                    log.write("INFO", "received an unrecognized push line — ignoring");
                }
                Err(e) => {
                    log.write("WARN", &format!("subscription read failed: {e:#}"));
                    break;
                }
            }
        }

        if let Ok(mut h) = state.herdr.lock() {
            h.connected = false;
        }
        *state.herdr_interrupt.lock().unwrap() = None;
        // 落回外层:立即尝试重连;herdr 不在时由 connect 失败分支做 backoff
    }
}

/// 枚举当前全部 agent,把需要 share 的入队。
fn enumerate(
    client: &HerdrClient,
    state: &SharedState,
    log: &Log,
    tx: &std::sync::mpsc::Sender<ShareJob>,
) {
    let agents = match client.snapshot_agents() {
        Ok(a) => a,
        Err(e) => {
            log.write("WARN", &format!("snapshot agents failed: {e:#}"));
            return;
        }
    };
    let listed = crate::share_manager::list().unwrap_or_default();
    let mut queued = 0usize;
    for a in &agents {
        if !is_shareable_status(&a.agent_status) {
            continue;
        }
        let mine = state.mine.lock().unwrap().clone();
        if !should_share(&a.pane_id, &listed, &mine) {
            continue;
        }
        let _ = tx.send(ShareJob {
            pane_id: a.pane_id.clone(),
            agent: a.agent.clone(),
        });
        queued += 1;
    }
    log.write(
        "INFO",
        &format!("enumerated {} agent(s) — {queued} to share", agents.len()),
    );
}

// ===== worker 线程 =====

/// 消费 share 队列。执行前对 `list()` + `mine` 再判重一次(队列可能过期:
/// 用户可能刚在 TUI unshare,或同一 pane 已被 share)。失败记录后继续。
fn worker_loop(rx: std::sync::mpsc::Receiver<ShareJob>, state: SharedState, log: Arc<Log>) {
    loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(job) => {
                if state.shutdown.load(Ordering::Relaxed) {
                    break;
                }
                *state.share_in_progress.lock().unwrap() = Some(job.pane_id.clone());
                let listed = crate::share_manager::list().unwrap_or_default();
                let mine = state.mine.lock().unwrap().clone();
                if !should_share(&job.pane_id, &listed, &mine) {
                    log.write("INFO", &format!("skip {} — already shared", job.pane_id));
                    *state.share_in_progress.lock().unwrap() = None;
                    continue;
                }
                match crate::share_manager::share(&job.pane_id) {
                    Ok(pane_id) => {
                        state.mine.lock().unwrap().insert(job.pane_id.clone());
                        state.shared.lock().unwrap().push(SharedEntry {
                            target: job.pane_id.clone(),
                            pane_id: pane_id.clone(),
                            agent: job.agent.clone(),
                        });
                        log.write(
                            "INFO",
                            &format!("shared {} ({}) → {pane_id}", job.pane_id, job.agent),
                        );
                    }
                    Err(e) => {
                        state.failed.lock().unwrap().push(FailedEntry {
                            target: job.pane_id.clone(),
                            error: format!("{e:#}"),
                        });
                        log.write("WARN", &format!("share {} failed: {e:#}", job.pane_id));
                    }
                }
                *state.share_in_progress.lock().unwrap() = None;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if state.shutdown.load(Ordering::Relaxed) {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::share_manager::Session;

    fn session(target: &str, pane: &str) -> Session {
        Session {
            pane_id: pane.into(),
            target: target.into(),
        }
    }

    #[test]
    fn should_share_checks_both_layers() {
        let listed = vec![session("w5:pS", "w5:pT"), session("w8:p2", "w8:p3")];
        let mut mine = HashSet::new();
        mine.insert("w9:p6".to_string());

        assert!(should_share("wF:p5", &listed, &mine)); // 谁都没碰过
        assert!(!should_share("w5:pS", &listed, &mine)); // 列表里已有(TUI 手动 share)
        assert!(!should_share("w9:p6", &listed, &mine)); // 本 daemon share 过
        assert!(should_share("w5:pS", &[], &mine)); // 列表空(pane 死了)→ 允许重建
    }

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(backoff(0), Duration::from_secs(1));
        assert_eq!(backoff(1), Duration::from_secs(2));
        assert_eq!(backoff(2), Duration::from_secs(4));
        assert_eq!(backoff(3), Duration::from_secs(8));
        assert_eq!(backoff(4), Duration::from_secs(15));
        assert_eq!(backoff(10), Duration::from_secs(15));
    }

    #[test]
    fn rfc3339_known_epochs() {
        assert_eq!(
            format_rfc3339_millis(Duration::from_secs(0)),
            "1970-01-01T00:00:00.000Z"
        );
        // date -u -r 1760000000 → 2025-10-09T08:53:20Z
        assert_eq!(
            format_rfc3339_millis(Duration::from_secs(1_760_000_000)),
            "2025-10-09T08:53:20.000Z"
        );
        // 闰年当天:2024-02-29T12:00:00Z = 1709208000
        assert_eq!(
            format_rfc3339_millis(Duration::from_secs(1_709_208_000)),
            "2024-02-29T12:00:00.000Z"
        );
        // 毫秒位
        assert_eq!(
            format_rfc3339_millis(Duration::from_millis(1_760_000_000_123)),
            "2025-10-09T08:53:20.123Z"
        );
    }

    #[test]
    fn log_open_creates_dir_and_appends() {
        let path = std::env::temp_dir().join(format!("vibeup-log-test-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let log = Log::open(&path).unwrap();
            log.write("INFO", "first");
            log.write("WARN", "second");
        }
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("INFO] first"), "{lines:?}");
        assert!(lines[1].contains("WARN] second"), "{lines:?}");
        assert!(lines[0].starts_with('['));
        let _ = std::fs::remove_file(&path);
    }
}
