//! BLE central:扫描/连接/读写设备配置。
//!
//! 协议与代码移植自 vibekeys_app 的 `src/main.rs`(btleplug 0.12)。两个设备
//! (VibeKeys 键盘、VibeWatch 手表)广播**同一个** service UUID,配置都走同一个
//! CONFIG characteristic(READ = 全量快照,WRITE = JSON 局部 patch),只能靠广播名
//! 区分设备,因此 `DeviceKind` 必须显式指定。

use anyhow::{Result, anyhow, bail};
use btleplug::api::{Central, Manager as _, Peripheral as _, ScanFilter, WriteType};
use btleplug::platform::{Adapter, Manager, Peripheral};
use std::time::Duration;

/// 配置服务(两设备同用一个)。
pub const SERVICE_ID: uuid::Uuid = uuid::Uuid::from_u128(0x623fa3e2_631b_4f8f_a6e7_a7b09c03e7e0);
/// 统一配置:READ 全量快照 / WRITE JSON 局部 patch。
pub const CONFIG_ID: uuid::Uuid = uuid::Uuid::from_u128(0xcef520a9_bcb5_4fc6_87f7_82804eee2b20);
/// 键位重映射(仅键盘;固件把顶层键合并进现有键位表)。
pub const KEYMAP_CONFIG_ID: uuid::Uuid =
    uuid::Uuid::from_u128(0x6f2a291c_0e4d_4f0f_9446_50bcd0b73bb0);
/// 写 ASCII `RESET` → 设备重启使配置生效。
pub const RESET_ID: uuid::Uuid = uuid::Uuid::from_u128(0xf0e1d2c3_b4a5_6789_0abc_def123456789);

/// wifi_list 最多条数,与固件 `MAX_WIFI_CREDS` 一致。
pub const MAX_WIFI_CREDS: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DeviceKind {
    #[default]
    VibeKeys,
    Watch,
}

impl DeviceKind {
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "vibekeys" | "keys" => Ok(Self::VibeKeys),
            "watch" => Ok(Self::Watch),
            other => bail!("unknown device `{other}` — available: vibekeys, watch"),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::VibeKeys => "vibekeys",
            Self::Watch => "watch",
        }
    }

    /// 广播名前缀(扫描后用它区分同 service UUID 的两台设备)。
    pub(crate) fn name_prefix(self) -> &'static str {
        match self {
            Self::VibeKeys => "VibeKeys",
            Self::Watch => "Watch",
        }
    }

    /// 网页配置页:各固件仓库的 GitHub Pages(assets/setup.html 部署为首页)。
    pub fn setup_url(self) -> &'static str {
        match self {
            Self::VibeKeys => "https://second-state.github.io/vibekeys_firmware/",
            Self::Watch => "https://second-state.github.io/vibewatch_firmware/",
        }
    }
}

/// 扫描到的候选设备(同型设备可能不止一台在广播)。
#[derive(Clone)]
pub struct FoundDevice {
    pub name: String,
    /// Peripheral id 的 Display 形式,重扫时用来精确定位同一台;存字符串是
    /// 为了让 demo 模式能伪造假 id。
    pub id: String,
}

/// BLE 会话目标:`kind` 必填;`device` 为空 = 该类型扫到的第一台(旧行为,
/// CLI 用),指定 = 精确连用户在列表里选中的那一台(多台同名时不会连错)。
#[derive(Clone)]
pub struct Target {
    pub kind: DeviceKind,
    pub device: Option<String>,
}

impl Target {
    pub fn auto(kind: DeviceKind) -> Self {
        Self { kind, device: None }
    }

    pub fn picked(kind: DeviceKind, id: String) -> Self {
        Self {
            kind,
            device: Some(id),
        }
    }
}

/// 把单个配置字段包成 CONFIG 部分对象 patch,设备只更新出现的字段。
pub fn patch(field: &str, value: serde_json::Value) -> Vec<u8> {
    serde_json::json!({ field: value }).to_string().into_bytes()
}

/// 从 CONFIG 快照 JSON 解析出 wifi_list(顺序即优先级)。
pub fn parse_wifi_list(snapshot_json: &str) -> Vec<(String, String)> {
    serde_json::from_str::<serde_json::Value>(snapshot_json)
        .ok()
        .and_then(|v| {
            v.get("wifi_list").and_then(|v| v.as_array()).map(|arr| {
                arr.iter()
                    .map(|c| {
                        (
                            c.get("ssid")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            c.get("pass")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                        )
                    })
                    .collect()
            })
        })
        .unwrap_or_default()
}

/// 把 (ssid, pass) 列表序列化成固件 wifi_list JSON。
pub fn wifi_list_to_json(list: &[(String, String)]) -> serde_json::Value {
    serde_json::Value::Array(
        list.iter()
            .map(|(s, p)| serde_json::json!({"ssid": s, "pass": p}))
            .collect(),
    )
}

/// 规整 wifi_list:丢弃空 ssid,截断到 MAX_WIFI_CREDS。
pub fn sanitize_wifi_list(list: &[serde_json::Value]) -> Vec<serde_json::Value> {
    list.iter()
        .filter_map(|c| {
            let ssid = c.get("ssid").and_then(|v| v.as_str())?;
            if ssid.is_empty() {
                return None;
            }
            let pass = c.get("pass").and_then(|v| v.as_str()).unwrap_or("");
            Some(serde_json::json!({"ssid": ssid, "pass": pass}))
        })
        .take(MAX_WIFI_CREDS)
        .collect()
}

async fn get_adapter() -> Result<Adapter> {
    let manager = Manager::new().await?;
    let adapter = manager
        .adapters()
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("No Bluetooth adapter"))?;
    Ok(adapter)
}

/// 一轮轮扫描收集:每轮 100ms × 20 轮,轮间重启扫描(节奏照抄 vibekeys_app)。
/// 返回 (匹配 kind 的设备,按发现顺序去重; 出现过的其他设备名,报错用)。
/// `stop_at_first` = 扫到第一台匹配就提前收工(CLI 直连);TUI 的设备列表要
/// 列出全部候选,扫满整个广播窗口。
async fn collect_scan(
    adapter: &Adapter,
    kind: DeviceKind,
    stop_at_first: bool,
) -> Result<(Vec<FoundDevice>, Vec<String>)> {
    let mut filter = ScanFilter::default();
    filter.services.push(SERVICE_ID);
    let mut seen: Vec<String> = Vec::new();
    let mut found: Vec<FoundDevice> = Vec::new();
    adapter.start_scan(filter.clone()).await?;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let peripherals = adapter.peripherals().await?;
        if peripherals.is_empty() {
            continue;
        }
        adapter.stop_scan().await?;
        for p in &peripherals {
            let Ok(Some(props)) = p.properties().await else {
                continue;
            };
            if !props.services.contains(&SERVICE_ID) {
                continue;
            }
            // 缓存幽灵过滤:BlueZ 把连过一次的设备永久缓存,关机也在
            // peripherals() 列表里。活着的设备只有两种状态:已连接(开机
            // 自动重连)或在广播(rssi 有值);缓存幽灵两者皆无。
            let connected = p.is_connected().await.unwrap_or(false);
            if !connected && props.rssi.is_none() {
                continue;
            }
            let name = props.local_name.clone().unwrap_or_default();
            if name.starts_with(kind.name_prefix()) {
                let id = p.id().to_string();
                if !found.iter().any(|f| f.id == id) {
                    found.push(FoundDevice { name, id });
                }
            } else if !name.is_empty() && !seen.contains(&name) {
                seen.push(name);
            }
        }
        if stop_at_first && !found.is_empty() {
            break;
        }
        adapter.start_scan(filter.clone()).await?;
    }
    let _ = adapter.stop_scan().await;
    Ok((found, seen))
}

fn no_device_error(kind: DeviceKind, seen: &[String]) -> anyhow::Error {
    if seen.is_empty() {
        anyhow!(
            "no {} in provisioning mode — put it there first ({})",
            kind.name(),
            match kind {
                DeviceKind::VibeKeys => "boot into Keyboard mode, press ACCEPT to re-advertise",
                DeviceKind::Watch => "Setting → Enable BLE",
            }
        )
    } else {
        anyhow!(
            "found other device(s) but no {}: {} — check the device and retry",
            kind.name(),
            seen.join(", ")
        )
    }
}

/// 在本 adapter 已发现的设备里按 id 找回 Peripheral 句柄(扫描刚停,设备还在
/// 发现列表里,无需重扫)。
async fn find_peripheral(adapter: &Adapter, id: &str) -> Result<Peripheral> {
    for p in adapter.peripherals().await? {
        if p.id().to_string() == id {
            return Ok(p);
        }
    }
    bail!("device {id} disappeared right after the scan")
}

/// 按广播名前缀挑第一台(旧行为,CLI 直连)。
async fn scan_and_find(adapter: &Adapter, kind: DeviceKind) -> Result<Peripheral> {
    let (found, seen) = collect_scan(adapter, kind, true).await?;
    match found.into_iter().next() {
        Some(d) => find_peripheral(adapter, &d.id).await,
        None => Err(no_device_error(kind, &seen)),
    }
}

/// 重扫并精确定位之前选中的那台。和 `collect_scan` 同一套活性判断:
/// 已连接或 rssi 有值才算,缓存幽灵不算。
async fn scan_pick_id(adapter: &Adapter, kind: DeviceKind, id: &str) -> Result<Peripheral> {
    let mut filter = ScanFilter::default();
    filter.services.push(SERVICE_ID);
    adapter.start_scan(filter.clone()).await?;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let peripherals = adapter.peripherals().await?;
        for p in &peripherals {
            if p.id().to_string() != id {
                continue;
            }
            let connected = p.is_connected().await.unwrap_or(false);
            let rssi = p
                .properties()
                .await
                .ok()
                .flatten()
                .and_then(|props| props.rssi);
            if connected || rssi.is_some() {
                let _ = adapter.stop_scan().await;
                return Ok(p.clone());
            }
        }
    }
    let _ = adapter.stop_scan().await;
    bail!(
        "{} is no longer advertising — put it back in provisioning mode and rescan",
        kind.name()
    )
}

/// 扫描 → 连接 → 发现服务 → 执行 `f` → 断开。一次性连接,vibeup 的配置是低频
/// 操作,不做 vibekeys_app 那套常驻 server。不往 stdout 打任何东西:TUI 会把
/// 它放进后台线程跑,打印会花掉整屏。
pub fn with_target<T, F>(target: Target, f: impl FnOnce(Peripheral) -> F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    let kind = target.kind;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let adapter = get_adapter().await?;
        let p = match &target.device {
            Some(id) => scan_pick_id(&adapter, kind, id).await?,
            None => scan_and_find(&adapter, kind).await?,
        };
        p.connect().await?;
        p.discover_services().await?;
        let res = f(p.clone()).await;
        // 断开失败不影响结果;不 await 它(连接马上随进程结束被回收)。
        drop(p.disconnect());
        res
    })
}

pub async fn write_char(p: &Peripheral, char_uuid: uuid::Uuid, data: &[u8]) -> Result<()> {
    for c in &p.characteristics() {
        if c.uuid == char_uuid {
            p.write(c, data, WriteType::WithResponse).await?;
            return Ok(());
        }
    }
    bail!("characteristic {char_uuid} not found")
}

pub async fn read_char(p: &Peripheral, char_uuid: uuid::Uuid) -> Result<Vec<u8>> {
    for c in &p.characteristics() {
        if c.uuid == char_uuid {
            return Ok(p.read(c).await?);
        }
    }
    bail!("characteristic {char_uuid} not found")
}

/// 读 KEYMAP_CONFIG_ID(键名 → 绑定对象的扁平 JSON,与写入格式一致)。
/// 固件初值可能为空字节(开机从 NVS 刷新前),按空对象处理。
pub async fn read_keymap(p: &Peripheral) -> Result<serde_json::Value> {
    let data = read_char(p, KEYMAP_CONFIG_ID).await?;
    let s = String::from_utf8_lossy(&data);
    if s.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(&s).map_err(|e| anyhow!("device returned invalid keymap JSON: {e}"))
}

/// 读 CONFIG 全量快照并解析成 JSON Value。keymap 固件单独放在
/// KEYMAP_CONFIG_ID,这里读回来合并进 `keymap` 字段;没有该特征
/// (watch / wifi 模式 / 旧固件)就保持 CONFIG 原样,不影响别的字段。
pub async fn read_config(p: &Peripheral) -> Result<serde_json::Value> {
    let data = read_char(p, CONFIG_ID).await?;
    let mut cfg: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&data))
        .map_err(|e| anyhow!("device returned invalid config JSON: {e}"))?;
    if let Ok(km) = read_keymap(p).await {
        cfg["keymap"] = km;
    }
    Ok(cfg)
}

/// 写一个 CONFIG 字段 patch。
pub async fn write_config_field(
    p: &Peripheral,
    field: &str,
    value: serde_json::Value,
) -> Result<()> {
    write_char(p, CONFIG_ID, &patch(field, value)).await
}

/// 写 RESET(设备重启,连接随之断开)。
pub async fn reset_device(p: &Peripheral) -> Result<()> {
    match write_char(p, RESET_ID, b"RESET").await {
        Ok(()) => Ok(()),
        // 设备重启可能先于写响应掐断连接,这不算失败。
        Err(e) if e.to_string().to_lowercase().contains("disconnect") => Ok(()),
        Err(e) => Err(e),
    }
}

// ===== 常驻会话(TUI 用)=====

/// 会话推给 TUI 的事件。
pub enum SessionEvent {
    /// 连接建立,附首份 CONFIG 快照。
    Ready(Result<serde_json::Value>),
    /// 连接断了(检测方式见 `check_connected`)。连接建立前失败也走这里。
    Disconnected(anyhow::Error),
}

/// 会话命令:TUI 的设备操作发到后台线程执行。
enum SessionCmd {
    ConfigRead {
        reply: std::sync::mpsc::Sender<Result<serde_json::Value>>,
    },
    ConfigWrite {
        field: String,
        value: serde_json::Value,
        reply: std::sync::mpsc::Sender<Result<()>>,
    },
    KeymapWrite {
        payload: String,
        reply: std::sync::mpsc::Sender<Result<()>>,
    },
    Reset {
        reply: std::sync::mpsc::Sender<Result<()>>,
    },
}

/// 常驻 BLE 会话:后台线程扫到目标设备就连上并持有,TUI 的每次读写走命令
/// 通道,免得每个动作都重扫重连。断连检测仿 vibekeys_app —— btleplug 的
/// `is_connected` 在设备消失时可能永久挂起,所以包一层 1s 超时,超时 = 断开;
/// 平时每 5s 轮询一次,每次收发前也先查一遍。
pub struct Session {
    cmd_tx: tokio::sync::mpsc::Sender<SessionCmd>,
    events: std::sync::mpsc::Receiver<SessionEvent>,
}

impl Session {
    /// 起一个会话线程,立刻返回句柄。连接结果(Ready/Disconnected)从
    /// `try_event` 拿;TUI 丢掉句柄 = 命令通道关闭,线程连上后自退。
    pub fn open(target: Target) -> Self {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(8);
        let (ev_tx, ev_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let opened = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(anyhow::Error::from)
                .and_then(|rt| rt.block_on(session_loop(target, cmd_rx, &ev_tx)));
            // 线程退出 = 断连;兜底再报一次,保证 TUI 不会干等事件。
            if let Err(e) = opened {
                let _ = ev_tx.send(SessionEvent::Disconnected(e));
            }
        });
        Self { cmd_tx, events: ev_rx }
    }

    /// 非阻塞收一条事件。
    pub fn try_event(&self) -> Option<SessionEvent> {
        self.events.try_recv().ok()
    }

    /// 发一条命令等回复。会话已关(线程退出)时报错。
    /// 用 `try_send` + `recv_timeout` 而非 `blocking_send` + `recv`:
    /// UI 事件 handler 跑在 tokio runtime 线程上,`blocking_send` 在那里
    /// 会 panic("cannot block from within a runtime"),`recv` 也会阻塞
    /// worker;`try_send` 非阻塞、`recv_timeout` 有界,ms 级往返完全够用。
    fn call<T>(
        &self,
        make: impl FnOnce(std::sync::mpsc::Sender<Result<T>>) -> SessionCmd,
    ) -> Result<T> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.cmd_tx
            .try_send(make(tx))
            .map_err(|_| anyhow!("device session is closed"))?;
        // 超时 ≠ 会话关闭:命令多半已发出,设备端可能卡住/重启,
        // 写入也许已生效。分开报,别让两种故障共用一句。
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(res) => res,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                anyhow::bail!("device did not respond within 5s — it may be stuck or rebooting; the write may still have been applied")
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("device session is closed")
            }
        }
    }

    pub fn read_config(&self) -> Result<serde_json::Value> {
        self.call(|reply| SessionCmd::ConfigRead { reply })
    }

    pub fn write_config_field(&self, field: &str, value: serde_json::Value) -> Result<()> {
        self.call(|reply| SessionCmd::ConfigWrite {
            field: field.to_string(),
            value,
            reply,
        })
    }

    pub fn write_keymap(&self, payload: &str) -> Result<()> {
        self.call(|reply| SessionCmd::KeymapWrite {
            payload: payload.to_string(),
            reply,
        })
    }

    pub fn reset(&self) -> Result<()> {
        self.call(|reply| SessionCmd::Reset { reply })
    }
}

/// vibekeys_app 的 `check_connected`:`is_connected` 包 1s 超时,超时/出错
/// 一律按断开算(库在设备消失时会让这个调用挂死)。
async fn check_connected(p: &Peripheral) -> bool {
    tokio::time::timeout(std::time::Duration::from_secs(1), p.is_connected())
        .await
        .unwrap_or(Ok(false))
        .unwrap_or(false)
}

/// vibekeys_app 的 `loop_check_connection`:每 5s 一查,断了就返回。
async fn until_disconnected(p: &Peripheral) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        if !check_connected(p).await {
            return;
        }
    }
}

async fn session_loop(
    target: Target,
    mut cmd_rx: tokio::sync::mpsc::Receiver<SessionCmd>,
    ev_tx: &std::sync::mpsc::Sender<SessionEvent>,
) -> Result<()> {
    let adapter = get_adapter().await?;
    let p = match &target.device {
        Some(id) => scan_pick_id(&adapter, target.kind, id).await?,
        None => scan_and_find(&adapter, target.kind).await?,
    };
    p.connect().await?;
    p.discover_services().await?;
    let _ = ev_tx.send(SessionEvent::Ready(read_config(&p).await));

    loop {
        tokio::select! {
            // 断连看门狗:每 5s 探一次。
            _ = until_disconnected(&p) => {
                let _ = ev_tx.send(SessionEvent::Disconnected(anyhow!("device disconnected")));
                return Ok(());
            }
            // 命令服务:每次收发前先查连接(vibekeys_app 同款前置检查)。
            cmd = cmd_rx.recv() => {
                match cmd {
                    // TUI 侧弃用会话:安静收尾,不算断连。
                    None => return Ok(()),
                    Some(c) => {
                        if !serve_cmd(&p, c, ev_tx).await {
                            return Ok(());
                        }
                    }
                }
            }
        }
    }
}

/// 执行一条命令;回复一定发出(哪怕失败),TUI 才不会干等。返回 false =
/// 连接已断,会话收尾(Disconnected 事件已发)。
async fn serve_cmd(
    p: &Peripheral,
    cmd: SessionCmd,
    ev_tx: &std::sync::mpsc::Sender<SessionEvent>,
) -> bool {
    // 前置连接检查(vibekeys_app 同款);不过 = 断开。
    if !check_connected(p).await {
        let _ = ev_tx.send(SessionEvent::Disconnected(anyhow!("device disconnected")));
        match cmd {
            SessionCmd::ConfigRead { reply } => {
                let _ = reply.send(Err(anyhow!("device disconnected")));
            }
            SessionCmd::ConfigWrite { reply, .. }
            | SessionCmd::KeymapWrite { reply, .. }
            | SessionCmd::Reset { reply } => {
                let _ = reply.send(Err(anyhow!("device disconnected")));
            }
        }
        return false;
    }
    match cmd {
        SessionCmd::ConfigRead { reply } => {
            let _ = reply.send(read_config(p).await);
        }
        SessionCmd::ConfigWrite { field, value, reply } => {
            let _ = reply.send(write_config_field(p, &field, value).await);
        }
        SessionCmd::KeymapWrite { payload, reply } => {
            let res = write_char(p, KEYMAP_CONFIG_ID, payload.as_bytes()).await;
            // 固件 on_write 只把 KeymapConfig 投进主循环队列,NVS 保存和读缓存
            // 刷新在主循环里完成;立刻回读会撞上旧缓存。等一拍再回复,
            // 紧随其后的 ConfigRead 就能读到新值。
            if res.is_ok() {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            let _ = reply.send(res);
        }
        SessionCmd::Reset { reply } => {
            let _ = reply.send(reset_device(p).await);
        }
    }
    true
}
