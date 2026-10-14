//! 设备页状态机:进入 tab → 后台扫描 → 选设备 → 连接 → Ready 后是配置界面。
//! 连接成功后持有常驻 BLE 会话(`ble::Session`,后台线程),动作瞬时完成、
//! 断连自动弹窗(vibekeys_app 式检测);扫描/连接放后台线程跑,结果经
//! channel 回来,app 层事件循环轮询——界面不冻结,还能画转圈动画。

use super::demo;
use super::fields::{self, DevRow, Field, PopupState};
use crate::ble::{self, DeviceKind, FoundDevice, Session, SessionEvent};
use anyhow::{Result, bail};

/// 设备页连接状态。
#[derive(Clone, PartialEq, Debug)]
pub(super) enum Conn {
    /// 还没进入过(进入 tab 时弹入口菜单)。
    Fresh,
    /// 入口菜单:1 扫描设备 / 2 从网页配置。
    Menu,
    /// 网页配置:显示 setup 页地址(`opened` = 浏览器是否已尝试打开)。
    WebSetup { url: &'static str },
    /// 后台扫描中。
    Scanning,
    /// 扫到候选设备,等用户挑(列表在 `pick`)。
    Pick,
    /// 正在连接(后台),存设备名供提示。
    Connecting(String),
    /// 已连上,配置界面就绪。
    Ready,
    /// 扫描/连接失败,可重试。
    Failed(String),
    /// 连上之后设备断开了(动作/重读报断连类错误):弹窗等回车,回扫描页。
    Disconnected,
}

/// 后台 BLE 任务:扫描 2 秒上下、连接也要几秒,放线程里跑,结果经 channel
/// 回来,TUI 事件循环轮询——界面不冻结,还能画转圈动画。
pub(super) struct Job {
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    rx: std::sync::mpsc::Receiver<JobOut>,
}

pub(super) enum JobOut {
    Scan(anyhow::Result<Vec<FoundDevice>>),
    Config(anyhow::Result<serde_json::Value>),
}

pub(super) fn spawn_job(f: impl FnOnce() -> JobOut + Send + 'static) -> Job {
    use std::sync::atomic::AtomicBool;
    let (tx, rx) = std::sync::mpsc::channel();
    let done = std::sync::Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    std::thread::spawn(move || {
        let _ = tx.send(f());
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    Job { done, rx }
}

/// 已连上/正在连的设备:名字给界面顶部显示,连接本体在 `session` 里。
pub(super) struct Connected {
    pub(super) name: String,
}

pub(super) struct DeviceTab {
    pub(super) kind: DeviceKind,
    /// 连接流程状态:进 tab 扫描 → 选设备 → 连接 → Ready 后才是配置界面。
    pub(super) conn: Conn,
    /// 扫描出的候选设备(Pick 状态下的选择列表)。
    pub(super) pick: Vec<FoundDevice>,
    pub(super) pick_sel: usize,
    /// 入口菜单的选中项(0 = 扫描设备,1 = 从网页配置)。
    pub(super) menu_sel: usize,
    pub(super) config: Option<serde_json::Value>,
    /// 选中的设备(Ready 后所有动作和重读都发给它,顶部显示名字)。
    pub(super) connected: Option<Connected>,
    /// 正在连接的设备,连接成功后转正为 `connected`。
    pending: Option<Connected>,
    pub(super) rows: Vec<DevRow>,
    pub(super) sel: usize,
    /// demo 模式:不碰 BLE,动作改内存里的假配置,扫描/连接用假设备走流程。
    pub(super) demo: bool,
    mock: serde_json::Value,
    /// 行内编辑:选中行下标 + 当前输入。
    pub(super) editing: Option<(usize, super::input::Input)>,
    /// 打开的 pop 框。
    pub(super) popup: Option<PopupState>,
    /// 进行中的后台扫描/连接。
    pub(super) job: Option<Job>,
    /// 常驻 BLE 会话(真机 Ready 后持有,动作走它,断连事件从它来)。
    pub(super) session: Option<Session>,
    /// 转圈动画帧计数(事件循环空转时递增)。
    pub(super) spin: usize,
}

impl DeviceTab {
    pub(super) fn new(kind: DeviceKind, demo: bool) -> Self {
        let mut t = Self {
            kind,
            conn: Conn::Fresh,
            pick: Vec::new(),
            pick_sel: 0,
            menu_sel: 0,
            config: None,
            connected: None,
            pending: None,
            rows: Vec::new(),
            sel: 0,
            demo,
            mock: demo::sample_config(kind),
            editing: None,
            popup: None,
            job: None,
            session: None,
            spin: 0,
        };
        t.rebuild_rows();
        t
    }

    /// 常驻会话(真机动作走它)。
    fn session(&self) -> Result<&Session> {
        self.session
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("device session is closed"))
    }

    /// 提交一个编辑值:demo 落假配置,真机走常驻会话(瞬时,不重扫重连)。
    pub(super) fn do_field(&mut self, field: Field, val: &str) -> Result<()> {
        let val = val.trim();
        match field {
            Field::ServerUrl => {
                if self.demo {
                    self.mock["server_url"] = val.into();
                    return Ok(());
                }
                self.session()?.write_config_field("server_url", val.into())
            }
            Field::AsrUri => {
                if self.demo {
                    demo::mock_asr_set(&mut self.mock, Some(val), None, None);
                    return Ok(());
                }
                // 与 CLI `asr --uri` 同款:只带 uri,其余置 null(固件忽略)。
                self.session()?.write_config_field(
                    "asr_config",
                    serde_json::json!({
                        "platform": "whisper",
                        "uri": val,
                        "api_key": serde_json::Value::Null,
                        "model": serde_json::Value::Null,
                    }),
                )
            }
            Field::AsrApiKey => {
                if self.demo {
                    demo::mock_asr_set(&mut self.mock, None, Some(val), None);
                    return Ok(());
                }
                self.session()?.write_config_field(
                    "asr_config",
                    serde_json::json!({
                        "platform": "whisper",
                        "uri": serde_json::Value::Null,
                        "api_key": val,
                        "model": serde_json::Value::Null,
                    }),
                )
            }
            Field::AsrModel => {
                if self.demo {
                    demo::mock_asr_set(&mut self.mock, None, None, Some(val));
                    return Ok(());
                }
                self.session()?.write_config_field(
                    "asr_config",
                    serde_json::json!({
                        "platform": "whisper",
                        "uri": serde_json::Value::Null,
                        "api_key": serde_json::Value::Null,
                        "model": val,
                    }),
                )
            }
            Field::MicModel => {
                let v = match val.to_lowercase().as_str() {
                    "ptt" => 0,
                    "toggle" => 1,
                    other => bail!("invalid mic mode `{other}` — use ptt or toggle"),
                };
                if self.demo {
                    self.mock["mic_model"] = serde_json::json!(v);
                    return Ok(());
                }
                self.session()?
                    .write_config_field("mic_model", serde_json::json!(v))
            }
            Field::PreferAsr => {
                let on = match val.to_lowercase().as_str() {
                    "on" | "true" => true,
                    "off" | "false" => false,
                    other => bail!("invalid value `{other}` — use on or off"),
                };
                if self.demo {
                    self.mock["prefer_builtin_asr"] = serde_json::json!(on);
                    return Ok(());
                }
                self.session()?
                    .write_config_field("prefer_builtin_asr", serde_json::json!(on))
            }
            Field::Keymap => {
                let Some((key, binding)) = val.split_once('=') else {
                    bail!("expected `KEY=BINDING`, e.g. CUSTOM=\"/compact\\n\"");
                };
                if self.demo {
                    demo::mock_keymap(&mut self.mock, key.trim(), binding.trim());
                    return Ok(());
                }
                let payload = crate::config_cmd::build_keymap_config(key.trim(), binding.trim());
                self.session()?.write_keymap(&payload)
            }
            Field::Tailscale => {
                if self.demo {
                    self.mock["tailscale_key"] = val.into();
                    return Ok(());
                }
                if val.is_empty() {
                    bail!("empty tailscale auth key");
                }
                // 手表上这条写是独占的:固件存完立刻跳 Tailscale 页、断连 ——
                // 断连会以 Disconnected 事件到达,弹窗是预期表现。
                self.session()?
                    .write_config_field("tailscale_key", serde_json::Value::String(val.into()))
            }
        }
    }

    /// WiFi 增改(同 ssid = 更新密码):demo 落假配置,真机读-改-写整个列表
    /// (和 CLI `wifi add` 一致,顺序 = 优先级)。
    pub(super) fn do_wifi_add(&mut self, ssid: &str, pass: &str) -> Result<()> {
        if ssid.trim().is_empty() {
            bail!("ssid must not be empty");
        }
        if self.demo {
            demo::mock_wifi_add(&mut self.mock, ssid.trim(), pass);
            return Ok(());
        }
        let session = self.session()?;
        let list = ble::parse_wifi_list(&serde_json::to_string(&session.read_config()?)?);
        let mut list = list;
        match list.iter_mut().find(|(s, _)| *s == ssid.trim()) {
            Some(e) => e.1 = pass.to_string(),
            None => list.push((ssid.trim().to_string(), pass.to_string())),
        }
        if list.len() > ble::MAX_WIFI_CREDS {
            bail!(
                "wifi list is full ({}/{}): remove one first",
                list.len(),
                ble::MAX_WIFI_CREDS
            );
        }
        session.write_config_field("wifi_list", ble::wifi_list_to_json(&list))
    }

    /// WiFi 删除。
    pub(super) fn do_wifi_remove(&mut self, ssid: &str) -> Result<()> {
        if self.demo {
            demo::mock_wifi_remove(&mut self.mock, ssid);
            return Ok(());
        }
        let session = self.session()?;
        let list = ble::parse_wifi_list(&serde_json::to_string(&session.read_config()?)?);
        let rest: Vec<_> = list.iter().filter(|(s, _)| *s != ssid).cloned().collect();
        if rest.len() == list.len() {
            bail!("wifi `{ssid}' not in the list");
        }
        session.write_config_field("wifi_list", ble::wifi_list_to_json(&rest))
    }

    /// 清空 ASR 配置(与 vibekeys_app `asr-config --clean` 一致:全空串)。
    pub(super) fn do_asr_clear(&mut self) -> Result<()> {
        if self.demo {
            self.mock["asr_config"] =
                serde_json::json!({"platform": "whisper", "uri": "", "api_key": "", "model": ""});
            return Ok(());
        }
        self.session()?.write_config_field(
            "asr_config",
            serde_json::json!({"platform": "whisper", "uri": "", "api_key": "", "model": ""}),
        )
    }

    /// 设备重启使已存配置生效。重启会掐断连接,断连事件随后到 → 弹窗,
    /// 回车重扫即可重连。
    pub(super) fn do_reset(&mut self) -> Result<()> {
        if self.demo {
            return Ok(());
        }
        self.session()?.reset()
    }

    /// 进入设备页:先弹入口菜单(扫描设备 / 从网页配置),不自动扫描。
    pub(super) fn enter(&mut self) {
        if self.conn == Conn::Fresh {
            self.conn = Conn::Menu;
        }
    }

    /// 网页配置:切到地址弹窗,URL 由用户自己复制去浏览器打开。
    fn show_web_setup(&mut self) {
        let url = self.kind.setup_url();
        self.conn = Conn::WebSetup { url };
    }

    pub(super) fn start_scan(&mut self) {
        // 旧任务直接弃:线程跑完自然退出,结果没人收而已。
        self.job = None;
        self.conn = Conn::Scanning;
        let kind = self.kind;
        self.job = Some(match self.demo {
            true => demo::demo_scan_job(kind),
            false => super::device::spawn_job(move || JobOut::Scan(ble::scan_devices(kind))),
        });
    }

    fn start_connect(&mut self, dev: FoundDevice) {
        self.job = None;
        self.session = None;
        self.pending = Some(Connected {
            name: dev.name.clone(),
        });
        self.conn = Conn::Connecting(dev.name);
        let kind = self.kind;
        match self.demo {
            // demo:假任务 0.4s 后回样例配置。
            true => self.job = Some(demo::demo_connect_job(kind)),
            // 真机:起常驻会话,连接结果(Ready/Disconnected)从事件来。
            false => {
                let target = ble::Target::picked(self.kind, dev.id.clone());
                self.session = Some(Session::open(target));
            }
        }
    }

    pub(super) fn pick_connect(&mut self) {
        if let Some(dev) = self.pick.get(self.pick_sel).cloned() {
            self.start_connect(dev);
        }
    }

    /// 轮询后台任务;完成则推进状态机,返回给 footer 的消息。
    pub(super) fn poll_job(&mut self) -> Option<String> {
        let job = self.job.take()?;
        if !job.done.load(std::sync::atomic::Ordering::Relaxed) {
            self.job = Some(job);
            return None;
        }
        match job.rx.try_recv().ok() {
            Some(JobOut::Scan(res)) => Some(self.on_scan_result(res)),
            Some(JobOut::Config(res)) => Some(self.on_connect_result(res)),
            // done 置位却没有结果:线程 panic 了,极少见,按失败走。
            None => {
                self.conn = Conn::Failed("background task died".into());
                Some("background task died".into())
            }
        }
    }

    fn on_scan_result(&mut self, res: anyhow::Result<Vec<FoundDevice>>) -> String {
        match res {
            Ok(devs) if !devs.is_empty() => {
                self.pick = devs;
                self.pick_sel = 0;
                self.conn = Conn::Pick;
                format!("found {} device(s) — pick one", self.pick.len())
            }
            Ok(_) => {
                self.conn = Conn::Failed("no devices found".into());
                "no devices found".into()
            }
            Err(e) => {
                let msg = format!("{e:#}");
                self.conn = Conn::Failed(msg.clone());
                msg
            }
        }
    }

    pub(super) fn on_connect_result(&mut self, res: anyhow::Result<serde_json::Value>) -> String {
        match res {
            Ok(cfg) => {
                self.config = Some(cfg);
                self.connected = Some(self.pending.take().unwrap_or_else(|| Connected {
                    name: self.kind.name().into(),
                }));
                self.rebuild_rows();
                self.sel = self.sel.min(self.rows.len() - 1);
                self.conn = Conn::Ready;
                "connected".into()
            }
            Err(e) => {
                let msg = format!("{e:#}");
                self.conn = Conn::Failed(msg.clone());
                msg
            }
        }
    }

    /// 轮询常驻会话事件(连接建立/断连)。断连按所处阶段分流:Ready → 弹窗;
    /// 还在连接中 → 失败屏。
    pub(super) fn poll_session(&mut self) -> Option<String> {
        let ev = self.session.as_ref()?.try_event()?;
        Some(self.on_session_event(ev))
    }

    pub(super) fn on_session_event(&mut self, ev: SessionEvent) -> String {
        match ev {
            SessionEvent::Ready(res) => self.on_connect_result(res),
            SessionEvent::Disconnected(e) => {
                self.session = None;
                if self.conn == Conn::Ready {
                    self.editing = None;
                    self.popup = None;
                    self.conn = Conn::Disconnected;
                    format!("device disconnected: {e:#}")
                } else {
                    // 连接还没建立就断了:走失败屏。
                    self.on_connect_result(Err(e))
                }
            }
        }
    }

    /// 按当前配置快照重建行(wifi 列表是动态的,其余节的值也一并刷新)。
    /// 行是纯数据:动作在 app 层按 RowAction 分发到 `do_*` 方法。
    fn rebuild_rows(&mut self) {
        let empty = serde_json::json!({});
        let cfg = self.config.as_ref().unwrap_or(&empty);
        self.rows = fields::build_rows(self.kind, cfg);
        self.sel = self.sel.min(self.rows.len() - 1);
    }

    /// 重读配置(r 键、动作成功后)。demo 读内存假数据;真机走常驻会话,
    /// 瞬时完成。返回错误消息,None = 成功;失败保留旧快照,界面不塌。
    /// 断连不走这里 —— 会话会推 Disconnected 事件,由 `on_session_event` 弹窗。
    pub(super) fn load_config(&mut self) -> Option<String> {
        if self.demo {
            self.config = Some(self.mock.clone());
            self.rebuild_rows();
            return None;
        }
        let session = self.session.as_ref()?;
        match session.read_config() {
            Ok(cfg) => {
                self.config = Some(cfg);
                self.rebuild_rows();
                None
            }
            Err(e) => Some(format!("{e:#}")),
        }
    }
}

/// 设备页连接流程按键:入口菜单 / 网页配置 / 扫描中 / 选设备 / 连接中 / 失败 /
/// 断连弹窗时的键位(Ready 走 app 层主循环)。
pub(super) fn device_flow_key(t: &mut DeviceTab, code: ratatui::crossterm::event::KeyCode, message: &mut String) {
    // 先折成 Copy 的小枚举,免得借用 &t.conn 卡住后面的 &mut 方法。
    #[derive(Clone, Copy)]
    enum Flow {
        Fresh,
        Menu,
        WebSetup,
        Scanning,
        Pick,
        Connecting,
        Failed,
        Disconnected,
    }
    let flow = match t.conn {
        Conn::Fresh => Flow::Fresh,
        Conn::Menu => Flow::Menu,
        Conn::WebSetup { .. } => Flow::WebSetup,
        Conn::Scanning => Flow::Scanning,
        Conn::Pick => Flow::Pick,
        Conn::Connecting(_) => Flow::Connecting,
        Conn::Ready => return,
        Conn::Failed(_) => Flow::Failed,
        Conn::Disconnected => Flow::Disconnected,
    };
    use ratatui::crossterm::event::KeyCode;
    match (flow, code) {
        (Flow::Fresh, KeyCode::Enter | KeyCode::Char('r')) => t.start_scan(),
        (Flow::Menu, KeyCode::Char('j') | KeyCode::Down) => {
            t.menu_sel = (t.menu_sel + 1).min(1);
        }
        (Flow::Menu, KeyCode::Char('k') | KeyCode::Up) => {
            t.menu_sel = t.menu_sel.saturating_sub(1);
        }
        (Flow::Menu, KeyCode::Enter) => match t.menu_sel {
            0 => t.start_scan(),
            _ => t.show_web_setup(),
        },
        // 数字直选(此时全局 1/2/3 切标签让位给菜单)。
        (Flow::Menu, KeyCode::Char('1')) => t.start_scan(),
        (Flow::Menu, KeyCode::Char('2')) => t.show_web_setup(),
        (Flow::WebSetup, KeyCode::Esc | KeyCode::Enter) => {
            // Esc/Enter 回菜单(Esc 惯例;Enter 兼容"看完按回车")。
            t.conn = Conn::Menu;
        }
        (Flow::Scanning, KeyCode::Char('r')) => t.start_scan(),
        (Flow::Scanning, KeyCode::Esc) => {
            t.job = None;
            t.conn = Conn::Menu;
            *message = "scan cancelled".into();
        }
        (Flow::Pick, KeyCode::Char('j') | KeyCode::Down) => {
            t.pick_sel = (t.pick_sel + 1).min(t.pick.len().saturating_sub(1));
        }
        (Flow::Pick, KeyCode::Char('k') | KeyCode::Up) => {
            t.pick_sel = t.pick_sel.saturating_sub(1);
        }
        (Flow::Pick, KeyCode::Enter) => t.pick_connect(),
        (Flow::Pick, KeyCode::Char('r') | KeyCode::Esc) => t.start_scan(),
        (Flow::Connecting, KeyCode::Esc) => {
            // 弃掉连接任务/会话,回列表;设备也许还在广播,重连再试即可。
            t.job = None;
            t.session = None;
            t.conn = Conn::Pick;
            *message = "connect cancelled".into();
        }
        (Flow::Failed, KeyCode::Enter | KeyCode::Char('r')) => t.start_scan(),
        // 断连弹窗:按回车确认,回扫描页(用户要求只认回车)。
        (Flow::Disconnected, KeyCode::Enter) => t.start_scan(),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::super::demo::sample_config;
    use super::*;
    use ratatui::crossterm::event::KeyCode;

    fn tab() -> DeviceTab {
        let mut t = DeviceTab::new(DeviceKind::VibeKeys, true);
        t.conn = Conn::Ready;
        t.connected = Some(Connected {
            name: "VibeKeys-1A2B".into(),
        });
        t.config = Some(t.mock.clone());
        t.rebuild_rows();
        t
    }

    fn devs() -> Vec<FoundDevice> {
        vec![
            FoundDevice {
                name: "VibeKeys-1A2B".into(),
                id: "demo-1A2B".into(),
            },
            FoundDevice {
                name: "VibeKeys-3C4D".into(),
                id: "demo-3C4D".into(),
            },
        ]
    }

    #[test]
    fn wifi_pass_edit_popup_upserts_same_ssid() {
        let mut t = tab();
        // 第一条 wifi 行(Home)的改密码 pop 框提交 → 同 ssid 更新,不新增。
        let rows_after = t.rows.len();
        t.do_wifi_add("Home", "new-pass").unwrap();
        let list = t.mock["wifi_list"].as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["pass"], "new-pass");
        assert_eq!(t.rows.len(), rows_after);
    }

    #[test]
    fn do_field_demo_updates_mock() {
        let mut t = DeviceTab::new(DeviceKind::VibeKeys, true);
        t.do_field(Field::ServerUrl, "mqtt://x").unwrap();
        assert_eq!(t.mock["server_url"], "mqtt://x");
        t.do_field(Field::MicModel, "ptt").unwrap();
        assert_eq!(t.mock["mic_model"], 0);
        t.do_field(Field::Keymap, "CUSTOM=\"/compact\\n\"")
            .unwrap();
        assert_eq!(t.mock["keymap"]["CUSTOM"]["type"], "text");
        t.do_field(Field::MicModel, "bogus").unwrap_err();
    }

    #[test]
    fn scan_result_fills_pick_list_and_connect_reaches_ready() {
        let mut t = DeviceTab::new(DeviceKind::VibeKeys, true);
        t.on_scan_result(Ok(devs()));
        assert_eq!(t.conn, Conn::Pick);
        assert_eq!(t.pick.len(), 2);
        assert_eq!(t.pick_sel, 0);
        t.pick_connect();
        assert_eq!(t.conn, Conn::Connecting("VibeKeys-1A2B".into()));
        assert!(t.pending.is_some());
        t.on_connect_result(Ok(sample_config(DeviceKind::VibeKeys)));
        assert_eq!(t.conn, Conn::Ready);
        // 转正后带广播名,顶部设备块用它显示。
        assert_eq!(t.connected.as_ref().unwrap().name, "VibeKeys-1A2B");
        assert!(!t.rows.is_empty());
    }

    #[test]
    fn scan_and_connect_failures_land_in_failed_state() {
        let mut t = DeviceTab::new(DeviceKind::VibeKeys, true);
        t.on_scan_result(Err(anyhow::anyhow!("no vibekeys in provisioning mode")));
        assert!(matches!(t.conn, Conn::Failed(_)));
        t.on_connect_result(Err(anyhow::anyhow!("connection refused")));
        assert!(matches!(t.conn, Conn::Failed(_)));
    }

    #[test]
    fn disconnect_event_pops_dialog_then_rescan_on_enter() {
        let mut t = tab();
        assert_eq!(t.conn, Conn::Ready);
        // 看门狗/前置检查报的断连事件 → 弹窗状态。
        let msg = t.on_session_event(SessionEvent::Disconnected(anyhow::anyhow!(
            "device disconnected"
        )));
        assert_eq!(t.conn, Conn::Disconnected);
        assert!(t.session.is_none());
        assert!(msg.starts_with("device disconnected"));
        let mut msg = String::new();
        device_flow_key(&mut t, KeyCode::Enter, &mut msg);
        assert_eq!(t.conn, Conn::Scanning);
    }

    #[test]
    fn disconnect_event_while_connecting_lands_in_failed_state() {
        let mut t = DeviceTab::new(DeviceKind::VibeKeys, true);
        t.start_connect(FoundDevice {
            name: "VibeKeys-1A2B".into(),
            id: "demo-1A2B".into(),
        });
        assert!(matches!(t.conn, Conn::Connecting(_)));
        t.on_session_event(SessionEvent::Disconnected(anyhow::anyhow!(
            "no longer advertising"
        )));
        assert!(matches!(t.conn, Conn::Failed(_)));
    }

    #[test]
    fn ready_event_from_session_reaches_connected() {
        let mut t = DeviceTab::new(DeviceKind::VibeKeys, true);
        t.start_connect(FoundDevice {
            name: "VibeKeys-1A2B".into(),
            id: "demo-1A2B".into(),
        });
        let msg = t.on_session_event(SessionEvent::Ready(Ok(sample_config(
            DeviceKind::VibeKeys,
        ))));
        assert_eq!(t.conn, Conn::Ready);
        assert_eq!(msg, "connected");
        assert_eq!(t.connected.as_ref().unwrap().name, "VibeKeys-1A2B");
    }

    #[test]
    fn flow_keys_navigate_pick_and_rescan() {
        let mut t = DeviceTab::new(DeviceKind::VibeKeys, true);
        t.on_scan_result(Ok(devs()));
        let mut msg = String::new();
        device_flow_key(&mut t, KeyCode::Down, &mut msg);
        assert_eq!(t.pick_sel, 1);
        device_flow_key(&mut t, KeyCode::Down, &mut msg);
        assert_eq!(t.pick_sel, 1); // 不越界
        device_flow_key(&mut t, KeyCode::Char('k'), &mut msg);
        assert_eq!(t.pick_sel, 0);
        device_flow_key(&mut t, KeyCode::Char('r'), &mut msg); // 重扫(demo 线程结果没人收,无碍)
        assert_eq!(t.conn, Conn::Scanning);
    }

    #[test]
    fn enter_shows_menu_and_menu_keys_dispatch() {
        let mut t = DeviceTab::new(DeviceKind::VibeKeys, true);
        t.enter();
        assert_eq!(t.conn, Conn::Menu);
        let mut msg = String::new();
        // 菜单导航。
        device_flow_key(&mut t, KeyCode::Down, &mut msg);
        assert_eq!(t.menu_sel, 1);
        device_flow_key(&mut t, KeyCode::Up, &mut msg);
        assert_eq!(t.menu_sel, 0);
        // 数字直选:1 = 扫描。
        device_flow_key(&mut t, KeyCode::Char('1'), &mut msg);
        assert_eq!(t.conn, Conn::Scanning);
        // Esc 取消回菜单,2 = 网页配置。
        device_flow_key(&mut t, KeyCode::Esc, &mut msg);
        assert_eq!(t.conn, Conn::Menu);
        t.menu_sel = 1;
        device_flow_key(&mut t, KeyCode::Char('2'), &mut msg);
        assert!(matches!(t.conn, Conn::WebSetup { .. }));
        // Esc 回菜单。
        device_flow_key(&mut t, KeyCode::Esc, &mut msg);
        assert_eq!(t.conn, Conn::Menu);
    }
}
