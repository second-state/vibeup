# vibeup

English | [简体中文](docs/README.zh-CN.md)

**vibeup** is the host-side installer and configuration tool for the vibe family — [vibetty](https://github.com/second-state/vibetty), [vibekeys](https://github.com/second-state/vibekeys_app), and their companion hardware (VibeKeys keyboard / VibeWatch watch).

It does two things:

1. **Package management** — download and install prebuilt binaries from each project's GitHub Releases, following the same `<bin>-<version>` + symlink convention as the projects' `install.sh`.
2. **Device configuration** — talk to a VibeKeys keyboard or VibeWatch watch over BLE and manage its config: WiFi networks, MQTT server, ASR service, mic mode, key remapping, Tailscale binding, and more.

A built-in TUI ties both together in one panel.

## Install

Build from source (Rust 1.85+ / edition 2024):

```sh
cargo install --path .
```

or build a static Linux binary:

```sh
cargo build --release
```

Supported host platforms: `linux-x64`, `macos-arm64`, `windows-x64`.

## Usage

Run `vibeup` with no subcommand (or `vibeup status`) to see what's installed vs. the latest release:

```
package  installed  latest  latest-pre
vibetty  0.4.0      0.4.2   -
vibekeys 0.3.1      0.4.0   0.4.3-rc.1
```

### Packages

```sh
vibeup install vibetty              # latest stable release
vibeup install vibekeys v0.4.3-rc.1 # pin a specific tag
vibeup update                       # update everything
vibeup update vibetty --pre         # update one package to the latest prerelease
vibeup uninstall vibekeys
```

Global flags:

| Flag | Effect |
|---|---|
| `--dir <PATH>` | Install directory (default `~/.cargo/bin`) |
| `--dry-run` | Print what would happen without downloading or writing |

Binaries are installed atomically as `<bin>-<version>` with a `<bin>` symlink (a copy on Windows) pointing at the active version; superseded versions are cleaned up automatically.

### Device configuration (BLE)

The `config` subcommands configure a VibeKeys keyboard or VibeWatch watch over Bluetooth LE. Both devices advertise the same service UUID, so the device type must be given explicitly (`vibekeys` or `watch`); vibeup connects to the first matching device it finds.

```sh
vibeup config show vibekeys                 # dump the full CONFIG snapshot

# WiFi — list order is connection priority, max 8 entries
vibeup config wifi watch show
vibeup config wifi watch add MySSID secret123
vibeup config wifi watch remove MySSID
vibeup config wifi vibekeys set '[{"ssid":"a","pass":"x"},{"ssid":"b","pass":""}]'

vibeup config server-url watch mqtt://broker.local:1883

# Whisper ASR service
vibeup config asr vibekeys --uri ws://asr.local:9000 --api-key KEY --model base.en
vibeup config asr vibekeys --clean          # clear ASR config

# Keyboard-only settings
vibeup config mic-model vibekeys ptt        # ptt (push-to-talk) | toggle
vibeup config prefer-asr vibekeys on

# Tailscale (watch reboots into the Tailscale page after this write)
vibeup config tailscale watch tskey-auth-...

# Key remapping (same binding syntax as `vibekeys keymap`)
vibeup config keymap vibekeys MIC '"hello\n"'   # quoted text = text macro
vibeup config keymap vibekeys CUSTOM CTRL+X     # single keys / combos

vibeup config reset watch                   # reboot the device to apply config
```

Many subcommands accept `--reset` to reboot the device after writing so the new config takes effect immediately.

#### Keymap binding syntax

`config keymap` rebinds one of the keyboard's physical keys — `MIC`, `CUSTOM`, `ESC`, `NEXT`, `BACKSPACE`, `SWITCH`, `ACCEPT`, `ROTATE` — to a new action. The binding argument takes one of these forms:

| Form | Meaning | Example |
|---|---|---|
| `"text"` | Text macro — typed verbatim when the key is pressed; escapes: `\n` `\r` `\t` `\\` `\"` `\0` `\xHH` | `'"/compact\n"'` types `/compact` then Enter |
| `A`, `5` | Single uppercase letter or digit | `Y` |
| key name | Any name from the table below | `delete`, `esc`, `f5` |
| `Mod+Key` | Key combo; modifiers: `ctrl` `alt` `shift` `option` `win` `cmd` `gui` `meta` | `Ctrl+C`, `Alt+Delete` |

Special key names (case-insensitive):

| Names | Key | Names | Key |
|---|---|---|---|
| `enter`, `return` | Enter | `space` | Space |
| `tab` | Tab | `escape`, `esc` | Escape |
| `backspace` | Backspace | `delete` | Delete |
| `insert` | Insert | `home`, `end` | Home / End |
| `pageup`, `pagedown` | Page Up / Page Down | `up`, `down`, `left`, `right` | Arrow keys |
| `f1` … `f12` | Function keys | `minus` | `-` |
| `equal`, `plus` | `=` key (for a literal `+` use `Shift+Equal`) | `semicolon`, `quote` | `;` `'` |
| `backquote`, `backslash` | `` ` `` `\` | `comma`, `period`, `slash` | `,` `.` `/` |
| `bracketleft`, `bracketright` | `[ ]` | `ctrl`, `shift`, `alt`, `gui`, `meta`, `win`, `cmd` | modifier keys pressed alone |

```sh
vibeup config keymap vibekeys CUSTOM delete       # CUSTOM acts as Delete
vibeup config keymap vibekeys MIC Ctrl+X          # MIC sends Ctrl+X
vibeup config keymap vibekeys ESC '"/compact\n"'  # ESC types /compact + Enter
```

### TUI

```sh
vibeup tui          # open the panel
vibeup tui --demo   # same flow with fake devices and data (no BLE)
```

Three tabs, switched with `Tab` or `1`/`2`/`3`:

- **Keyboard** / **Watch** — BLE device setup: entering the tab starts a background scan, discovered devices are listed for you to pick, and connecting opens a config screen grouped by feature (server / wifi / asr / …). Each row shows an action plus its current value; editable items turn into in-place input boxes, complex entries (adding WiFi) open a centered popup form, and the wifi list is dynamic (Enter to edit a password, `d` to delete, `add +` for a new entry). A resident BLE session keeps the connection alive and pops up a notice if the device disconnects.
- **Packages** — install, update, and uninstall vibetty / vibekeys.

Actions that need the terminal (scanning, installing) temporarily suspend the TUI and return when done.

## Adding a new package

Add one entry to the registry in [`src/pkg.rs`](src/pkg.rs) — the only requirement is that the release asset is named `<bin>-<platform>` (e.g. `vibetty-linux-x64`):

```rust
Pkg { name: "mytool", repo: "second-state/mytool", bin: "mytool" },
```

## Project layout

```
src/
├── main.rs         CLI definition (clap)
├── pkg.rs          package registry, platform assets, installed/release lookup
├── install.rs      download → verify → atomic install → symlink switching
├── status.rs       `vibeup status`
├── ble.rs          BLE central: scan/connect/read/write the CONFIG characteristic
├── config_cmd.rs   `vibeup config …`
└── tui/            the TUI, layered React-style:
    ├── app.rs      state + event loop + key dispatch
    ├── element.rs  virtual element tree + generic renderer
    ├── view.rs     UI components: pure (state, props) -> Element
    ├── device.rs   device-tab state machine, resident BLE session
    ├── fields.rs   row model: fields / actions / popups
    ├── input.rs    single-line input with cursor support
    ├── demo.rs     fake data for --demo mode
    └── pkgs.rs     package-tab entries
```

The BLE protocol is ported from [vibekeys_app](https://github.com/second-state/vibekeys_app): both devices expose a CONFIG characteristic (READ = full snapshot, WRITE = JSON partial patch) plus a KEYMAP characteristic for key remapping and a RESET characteristic (write ASCII `RESET` to reboot).

Static musl builds compile a vendored `libdbus` from source (the BlueZ backend used by btleplug); glibc builds are unaffected.
