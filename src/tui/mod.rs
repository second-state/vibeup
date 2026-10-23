//! `vibeup tui` —— ratatui-kit 版本。
//!
//! 三个标签:**Keyboard**(VibeKeys)/ **Watch**(VibeWatch)/ **Packages**
//! (vibetty / vibekeys 安装更新)。架构为 ratatui-kit 的 React 式组件模型:
//! - [`components::App`] —— 根组件:tab 状态、全局输入 handler(优先级链:
//!   退出 > pop 框 > 行内编辑 > 连接流程 > 全局键)、`use_future` 后台轮询
//!   (100ms 一拍:收扫描/会话/包任务事件、推进转圈动画)
//! - [`state`] —— 全部可 Clone 渲染状态 + 后台句柄(`DeviceRuntime` 等)
//! - [`actions`] —— 业务动作与按键分发(自由函数,操作 state + runtime)
//! - [`demo`] —— demo 模式假数据与假任务
//! - [`wizard`] —— `vibeup wizard` 的对话式引导(step-by-step.md),
//!   独立根组件,与标签页界面互不干扰
//!
//! BLE 会话/扫描/下载仍在后台线程(线程 + channel),组件层通过轮询把结果
//! 写回 hook 状态,waker 驱动重绘——界面不冻结、无闪屏。

mod actions;
mod components;
mod demo;
mod state;
mod wizard;

use anyhow::Result;
use ratatui_kit::prelude::*;
use std::path::PathBuf;

pub fn run(dir: Option<PathBuf>, dry_run: bool, demo: bool, wizard: bool) -> Result<()> {
    let dir = crate::pkg::install_dir(dir.as_deref());
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            if wizard {
                element!(wizard::WizardApp(dir: dir, dry_run: dry_run, demo: demo))
                    .fullscreen()
                    .await
            } else {
                element!(components::App(dir: dir, dry_run: dry_run, demo: demo))
                    .fullscreen()
                    .await
            }
        })
        .map_err(|e| anyhow::anyhow!("{e}"))
}
