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

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
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
    fn name_prefix(self) -> &'static str {
        match self {
            Self::VibeKeys => "VibeKeys",
            Self::Watch => "Watch",
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

/// 按 service UUID 扫描,再按广播名前缀挑出目标设备。扫描节奏照抄
/// vibekeys_app:每轮 100ms × 20 轮,轮间重启扫描。
async fn scan_and_find(adapter: &Adapter, kind: DeviceKind) -> Result<Peripheral> {
    let mut filter = ScanFilter::default();
    filter.services.push(SERVICE_ID);
    let mut seen: Vec<String> = Vec::new();
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
            let name = props.local_name.clone().unwrap_or_default();
            if name.starts_with(kind.name_prefix()) {
                return Ok(p.clone());
            }
            if !name.is_empty() && !seen.contains(&name) {
                seen.push(name);
            }
        }
        adapter.start_scan(filter.clone()).await?;
    }
    adapter.stop_scan().await?;
    if seen.is_empty() {
        bail!(
            "no {} in provisioning mode — put it there first ({})",
            kind.name(),
            match kind {
                DeviceKind::VibeKeys => "boot into Keyboard mode, press ACCEPT to re-advertise",
                DeviceKind::Watch => "Setting → Enable BLE",
            }
        );
    }
    bail!(
        "found other device(s) but no {}: {} — check the device and retry",
        kind.name(),
        seen.join(", ")
    )
}

/// 扫描 → 连接 → 发现服务 → 执行 `f` → 断开。一次性连接,vibeup 的配置是低频
/// 操作,不做 vibekeys_app 那套常驻 server。
pub fn with_device<T, F>(kind: DeviceKind, f: impl FnOnce(Peripheral) -> F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let adapter = get_adapter().await?;
        let p = scan_and_find(&adapter, kind).await?;
        println!("Connecting to {}…", kind.name());
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

/// 读 CONFIG 全量快照并解析成 JSON Value。
pub async fn read_config(p: &Peripheral) -> Result<serde_json::Value> {
    let data = read_char(p, CONFIG_ID).await?;
    serde_json::from_str(&String::from_utf8_lossy(&data))
        .map_err(|e| anyhow!("device returned invalid config JSON: {e}"))
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
