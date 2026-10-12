//! `vibeup config …` —— 通过 BLE 配置 VibeKeys 键盘 / VibeWatch 手表。
//!
//! 设备显式指定(`vibekeys` / `watch`):两台设备广播同一个 service UUID,配置协议
//! 同构(CONFIG characteristic,JSON 局部 patch),差异只在字段集:
//! - 仅键盘:`mic_model`、`prefer_builtin_asr`、键位重映射(KEYMAP characteristic)
//! - 仅手表独占:`tailscale_key`(该次写里其他字段被固件忽略,写完设备跳
//!   Tailscale 页、连接断开——视为成功)
//! - 都支持:`wifi_list`、`server_url`、`asr_config`、RESET
//!
//! 键位 binding 语法与 `vibekeys keymap` 一致,解析逻辑移植自 vibekeys_app 的
//! `parse_key_binding` / `unescape_string`。

use crate::ble::{self, DeviceKind};
use anyhow::{Result, bail};
use btleplug::platform::Peripheral;

const RESET_PREAMBLE: &str = "device must reboot to apply";

fn keys_only(kind: DeviceKind) -> Result<()> {
    if kind != DeviceKind::VibeKeys {
        bail!("`{}` is only supported on vibekeys, not watch", kind.name());
    }
    Ok(())
}

/// CONFIG 快照 pretty print。
pub fn show(kind: DeviceKind) -> Result<()> {
    ble::with_device(kind, |p| async move { read_and_print(&p).await })
}

async fn read_and_print(p: &Peripheral) -> Result<()> {
    let cfg = ble::read_config(p).await?;
    println!("{}", serde_json::to_string_pretty(&cfg)?);
    Ok(())
}

pub fn wifi(kind: DeviceKind, action: WifiAction, reset: bool) -> Result<()> {
    ble::with_device(kind, |p| async move {
        match action {
            WifiAction::Show => {
                let cfg = ble::read_config(&p).await?;
                let list = cfg
                    .get("wifi_list")
                    .cloned()
                    .unwrap_or(serde_json::Value::Array(vec![]));
                println!("{}", serde_json::to_string_pretty(&list)?);
            }
            WifiAction::Add { ssid, pass } => {
                // 读-改-写:追加(同 ssid 覆盖),全量写回。
                let cfg = ble::read_config(&p).await?;
                let snapshot = serde_json::to_string(&cfg)?;
                let mut list = ble::parse_wifi_list(&snapshot);
                match list.iter_mut().find(|(s, _)| *s == ssid) {
                    Some(entry) => entry.1 = pass,
                    None => list.push((ssid.clone(), pass.clone())),
                }
                if list.len() > ble::MAX_WIFI_CREDS {
                    bail!(
                        "wifi list is full ({}/{}): remove one first with --remove",
                        list.len(),
                        ble::MAX_WIFI_CREDS
                    );
                }
                ble::write_config_field(&p, "wifi_list", ble::wifi_list_to_json(&list)).await?;
                println!(
                    "wifi `{}' saved ({}/{}), order = priority",
                    ssid,
                    list.len(),
                    ble::MAX_WIFI_CREDS
                );
                maybe_reset(&p, reset).await?;
            }
            WifiAction::Remove { ssid } => {
                let cfg = ble::read_config(&p).await?;
                let snapshot = serde_json::to_string(&cfg)?;
                let list = ble::parse_wifi_list(&snapshot);
                let rest: Vec<_> = list.iter().filter(|(s, _)| *s != ssid).cloned().collect();
                if rest.len() == list.len() {
                    println!("wifi `{}' not in the list", ssid);
                } else {
                    ble::write_config_field(&p, "wifi_list", ble::wifi_list_to_json(&rest)).await?;
                    println!("wifi `{}' removed", ssid);
                    maybe_reset(&p, reset).await?;
                }
            }
            WifiAction::Set { json } => {
                let parsed: serde_json::Value = serde_json::from_str(&json)
                    .map_err(|e| anyhow::anyhow!("invalid wifi_list JSON: {e}"))?;
                let arr = parsed
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("expected a JSON array of {{ssid, pass}}"))?;
                let clean = ble::sanitize_wifi_list(arr);
                ble::write_config_field(&p, "wifi_list", serde_json::Value::Array(clean)).await?;
                println!("wifi list replaced");
                maybe_reset(&p, reset).await?;
            }
        }
        Ok(())
    })
}

pub enum WifiAction {
    Show,
    Add { ssid: String, pass: String },
    Remove { ssid: String },
    Set { json: String },
}

pub fn server_url(kind: DeviceKind, url: String, reset: bool) -> Result<()> {
    ble::with_device(kind, |p| async move {
        ble::write_config_field(&p, "server_url", serde_json::Value::String(url.clone())).await?;
        println!("server_url = {url}");
        maybe_reset(&p, reset).await?;
        Ok(())
    })
}

#[derive(Default)]
pub struct AsrOpts {
    pub uri: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub clean: bool,
}

pub fn asr(kind: DeviceKind, opts: AsrOpts, reset: bool) -> Result<()> {
    let inner = if opts.clean {
        // 与 vibekeys_app `asr-config --clean` 一致:全空串。
        serde_json::json!({"platform": "whisper", "uri": "", "api_key": "", "model": ""})
    } else {
        if opts.uri.is_none() && opts.api_key.is_none() && opts.model.is_none() {
            bail!("nothing to set — pass --uri/--api-key/--model, or --clean to clear");
        }
        serde_json::json!({
            "platform": "whisper",
            "uri": opts.uri,
            "api_key": opts.api_key,
            "model": opts.model,
        })
    };
    ble::with_device(kind, |p| async move {
        ble::write_config_field(&p, "asr_config", inner).await?;
        println!("asr_config updated");
        maybe_reset(&p, reset).await?;
        Ok(())
    })
}

pub fn mic_model(kind: DeviceKind, mode: String, reset: bool) -> Result<()> {
    keys_only(kind)?;
    let value: u8 = match mode.to_lowercase().as_str() {
        "ptt" => 0,
        "toggle" => 1,
        other => bail!("invalid mic mode `{other}` — use ptt or toggle"),
    };
    ble::with_device(kind, |p| async move {
        ble::write_config_field(&p, "mic_model", serde_json::json!(value)).await?;
        println!("mic_model = {value}");
        maybe_reset(&p, reset).await?;
        Ok(())
    })
}

pub fn prefer_asr(kind: DeviceKind, on: bool, reset: bool) -> Result<()> {
    keys_only(kind)?;
    ble::with_device(kind, |p| async move {
        ble::write_config_field(&p, "prefer_builtin_asr", serde_json::json!(on)).await?;
        println!("prefer_builtin_asr = {on}");
        maybe_reset(&p, reset).await?;
        Ok(())
    })
}

pub fn tailscale(kind: DeviceKind, key: String) -> Result<()> {
    if key.trim().is_empty() {
        bail!("empty tailscale auth key");
    }
    ble::with_device(kind, |p| async move {
        // 手表上这条写是独占的:固件忽略同包其他字段,并存后立刻跳
        // Tailscale 页、断开 BLE。连接断开导致的错误视为成功。
        let res =
            ble::write_config_field(&p, "tailscale_key", serde_json::Value::String(key.clone()))
                .await;
        match res {
            Ok(()) => println!("tailscale key bound — check the device page"),
            Err(e) if e.to_string().to_lowercase().contains("disconnect") => {
                println!("tailscale key bound (device left provisioning)")
            }
            Err(e) => return Err(e),
        }
        Ok(())
    })
}

pub fn keymap(kind: DeviceKind, key: String, binding: String) -> Result<()> {
    keys_only(kind)?;
    let payload = build_keymap_config(&key, &binding);
    ble::with_device(kind, |p| async move {
        ble::write_char(&p, ble::KEYMAP_CONFIG_ID, payload.as_bytes()).await?;
        println!("keymap {key} updated (merged into the device keymap)");
        Ok(())
    })
}

pub fn reset(kind: DeviceKind) -> Result<()> {
    ble::with_device(kind, |p| async move {
        ble::reset_device(&p).await?;
        println!("{} is rebooting", kind.name());
        Ok(())
    })
}

async fn maybe_reset(p: &Peripheral, reset: bool) -> Result<()> {
    if reset {
        ble::reset_device(p).await?;
        println!("{RESET_PREAMBLE} — device rebooting");
    } else {
        println!("note: stored; reboot the device (`vibeup config reset`) to apply");
    }
    Ok(())
}

// ===== 键位 binding 解析(移植自 vibekeys_app)=====

/// 用户-facing 键名 → 物理键名,含 `YOLO` → `SWITCH` 别名。
pub(crate) fn physical_key_name(key: &str) -> String {
    let key_upper = key.to_uppercase();
    if key_upper == "YOLO" {
        "SWITCH".to_string()
    } else {
        key_upper
    }
}

/// 把 `KEY BINDING` 变成固件键位表的 JSON patch,如
/// `{"CUSTOM": {"type": "text", "value": "/compact\n", ...}}`。
fn build_keymap_config(key: &str, binding: &str) -> String {
    serde_json::json!({ physical_key_name(key): parse_key_binding(binding) }).to_string()
}

pub(crate) fn parse_key_binding(input: &str) -> serde_json::Value {
    let trimmed = input.trim();

    // 引号包起来 = 文本宏:`"/review\n"` 输入 /review 再回车。
    if (trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2)
        || (trimmed.starts_with('\'') && trimmed.ends_with('\'') && trimmed.len() >= 2)
    {
        let content = &trimmed[1..trimmed.len() - 1];
        return serde_json::json!({
            "type": "text",
            "value": unescape_string(content),
            "raw": input,
        });
    }

    const VALID_KEYS: &[&str] = &[
        "enter",
        "return",
        "space",
        "tab",
        "escape",
        "esc",
        "backspace",
        "delete",
        "insert",
        "home",
        "end",
        "pageup",
        "pagedown",
        "up",
        "down",
        "left",
        "right",
        "ctrl",
        "shift",
        "alt",
        "option",
        "gui",
        "win",
        "meta",
        "cmd",
        "command",
        "f1",
        "f2",
        "f3",
        "f4",
        "f5",
        "f6",
        "f7",
        "f8",
        "f9",
        "f10",
        "f11",
        "f12",
        "plus",
        "minus",
        "equal",
        "semicolon",
        "quote",
        "backquote",
        "backslash",
        "comma",
        "period",
        "slash",
        "bracketleft",
        "bracketright",
    ];
    const VALID_MODIFIERS: &[&str] = &["ctrl", "alt", "option", "shift", "meta", "win", "cmd"];

    // 单个大写字母 / 数字 = 单键 combo。
    if trimmed.len() == 1 {
        let c = trimmed.chars().next().unwrap();
        if c.is_ascii_uppercase() || c.is_ascii_digit() {
            return serde_json::json!({
                "type": "combo",
                "modifiers": [],
                "key": trimmed,
                "raw": input,
            });
        }
    }

    let key_lower = trimmed.to_lowercase();
    if VALID_KEYS.contains(&key_lower.as_str()) {
        return serde_json::json!({
            "type": "combo",
            "modifiers": [],
            "key": trimmed.to_uppercase(),
            "raw": input,
        });
    }

    // `Ctrl+C` 这样的修饰键组合:最后一段是键,前面全得是修饰键。
    if trimmed.contains('+') {
        let parts: Vec<&str> = trimmed.split('+').map(|p| p.trim()).collect();
        let all_mods_valid = parts[..parts.len() - 1]
            .iter()
            .all(|p| VALID_MODIFIERS.contains(&p.to_lowercase().as_str()));
        if all_mods_valid {
            let last = parts.last().unwrap();
            let last_lower = last.to_lowercase();
            let is_valid_key = VALID_KEYS.contains(&last_lower.as_str())
                || (last.len() == 1 && last.chars().next().is_some_and(char::is_alphanumeric));
            if is_valid_key {
                let modifiers: Vec<String> = parts[..parts.len() - 1]
                    .iter()
                    .map(|p| match p.to_lowercase().as_str() {
                        "win" | "cmd" => "meta".to_string(),
                        "option" => "alt".to_string(),
                        other => other.to_string(),
                    })
                    .collect();
                return serde_json::json!({
                    "type": "combo",
                    "modifiers": modifiers,
                    "key": last.to_uppercase(),
                    "raw": input,
                });
            }
        }
    }

    // 其余一律当文本宏。
    serde_json::json!({
        "type": "text",
        "value": trimmed,
        "raw": input,
    })
}

fn unescape_string(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('r') => result.push('\r'),
                Some('t') => result.push('\t'),
                Some('\\') => result.push('\\'),
                Some('"') => result.push('"'),
                Some('\'') => result.push('\''),
                Some('0') => result.push('\0'),
                Some('x') => {
                    let hex1 = chars.next().unwrap_or('0');
                    let hex2 = chars.next().unwrap_or('0');
                    if let Ok(byte) = u8::from_str_radix(&format!("{hex1}{hex2}"), 16) {
                        result.push(byte as char);
                    }
                }
                Some(other) => {
                    result.push('\\');
                    result.push(other);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(c);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_macro_with_quotes_and_escapes() {
        let v = parse_key_binding(r#""/compact\n""#);
        assert_eq!(v["type"], "text");
        assert_eq!(v["value"], "/compact\n");
        assert_eq!(v["raw"], r#""/compact\n""#);
    }

    #[test]
    fn single_letter_and_digit_are_plain_combos() {
        for s in ["Y", "1"] {
            let v = parse_key_binding(s);
            assert_eq!(v["type"], "combo");
            assert_eq!(v["key"], s);
            assert_eq!(v["modifiers"], serde_json::json!([]));
        }
    }

    #[test]
    fn named_key_is_uppercased_combo() {
        let v = parse_key_binding("enter");
        assert_eq!(v["type"], "combo");
        assert_eq!(v["key"], "ENTER");
    }

    #[test]
    fn modifier_combo_normalizes_modifiers() {
        let v = parse_key_binding("Option+Tab");
        assert_eq!(v["type"], "combo");
        assert_eq!(v["key"], "TAB");
        assert_eq!(v["modifiers"], serde_json::json!(["alt"]));

        let v = parse_key_binding("Cmd+C");
        assert_eq!(v["modifiers"], serde_json::json!(["meta"]));
    }

    #[test]
    fn bare_word_falls_back_to_text() {
        let v = parse_key_binding("hello");
        assert_eq!(v["type"], "text");
        assert_eq!(v["value"], "hello");
    }

    #[test]
    fn keymap_patch_uses_physical_key_and_yolo_alias() {
        let json = build_keymap_config("YOLO", "Shift+Tab");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v.get("SWITCH").is_some());
        assert_eq!(v["SWITCH"]["type"], "combo");
    }
}
