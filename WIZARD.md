# vibeup — configuration guide for agents

This document tells an AI agent (Claude Code, Codex, …) how to walk a user
through installing the vibe tools and configuring their device
(**VibeKeys** BLE keyboard / **VibeWatch** BLE watch).

**Golden rule: the agent runs every command itself.** Never tell the user to
open `vibeup`'s TUI or type commands — ask the user questions in chat, then
execute the matching CLI command and report the output. The user's only jobs
are: answering questions, pressing buttons on the device, and approving
installs.

## Step 0 — check vibeup itself

```sh
command -v vibeup && vibeup --version
```

- **Not installed → ask the user first.** Explain what it is and what the
  install does, and only proceed on their explicit yes:
  > "vibeup isn't installed yet. It's the installer/configurator for the
  > vibe hardware (VibeKeys keyboard, VibeWatch). Installing it downloads a
  > prebuilt binary to `~/.cargo/bin` via the official install script. Want
  > me to install it?"

  On agreement:
  ```sh
  curl -fsSL https://raw.githubusercontent.com/second-state/vibeup/main/install.sh | bash
  ```
  (or download a binary from
  <https://github.com/second-state/vibeup/releases/latest> and put it on
  `PATH` — see the [README](README.md#install)). If the user declines, stop —
  don't install anything.
- Installed but old → suggest `vibeup update self` (also ask first).
- No Bluetooth adapter on this machine at all? BLE configuration is
  impossible — tell the user up front (the device also needs to be
  physically nearby).
- **Before scanning for a VibeKeys**: the keyboard must be in **keyboard
  mode** first (toggle with its mode switch) — **then** press **ACCEPT** to
  start advertising. Just pressing ACCEPT in another mode does nothing.
- **Have the user connect the device in the OS Bluetooth settings first**
  (e.g. GNOME/KDE Settings → Bluetooth → add device → pick the VibeKeys /
  VibeWatch; macOS Control Center; Windows Settings → Bluetooth & devices).
  Pairing at the system level makes the subsequent BLE configuration
  connection reliable. Confirm it shows as connected/paired in that panel
  before running any `vibeup config …` command.

All commands below support `--dry-run` (global flag) to preview without
touching anything — useful for showing the user what will happen.

## Decision flow (same branches as the interactive wizard)

```
Which device? ── VibeKeys keyboard ──► wifi → input method → vibekeys+plugins → vibetty/broker
              └── VibeWatch watch ───► wifi → ensure vibetty ───────► vibetty/broker
```

Ask the user, then follow the matching branch. Every step is optional and
skippable; anything already configured is preserved (wifi entries append,
not replace).

### 1. WiFi (both devices, skippable)

> "The device needs WiFi to reach the broker. Want to add a network? I'll
> need the SSID and password."

```sh
vibeup config wifi <vibekeys|watch> show              # current list (order = priority)
vibeup config wifi <device> add <ssid> [pass]         # append / overwrite same ssid, max 8
vibeup config wifi <device> remove <ssid>
vibeup config wifi <device> set '[{"ssid":"a","pass":"…"}, …]'   # replace whole list
```

Existing entries are kept; same SSID overwrites; hard cap 8
(`wifi list is full (8/8)` — remove one first).

### 2. Input method (VibeKeys only) — ask the user

- **"How do you type — with an IME, or on-device voice ASR?"**
  - **Local input (IME)** → bind their IME hotkey to the MIC key and disable
    built-in ASR:
    ```sh
    vibeup config keymap vibekeys MIC 'Ctrl+Shift+Space'
    vibeup config prefer-asr vibekeys off
    ```
  - **Whisper-compatible API (on-device ASR)** → ask for the endpoint URL
    and API key:
    ```sh
    vibeup config asr vibekeys --uri <url> --api-key <key>
    vibeup config prefer-asr vibekeys on
    ```

Keymap binding syntax (same as `vibekeys keymap`; full reference in
`vibeup config keymap --help`):

- `"text"` — text macro typed verbatim, e.g. `'"/compact\n"'`
- `A`, `5` — single letter/digit
- `name` — key name (`enter`, `space`, `tab`, `esc`, `delete`, `f1`–`f12`, …)
- `Mod+Key` — combo, e.g. `Ctrl+C`, `Alt+Delete`, `Ctrl+Shift+T`
  (modifiers: `ctrl alt shift option win cmd gui meta`)

Remappable physical keys: `MIC CUSTOM ESC NEXT BACKSPACE SWITCH(YOLO) ACCEPT ROTATE`.

Note for IME users: bare modifiers (lone Ctrl/Alt) can't be a hotkey —
recommend a combo like `Ctrl+Shift+Space`.

### 3. vibekeys + agent plugins (VibeKeys only, skippable)

> "Install vibekeys-app? It shows Claude Code / Codex work status on the
> keyboard." (ask before installing)

```sh
vibeup install vibekeys                 # binary → ~/.cargo/bin + jump script → ~/.vibekeys/scripts
```

Then, for each CLI present on `PATH` (`command -v claude` / `command -v codex`),
ask and run on agreement:

```sh
claude plugin marketplace add second-state/marketplace
claude plugin install vibekeys@second-state-tools
codex plugin marketplace add second-state/marketplace
codex plugin add vibekeys@second-state-tools
```

The plugin carries all the hook config — installing it is enough. If a
plugin install fails, retry once, then skip and give the user the two
commands to run later.

### 4. vibetty + broker (both devices; watch path starts here)

vibetty is the remote-control host side. Check its state first:

```sh
vibeup status --json                    # is vibetty installed?
cat ~/.vibetty/config.toml              # [mqtt] broker set?
```

- vibetty missing → ask, then `vibeup install vibetty`.
- If a broker is already configured, push it to the device (see step 5) —
  if it points at `127.0.0.1`/`::1`, replace the host with a reachable
  address (step 5a) first.
- Otherwise ask: **"Will you use this outside your LAN?"**

#### 4a. LAN only

Detect this machine's LAN IP and use it as the broker host (an MQTT broker
ships inside vibetty, listening on port 1883):

```sh
vibeup config server-url <device> <LAN-IP>:1883
```

and point the host-side vibetty at itself — write to
`~/.vibetty/config.toml`:

```toml
[mqtt]
broker = "mqtt://127.0.0.1:1883"
```

(create the file if missing; preserve the rest of an existing file).

#### 4b. Outside the LAN

Ask: **"Do you have a Tailscale account?"**

- **Yes / wants Tailscale (recommended, free)** — walk the user through
  <https://login.tailscale.com/admin/settings/keys> → generate a **one-off**
  auth key (`tskey-auth-…`), then:
  ```sh
  vibeup config tailscale <device> <auth-key>
  ```
  The device joins the tailnet itself and caches its node keys. Then get the
  host's tailscale IP and finish like 4a:
  ```sh
  tailscale ip -4                        # not found? install tailscale + `tailscale up` first
  vibeup config server-url <device> <ts-ip>:1883
  ```
- **No — EMQX Cloud** (free up to 1 GB/month): user signs up at
  <https://www.emqx.com/en/cloud>, agent writes the broker URL to **both**
  the device and `~/.vibetty/config.toml`:
  ```sh
  vibeup config server-url <device> mqtts://<user>:<pass>@xxxx.emqx.cloud:8883
  ```
- **No — self-hosted broker**: same as EMQX, with the user's own URL.

Valid schemes: `mqtt:// mqtts:// ws:// wss://` with a host.

### 5. Verify

```sh
vibeup config show <device>             # full CONFIG snapshot from the device
vibeup status --json                    # installed versions vs latest
```

Tell the user to try it: MIC key should trigger the configured hotkey;
agent status should light up the keyboard when Claude Code/Codex works.

### Device side-notes

- The `device` argument (`vibekeys` | `watch`) connects to the **first
  found** device of that kind — make sure only the target device is nearby
  and advertising.
- Several writes accept `--reset` to reboot the device afterwards so the
  setting applies; a `tailscale` write makes the device jump to its
  Tailscale page and **drop the BLE connection — that is success**, not an
  error.
- `vibeup config reset <device>` reboots the device on demand.

## Where everything lands

| Setting | Location | Written how |
|---|---|---|
| `wifi_list` (max 8, order = priority) | device | `vibeup config wifi …` over BLE |
| MIC / other keymap bindings | device | `vibeup config keymap …` over BLE |
| `asr_config` (platform/uri/api_key/model) | device | `vibeup config asr …` over BLE |
| `prefer_builtin_asr` (`on`/`off`) | device | `vibeup config prefer-asr …` over BLE |
| `tailscale_key` | device | `vibeup config tailscale …` over BLE |
| `server_url` (device's broker) | device | `vibeup config server-url …` over BLE |
| vibekeys / vibetty binaries | `~/.cargo/bin` | `vibeup install <name>` |
| vibekeys jump script | `~/.vibekeys/scripts` | automatically after `vibeup install vibekeys` |
| Claude Code / Codex plugin | CLI-managed | `plugin marketplace add` + `plugin install` / `plugin add` |
| vibetty broker | `~/.vibetty/config.toml` `[mqtt] broker` | file edit (create if missing, preserve the rest) |

## Troubleshooting (unexpected situations)

What you'll see and what to do — for CLI runs the error message is printed
to stderr; for the interactive wizard the same text appears in a red alert
box:

| Error / situation | Cause | What to do |
|---|---|---|
| `no devices found` | Device not advertising | **VibeKeys: switch it into keyboard mode first, then press ACCEPT** to re-advertise. VibeWatch: put it in provisioning mode. Wait 2–3 s, retry. Also: get closer to the machine, and make sure the device isn't still connected to another host (phone/another computer) — BLE won't advertise while connected elsewhere |
| Scan error ("No such device", "permission denied", …) | No Bluetooth adapter, or the adapter/BlueZ service is down | Retrying won't fix a missing adapter. Check: `bluetoothctl list` (is there a controller?), `rfkill list bluetooth` (soft-blocked?), `systemctl status bluetooth`. Fix, then retry |
| `connect failed: …` | BLE pairing/connect failed | First make sure the device is paired **in the OS Bluetooth settings** (see Step 0) and shows as connected there, then retry; power-cycle the device if it repeats. If it was just paired with another host, unpair it there first |
| `device disconnected: …` / `device session is closed` mid-config | Link dropped between steps | Reconnect and continue with the remaining steps — earlier steps already succeeded and are saved on the device |
| WiFi write fails | Wrong password, or list full | Check the password; `wifi list is full (8/8): remove one first with --remove` → remove an entry, then retry |
| `could not detect the LAN IP` | No usable network interface | Connect the host to the network, retry |
| `tailscale ip -4` returns nothing | tailscale not installed / not logged in | Install tailscale, `tailscale up`, complete browser login, retry |
| `not a valid broker URL` | Malformed address | Include the scheme and host, e.g. `mqtts://abcdef.emqx.cloud:8883` |
| `vibetty config: …` | Couldn't write `~/.vibetty/config.toml` | Check home directory permissions |
| Plugin install fails | No network / CLI not logged in / CLI too old for `plugin` | Retry once, then give the user the two commands for later; update the CLI if it lacks `plugin` support |
| Download stalls | Network to GitHub slow/blocked | Set `https_proxy` in the environment and retry |

BLE diagnostics (Linux): `bluetoothctl devices` (does the device appear?),
`bluetoothctl info <MAC>` (paired/connected?), `btmon` while reproducing.
On macOS there is no user-facing BLE log — restart Bluetooth from Control
Center and retry.

## Appendix: the interactive wizard

Everything above is also automated by `vibeup wizard`, an interactive TUI
for users who set things up themselves: it asks up front which device to
set up (keyboard or watch), then scans and connects over BLE and walks the
same questions (WiFi → input method → vibekeys + plugins → vibetty →
LAN/Tailscale/EMQX/self-hosted broker), ending with a ✓/✗ summary page. Users reach it via `vibeup wizard`, the `w`
key in the TUI's Packages tab, or an auto-popup on first run (nothing
installed yet). It supports `--demo` to walk the whole flow with fake
hardware. Agents should not send users here — run the commands above
instead.
