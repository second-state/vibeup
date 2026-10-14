//! `vibeup tui` —— 三个标签:**键盘** / **手表**(BLE 设备配置)+ **管理**
//! (vibetty / vibekeys 的安装更新)。
//!
//! Tab 或 1/2/3 切标签。设备页是连接流程:进入 tab 自动后台扫描(界面转圈不
//! 冻结),扫到后列出候选设备让用户挑,选中连接成功才进入配置界面(见
//! `device::Conn`)。配置界面按功能分节(server / wifi / asr / …),每行是
//! 「动作 + 当前值」,选中后回车:可编辑项原地变成编辑框,复杂输入(加 WiFi)
//! 弹居中的 pop 框分字段填写,立即执行的项直接跑。wifi 节是动态列表:现有
//! 网络每条一行(回车改密码,`d` 删除),末尾固定一行 `add +` 弹框新增。
//! 连接成功后持有常驻 BLE 会话(`ble::Session`,后台线程),设备动作瞬时完成、
//! 断连自动弹窗(vibekeys_app 式检测);扫描/安装这类要打 stdout 或耗时的动作
//! 先退出 TUI 模式,结束后再回来(见 `view::suspend`)。demo 模式用内存假数据
//! 和假设备名走完同样的流程,不碰蓝牙。
//!
//! 结构(React 式分层):
//!
//! - [`app`]     —— `App`:全部状态 + 事件循环 + 按键分发(只改状态,不画)
//! - [`element`] —— `Element` 虚拟树 + 通用渲染器(相当于 react-dom)
//! - [`view`]    —— UI 组件:纯函数 `(state, props) -> Element`,拼出整棵树
//! - [`device`]  —— 设备页状态机:连接流程、常驻会话、设备动作
//! - [`fields`]  —— 设备页行模型:字段 / 动作 / 弹框 / 行构建
//! - [`input`]   —— 带光标的单行输入框
//! - [`demo`]    —— demo 模式假数据与假任务
//! - [`pkgs`]    —— 管理标签的包条目

mod app;
mod demo;
mod device;
mod element;
mod fields;
mod input;
mod pkgs;
mod view;

use anyhow::Result;
use std::path::PathBuf;

pub fn run(dir: Option<PathBuf>, dry_run: bool, demo: bool) -> Result<()> {
    app::App::new(dir, dry_run, demo).run()
}
