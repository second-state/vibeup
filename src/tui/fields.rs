//! 设备页的行模型:`Field`(可编辑字段)/ `RowAction`(回车后干什么)/
//! `DevRow`(一行的纯数据)/ `PopupState`(弹框输入)。行随配置快照重建,
//! 动作在 app 层按 `RowAction` 分发到 `DeviceTab::do_*`。

use crate::ble::DeviceKind;
use super::input::Input;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

/// 可编辑字段:决定行内编辑提交时调用哪个动作。
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Field {
    ServerUrl,
    AsrUri,
    AsrApiKey,
    AsrModel,
    MicModel,
    PreferAsr,
    Keymap,
    Tailscale,
}

/// 编辑框的格式提示。
pub(super) fn field_hint(field: Field) -> &'static str {
    match field {
        Field::ServerUrl => "mqtt://user:pass@host:1883",
        Field::AsrUri => "https://…/audio/transcriptions",
        Field::AsrApiKey => "api key",
        Field::AsrModel => "whisper-1",
        Field::MicModel => "ptt | toggle",
        Field::PreferAsr => "on | off",
        Field::Keymap => "KEY=BINDING, e.g. CUSTOM=\"/compact\\n\"",
        Field::Tailscale => "tskey-auth-…",
    }
}

/// 枚举选项的下一个值:当前值在列表里则取其后一个(循环),不在/为空则取第一个。
pub(super) fn next_in_cycle<'a>(values: &'a [&'a str], current: &str) -> &'a str {
    match values.iter().position(|v| v.eq_ignore_ascii_case(current)) {
        Some(i) => values[(i + 1) % values.len()],
        None => values[0],
    }
}

/// 从 CONFIG 快照取一个字段的展示值(走点路径,缺失 = "")。
pub(super) fn cfg_get(v: &serde_json::Value, path: &[&str]) -> String {
    let mut cur = v;
    for p in path {
        match cur.get(p) {
            Some(next) => cur = next,
            None => return String::new(),
        }
    }
    match cur {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn asr_cfg(v: &serde_json::Value, key: &str) -> String {
    cfg_get(v, &["asr_config", key])
}

/// pop 框种类。提交动作不用闭包:动作就两个(wifi 增/改),app 层按 kind
/// 直接分发,行数据保持纯数据。
#[derive(Clone)]
pub(super) enum PopupKind {
    /// 新增 WiFi:ssid + pass 两个字段。
    WifiAdd,
    /// 改某条 WiFi 的密码。
    WifiPass { ssid: String },
}

impl PopupKind {
    /// (标题, 字段名列表)。
    fn spec(&self) -> (&'static str, &'static [&'static str]) {
        match self {
            Self::WifiAdd => (" add wifi ", &["ssid", "pass"]),
            Self::WifiPass { .. } => (" wifi: change password ", &["pass"]),
        }
    }
}

#[derive(Clone)]
pub(super) enum RowAction {
    /// 清空 ASR 配置。
    AsrClear,
    /// 设备重启使已存配置生效。
    ResetDevice,
    /// 行内单行编辑(选中回车,该行变编辑框)。
    Edit(Field),
    /// 枚举选项:回车直接切到下一个值,不进编辑框。
    Cycle {
        field: Field,
        values: Vec<&'static str>,
    },
    /// 回车弹 pop 框分字段填写。
    Popup(PopupKind),
    /// 一条现有 WiFi:回车弹框改密码,`d` 删除。
    WifiEntry { ssid: String },
}

/// 设备页一行:所属节 + 标签 + 值 + 动作。纯数据,由 `build_rows` 按配置
/// 快照生成。
pub(super) struct DevRow {
    pub(super) section: &'static str,
    pub(super) label: String,
    pub(super) value: String,
    pub(super) action: RowAction,
}

impl DevRow {
    fn edit(section: &'static str, label: &str, field: Field, value: String) -> Self {
        Self {
            section,
            label: label.to_string(),
            value,
            action: RowAction::Edit(field),
        }
    }
}

/// 运行中的 pop 框:字段展开成带光标的输入,提交按 kind 分发。
pub(super) struct PopupState {
    pub(super) kind: PopupKind,
    pub(super) title: String,
    pub(super) labels: Vec<&'static str>,
    pub(super) inputs: Vec<Input>,
    pub(super) cur: usize,
}

impl PopupState {
    /// `initials` 按字段顺序给初始值(改密码预填旧密码;新增 WiFi 全空)。
    pub(super) fn new(kind: PopupKind, initials: Vec<String>) -> Self {
        let (title, labels) = kind.spec();
        Self {
            kind,
            title: title.into(),
            labels: labels.to_vec(),
            inputs: labels
                .iter()
                .enumerate()
                .map(|(i, _)| Input::with_str(initials.get(i).map(String::as_str).unwrap_or("")))
                .collect(),
            cur: 0,
        }
    }

    /// 当前字段的光标 overlay(反显粗块,包住光标处字符;末尾则是色块)。
    pub(super) fn cursor_spans(&self, i: usize, base: Style) -> Vec<Span<'static>> {
        if i != self.cur {
            return vec![];
        }
        let input = &self.inputs[i];
        let b = input.byte_pos();
        match input.buf[b..].chars().next() {
            Some(c) => vec![Span::styled(
                c.to_string(),
                base.add_modifier(Modifier::REVERSED)
                    .add_modifier(Modifier::BOLD),
            )],
            None => vec![Span::styled(
                " ",
                base.bg(Color::LightBlue).add_modifier(Modifier::BOLD),
            )],
        }
    }
}

/// 连续行按 section 分组(fieldset 的数据来源)。
pub(super) struct Section {
    pub(super) name: &'static str,
    pub(super) start: usize,
    pub(super) len: usize,
}

pub(super) fn sections_of(rows: &[DevRow]) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        match out.last_mut() {
            Some(s) if s.name == row.section => s.len += 1,
            _ => out.push(Section {
                name: row.section,
                start: i,
                len: 1,
            }),
        }
    }
    out
}

/// 按配置快照重建全部行(wifi 列表是动态的,其余节的值也一并刷新)。
pub(super) fn build_rows(kind: DeviceKind, cfg: &serde_json::Value) -> Vec<DevRow> {
    let keys = kind == DeviceKind::VibeKeys;
    let mut rows: Vec<DevRow> = Vec::new();

    rows.push(DevRow::edit(
        "server",
        "server-url",
        Field::ServerUrl,
        cfg_get(cfg, &["server_url"]),
    ));

    // wifi:现有网络每条一行(回车改密码,d 删除),末尾固定 add +。
    for e in cfg
        .get("wifi_list")
        .and_then(|v| v.as_array())
        .unwrap_or(&vec![])
    {
        let ssid = e
            .get("ssid")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let pass = e
            .get("pass")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        rows.push(DevRow {
            section: "wifi",
            label: ssid.clone(),
            value: if pass.is_empty() {
                "(open)".into()
            } else {
                "••••••".into()
            },
            action: RowAction::WifiEntry { ssid: ssid.clone() },
        });
    }
    rows.push(DevRow {
        section: "wifi",
        label: "add +".into(),
        value: String::new(),
        action: RowAction::Popup(PopupKind::WifiAdd),
    });

    rows.push(DevRow::edit(
        "asr",
        "uri",
        Field::AsrUri,
        asr_cfg(cfg, "uri"),
    ));
    rows.push(DevRow::edit(
        "asr",
        "api-key",
        Field::AsrApiKey,
        asr_cfg(cfg, "api_key"),
    ));
    rows.push(DevRow::edit(
        "asr",
        "model",
        Field::AsrModel,
        asr_cfg(cfg, "model"),
    ));
    rows.push(DevRow {
        section: "asr",
        label: "clear".into(),
        value: String::new(),
        action: RowAction::AsrClear,
    });

    if keys {
        rows.push(DevRow {
            section: "keyboard",
            label: "mic-model".into(),
            value: match cfg.get("mic_model").and_then(|v| v.as_u64()) {
                Some(0) => "ptt".into(),
                Some(1) => "toggle".into(),
                _ => String::new(),
            },
            action: RowAction::Cycle {
                field: Field::MicModel,
                values: vec!["ptt", "toggle"],
            },
        });
        rows.push(DevRow {
            section: "keyboard",
            label: "prefer-asr".into(),
            value: match cfg.get("prefer_builtin_asr").and_then(|v| v.as_bool()) {
                Some(true) => "on".into(),
                Some(false) => "off".into(),
                _ => String::new(),
            },
            action: RowAction::Cycle {
                field: Field::PreferAsr,
                values: vec!["on", "off"],
            },
        });
        rows.push(DevRow::edit(
            "keyboard",
            "keymap (KEY=BINDING)",
            Field::Keymap,
            match cfg.get("keymap").and_then(|v| v.as_object()) {
                Some(m) if !m.is_empty() => m
                    .iter()
                    .map(|(k, val)| format!("{k}={val}"))
                    .collect::<Vec<_>>()
                    .join("  "),
                _ => String::new(),
            },
        ));
    }

    rows.push(DevRow::edit(
        "device",
        "tailscale: bind",
        Field::Tailscale,
        {
            // 真机快照不含 tailscale_key(仅写);demo 里改过才显示。
            let v = cfg_get(cfg, &["tailscale_key"]);
            if v.is_empty() { v } else { "(set)".into() }
        },
    ));
    rows.push(DevRow {
        section: "device",
        label: "reset device".into(),
        value: String::new(),
        action: RowAction::ResetDevice,
    });

    rows
}

#[cfg(test)]
mod tests {
    use super::super::demo::sample_config;
    use super::*;

    fn rows() -> Vec<DevRow> {
        build_rows(DeviceKind::VibeKeys, &sample_config(DeviceKind::VibeKeys))
    }

    #[test]
    fn sections_group_consecutive_rows() {
        let secs = sections_of(&rows());
        let names: Vec<&str> = secs.iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["server", "wifi", "asr", "keyboard", "device"]);
        assert_eq!(secs.iter().map(|s| s.len).sum::<usize>(), rows().len());
    }

    #[test]
    fn wifi_rows_are_dynamic_with_add_row_last() {
        let rows = rows();
        let wifi: Vec<&DevRow> = rows.iter().filter(|r| r.section == "wifi").collect();
        // 2 条现有网络 + add +
        assert_eq!(wifi.len(), 3);
        assert_eq!(wifi[0].label, "Home");
        assert_eq!(wifi[1].label, "Office");
        assert_eq!(wifi[2].label, "add +");
        // 现有网络 = WifiEntry(回车改密码,d 删除);add + = Popup。
        assert!(matches!(&wifi[0].action, RowAction::WifiEntry { ssid } if ssid == "Home"));
        assert!(matches!(wifi[2].action, RowAction::Popup(_)));
    }

    #[test]
    fn cycle_picks_next_and_wraps() {
        let vals = ["ptt", "toggle"];
        assert_eq!(next_in_cycle(&vals, "toggle"), "ptt");
        assert_eq!(next_in_cycle(&vals, "ptt"), "toggle");
        // 未知/空值 → 第一个。
        assert_eq!(next_in_cycle(&vals, ""), "ptt");
        assert_eq!(next_in_cycle(&vals, "bogus"), "ptt");
    }
}
