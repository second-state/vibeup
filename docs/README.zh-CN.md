# vibeup 中文说明

[English](../README.md) | 简体中文

**vibeup** 是 vibe 家族的主机端工具:负责安装/更新 [vibetty](https://github.com/second-state/vibetty)、[vibekeys](https://github.com/second-state/vibekeys_app) 及其配套硬件(VibeKeys 键盘 / VibeWatch 手表)。

两大功能:

1. **包管理** —— 从各项目的 GitHub Releases 下载对应平台的预编译二进制并安装,遵循各仓库 `install.sh` 的 `<bin>-<版本>` + symlink 约定。
2. **设备配置** —— 通过 BLE 连接 VibeKeys 键盘 / VibeWatch 手表,管理设备配置:WiFi 列表、MQTT 服务器、ASR 服务、麦克风模式、键位重映射、Tailscale 绑定等。

内置 TUI 面板把两者整合在一起。

## 安装

从源码构建(需要 Rust 1.85+ / edition 2024):

```sh
cargo install --path .
```

或构建 Linux 静态二进制:

```sh
cargo build --release
```

支持的主机平台:`linux-x64`、`macos-arm64`、`windows-x64`。

## 使用

不带子命令直接运行 `vibeup`(或 `vibeup status`),查看已装版本 vs 最新版本:

```
package  installed  latest  latest-pre
vibetty  0.4.0      0.4.2   -
vibekeys 0.3.1      0.4.0   0.4.3-rc.1
```

### 包管理

```sh
vibeup install vibetty              # 最新稳定版
vibeup install vibekeys v0.4.3-rc.1 # 指定 tag
vibeup update                       # 更新全部包
vibeup update vibetty --pre         # 更新单个包到最新 prerelease
vibeup uninstall vibekeys
```

全局参数:

| 参数 | 作用 |
|---|---|
| `--dir <PATH>` | 安装目录(默认 `~/.cargo/bin`) |
| `--dry-run` | 只打印将要做的事,不实际下载/写盘 |

二进制以 `<bin>-<版本>` 原子安装,`<bin>` symlink(Windows 上是副本)指向当前生效版本;被替换的旧版本自动清理。

### 设备配置(BLE)

`config` 子命令通过蓝牙 LE 配置 VibeKeys 键盘 / VibeWatch 手表。两台设备广播同一个 service UUID,所以必须显式指定设备类型(`vibekeys` 或 `watch`);vibeup 会连接扫描到的第一台匹配设备。

```sh
vibeup config show vibekeys                 # 打印完整 CONFIG 快照

# WiFi —— 列表顺序即连接优先级,最多 8 条
vibeup config wifi watch show
vibeup config wifi watch add MySSID secret123
vibeup config wifi watch remove MySSID
vibeup config wifi vibekeys set '[{"ssid":"a","pass":"x"},{"ssid":"b","pass":""}]'

vibeup config server-url watch mqtt://broker.local:1883

# Whisper ASR 服务
vibeup config asr vibekeys --uri ws://asr.local:9000 --api-key KEY --model base.en
vibeup config asr vibekeys --clean          # 清空 ASR 配置

# 仅键盘支持的设置
vibeup config mic-model vibekeys ptt        # ptt(按住说话)| toggle(点按切换)
vibeup config prefer-asr vibekeys on

# Tailscale(写入后手表自动跳转 Tailscale 页)
vibeup config tailscale watch tskey-auth-...

# 键位重映射(binding 语法同 `vibekeys keymap`)
vibeup config keymap vibekeys MIC '"hello\n"'   # 加引号 = 文本宏
vibeup config keymap vibekeys CUSTOM CTRL+X     # 单键 / 组合键

vibeup config reset watch                   # 重启设备使配置生效
```

多数子命令支持 `--reset`,写完立即重启设备让新配置生效。

### TUI

```sh
vibeup tui          # 打开面板
vibeup tui --demo   # 用假设备假数据走完同样流程(不碰蓝牙)
```

三个标签,`Tab` 或 `1`/`2`/`3` 切换:

- **键盘** / **手表** —— BLE 设备配置:进入标签自动后台扫描,扫到后列出候选设备供挑选,连接成功后进入按功能分节的配置界面(server / wifi / asr / …)。每行是「动作 + 当前值」:可编辑项原地变成编辑框,复杂输入(加 WiFi)弹居中的 pop 框分字段填写;wifi 列表是动态的(回车改密码,`d` 删除,`add +` 新增)。连接成功后持有常驻 BLE 会话,设备断连会自动弹窗提示。
- **Packages** —— 安装、更新、卸载 vibetty / vibekeys。

需要占用终端的动作(扫描、安装)会先暂时退出 TUI 模式,结束后再回来。

