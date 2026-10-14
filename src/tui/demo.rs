//! demo 模式:内存假配置 + 假设备广播名,扫描/连接用假任务走完整个流程,
//! 不碰蓝牙。动作(`do_field` / `do_wifi_*` / …)直接落在假配置上。

use super::device::{Job, JobOut, spawn_job};
use crate::ble::DeviceKind;
use crate::config_cmd;

pub(super) fn sample_config(kind: DeviceKind) -> serde_json::Value {
    let base = serde_json::json!({
        "wifi_list": [
            {"ssid": "Home", "pass": "horse-battery"},
            {"ssid": "Office", "pass": "correct-horse"}
        ],
        "server_url": "mqtt://127.0.0.1:1883",
        "asr_config": {
            "platform": "whisper",
            "uri": "https://api.openai.com/v1/audio/transcriptions",
            "api_key": "sk-demo",
            "model": "whisper-1"
        },
    });
    if kind == DeviceKind::VibeKeys {
        let mut v = base;
        v["mic_model"] = serde_json::json!(1);
        v["prefer_builtin_asr"] = serde_json::json!(true);
        v
    } else {
        base
    }
}

pub(super) fn mock_wifi_add(mock: &mut serde_json::Value, ssid: &str, pass: &str) {
    let list = mock["wifi_list"].as_array_mut().unwrap();
    match list.iter_mut().find(|e| e["ssid"] == ssid) {
        Some(e) => e["pass"] = pass.into(),
        None => list.push(serde_json::json!({"ssid": ssid, "pass": pass})),
    }
}

pub(super) fn mock_wifi_remove(mock: &mut serde_json::Value, ssid: &str) {
    mock["wifi_list"]
        .as_array_mut()
        .unwrap()
        .retain(|e| e["ssid"] != ssid);
}

pub(super) fn mock_asr_set(
    mock: &mut serde_json::Value,
    uri: Option<&str>,
    api_key: Option<&str>,
    model: Option<&str>,
) {
    let asr = mock.get_mut("asr_config").unwrap();
    if let Some(v) = uri {
        asr["uri"] = v.into();
    }
    if let Some(v) = api_key {
        asr["api_key"] = v.into();
    }
    if let Some(v) = model {
        asr["model"] = v.into();
    }
}

pub(super) fn mock_keymap(mock: &mut serde_json::Value, key: &str, binding: &str) {
    let physical = config_cmd::physical_key_name(key);
    let parsed = config_cmd::parse_key_binding(binding);
    mock["keymap"][&physical] = parsed;
}

/// demo 假设备广播名:仿真机形如 `VibeKeys-XXXX`。
pub(super) fn demo_devices(kind: DeviceKind) -> Vec<crate::ble::FoundDevice> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let suffixes = [(nanos & 0xffff) as u16, ((nanos >> 8) & 0xffff) as u16];
    suffixes
        .into_iter()
        .map(|s| crate::ble::FoundDevice {
            name: format!("{}-{s:04X}", kind.name_prefix()),
            id: format!("demo-{s:04X}"),
        })
        .collect()
}

/// demo 扫描:假装扫了 0.9s,回两台假设备。
pub(super) fn demo_scan_job(kind: DeviceKind) -> Job {
    spawn_job(move || {
        std::thread::sleep(std::time::Duration::from_millis(900));
        JobOut::Scan(Ok(demo_devices(kind)))
    })
}

/// demo 连接:假装连了 0.4s,回样例配置。
pub(super) fn demo_connect_job(kind: DeviceKind) -> Job {
    spawn_job(move || {
        std::thread::sleep(std::time::Duration::from_millis(400));
        JobOut::Config(Ok(sample_config(kind)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock() -> serde_json::Value {
        sample_config(DeviceKind::VibeKeys)
    }

    #[test]
    fn sample_config_has_device_specific_fields() {
        let keys = sample_config(DeviceKind::VibeKeys);
        assert!(keys.get("mic_model").is_some());
        assert!(keys.get("prefer_builtin_asr").is_some());
        let watch = sample_config(DeviceKind::Watch);
        assert!(watch.get("mic_model").is_none());
    }

    #[test]
    fn mock_wifi_add_upserts() {
        let mut m = mock();
        mock_wifi_add(&mut m, "Cafe", "pw");
        assert_eq!(m["wifi_list"].as_array().unwrap().len(), 3);
        mock_wifi_add(&mut m, "Cafe", "pw2");
        let list = m["wifi_list"].as_array().unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[2]["pass"], "pw2");
    }

    #[test]
    fn mock_wifi_remove_filters() {
        let mut m = mock();
        mock_wifi_remove(&mut m, "Home");
        let list = m["wifi_list"].as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["ssid"], "Office");
    }

    #[test]
    fn mock_asr_set_keeps_unset_fields() {
        let mut m = mock();
        mock_asr_set(&mut m, Some("https://x"), None, Some("m2"));
        let asr = &m["asr_config"];
        assert_eq!(asr["uri"], "https://x");
        assert_eq!(asr["api_key"], "sk-demo");
        assert_eq!(asr["model"], "m2");
    }

    #[test]
    fn mock_keymap_uses_physical_key_and_binding_parser() {
        let mut m = mock();
        mock_keymap(&mut m, "yolo", "\"/review\\n\"");
        let km = &m["keymap"];
        assert_eq!(km["SWITCH"]["type"], "text");
        assert_eq!(km["SWITCH"]["value"], "/review\n");
    }

    #[test]
    fn demo_devices_look_like_broadcast_names() {
        let keys = demo_devices(DeviceKind::VibeKeys);
        assert_eq!(keys.len(), 2);
        assert!(keys.iter().all(|d| d.name.starts_with("VibeKeys-")));
        let watch = demo_devices(DeviceKind::Watch);
        assert!(watch.iter().all(|d| d.name.starts_with("Watch-")));
    }
}
