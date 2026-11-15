//! `vibetty broker` 子进程生命周期。
//!
//! vibetty 内嵌 rumqttd,`vibetty broker` 前台跑它:子进程死 = broker 死
//! (rumqttd 无 shutdown 接口),也没有 pid 文件。它自己处理端口占用(被占时
//! 干净退出 exit 0)和缺 `[mqtt]` 配置(exit 1)—— 所以这里只管 spawn + 持有
//! Child + stop 时 kill,broker 本身是 best-effort:它退出 daemon 照常跑。

use anyhow::{Context, Result};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::control::BrokerState;
use super::daemon::Log;

pub struct BrokerHandle {
    pid: Option<u32>,
    port: Option<u16>,
    /// 单一所有权槽:watcher(自然退出)和 kill_and_reap(stop)都是 take 了再动,
    /// 不会双 wait。None = 子进程已被收走。
    child: Mutex<Option<std::process::Child>>,
    exit: Mutex<Option<std::process::ExitStatus>>,
}

impl BrokerHandle {
    /// spawn `vibetty broker`(stdout/stderr 并入 daemon 自己的输出 →
    /// detached 模式落到日志文件,前台模式落到终端)。
    pub fn spawn(log: &Log) -> Result<Self> {
        let port = broker_port_from_file(&super::vibetty_config_path());
        let child = Command::new("vibetty")
            .arg("broker")
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .context("failed to spawn `vibetty broker`")?;
        let pid = child.id();
        log.write(
            "INFO",
            &format!("[broker] spawned `vibetty broker` (pid {pid})"),
        );
        Ok(Self {
            pid: Some(pid),
            port,
            child: Mutex::new(Some(child)),
            exit: Mutex::new(None),
        })
    }

    /// status 展示用(纯读,不碰子进程)。
    pub fn state(&self) -> BrokerState {
        BrokerState {
            pid: self.pid,
            running: self.exit.lock().map(|e| e.is_none()).unwrap_or(true),
            exit_code: self
                .exit
                .lock()
                .ok()
                .and_then(|e| *e)
                .and_then(|s| s.code()),
            port: self.port,
        }
    }

    /// watcher 线程体:1s 轮询 try_wait,子进程退出后记录状态并 WARN
    /// (端口被占 exit 0 = 已有 broker 在跑,是正常情形;其余 WARN)。
    pub fn watch(self: &Arc<Self>, shutdown: &AtomicBool, log: &Log) {
        loop {
            if shutdown.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_secs(1));
            let mut slot = match self.child.lock() {
                Ok(s) => s,
                Err(_) => return,
            };
            let Some(child) = slot.as_mut() else { return };
            match child.try_wait() {
                Ok(Some(status)) => {
                    let code = status.code();
                    if let Ok(mut e) = self.exit.lock() {
                        *e = Some(status);
                    }
                    slot.take(); // 收走,防 stop 侧再 wait
                    match code {
                        Some(0) => log.write(
                            "INFO",
                            "[broker] `vibetty broker` exited 0 — broker already running elsewhere",
                        ),
                        _ => log.write(
                            "WARN",
                            &format!(
                                "[broker] `vibetty broker` exited (code {code:?}) — best-effort, daemon continues"
                            ),
                        ),
                    }
                    return;
                }
                Ok(None) => {}
                Err(e) => {
                    log.write("WARN", &format!("[broker] try_wait failed: {e}"));
                    return;
                }
            }
        }
    }

    /// stop 路径:kill + wait(进程死 = rumqttd 死,这是唯一的停止方式)。
    pub fn kill_and_reap(&self, log: &Log) {
        let mut slot = match self.child.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        if let Some(mut child) = slot.take() {
            log.write("INFO", "[broker] killing `vibetty broker` child");
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// `vibetty broker` 端口是否已在监听(等 rumqttd 起来用,失败无害)。
pub fn port_open(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_err()
}

/// 等 broker 端口开始监听(最多 timeout,150ms 一步);超时不报错。
pub fn wait_for_port(port: u16, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if port_open(port) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    port_open(port)
}

/// 从 `~/.vibetty/config.toml` 文本抠 `[mqtt] broker` 的端口(status 展示用)。
/// 没有 `[mqtt]` 段 / broker 行 → None。
pub fn broker_port_from_file(path: &Option<std::path::PathBuf>) -> Option<u16> {
    let text = std::fs::read_to_string(path.as_ref()?).ok()?;
    broker_port_from_config(&text)
}

pub fn broker_port_from_config(text: &str) -> Option<u16> {
    let mut in_mqtt = false;
    let mut url: Option<String> = None;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') {
            in_mqtt = t.eq_ignore_ascii_case("[mqtt]");
            continue;
        }
        if !in_mqtt {
            continue;
        }
        if let Some(rest) = t.strip_prefix("broker") {
            let Some(rest) = rest.trim_start().strip_prefix('=') else {
                continue;
            };
            // TOML 字符串双引号/单引号都合法
            url = Some(
                rest.trim()
                    .trim_matches(|c| c == '"' || c == '\'')
                    .to_string(),
            );
        }
    }
    parse_broker_port(&url?)
}

/// `mqtt://host:port` / `mqtts://…` → 端口;URL 没写端口时按 scheme 默认
/// (mqtt → 1883,mqtts → 8883,与 vibetty 的 split_broker_host_port 一致)。
pub fn parse_broker_port(url: &str) -> Option<u16> {
    let mqtts = url.starts_with("mqtts://");
    let after = url
        .strip_prefix("mqtt://")
        .or_else(|| url.strip_prefix("mqtts://"))?;
    let host_port = after.split('/').next().unwrap_or("");
    match host_port.rsplit_once(':') {
        Some((_, p)) => p.parse().ok(),
        None => Some(if mqtts { 8883 } else { 1883 }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_from_this_machines_real_config() {
        // 本机真实 ~/.vibetty/config.toml 的形态
        let text = "[mqtt]\nbroker = \"mqtt://127.0.0.1:8883\"\nbuiltin_ws_port = 9001\nkeep_alive_secs = 15\n";
        assert_eq!(broker_port_from_config(text), Some(8883));
    }

    #[test]
    fn port_defaults_follow_the_scheme() {
        assert_eq!(parse_broker_port("mqtt://host"), Some(1883));
        assert_eq!(parse_broker_port("mqtts://host"), Some(8883));
        assert_eq!(parse_broker_port("mqtt://100.107.1.65:1883"), Some(1883));
        assert_eq!(parse_broker_port("mqtt://[::1]:1884"), Some(1884));
        assert_eq!(parse_broker_port("mqtt://host:1883/path"), Some(1883));
        assert_eq!(parse_broker_port("mqtt://host:notaport"), None);
        assert_eq!(parse_broker_port("http://host:1"), None);
    }

    #[test]
    fn port_missing_mqtt_section_is_none() {
        assert_eq!(broker_port_from_config("[other]\nbroker = \"x\"\n"), None);
        assert_eq!(broker_port_from_config(""), None);
        // broker 出现在别的段里不算
        let text = "[asr]\nbroker = \"mqtt://h:1\"\n\n[mqtt]\nkeep_alive_secs = 30\n";
        assert_eq!(broker_port_from_config(text), None);
    }

    #[test]
    fn broker_in_second_mqtt_position_is_found() {
        let text = "# comment\n[server]\nurl = \"x\"\n\n[mqtt]\n# c\nbroker = 'mqtt://h:9999'\n";
        assert_eq!(broker_port_from_config(text), Some(9999));
    }
}
