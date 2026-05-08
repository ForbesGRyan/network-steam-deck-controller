# network-usb

Wireless gamepad bridge: Steam Deck controls → Windows PC. Built on stock
USB/IP — `usbip-host` on the Deck, `usbip-win2` on Windows. This repo
provides the discovery + lifecycle glue that makes the two ends find each
other and stay attached.

> **Status:** complete pivot. `discovery` crate, `server-deck`, and
> `client-win` are all rewritten around the usbip backend. Hardware-tested:
> `usbipd` + `usbip-win2 v0.9.7.7` produces a Deck that Steam recognizes
> and works in-game with no custom driver required. See
> [ARCHITECTURE.md](ARCHITECTURE.md) for design detail.

## Why

You have a Steam Deck and a Windows gaming PC. You want to use the Deck as a
wireless controller — including its gyro, trackpads, and back paddles —
without plugging anything in. This project does that by:

1. Running `usbipd` on the Deck and binding the internal controller's USB device.
2. Shipping the USB bus over TCP to your PC via `usbip-win2`.
3. Windows sees a real Steam Deck controller on a virtual USB host.
4. Steam Input recognizes it natively — every Steam game that already
   supports the Deck just works.

Why not VirtualHere / ViGEmBus / Steam Link?
[ARCHITECTURE.md](ARCHITECTURE.md#why-this-design) walks through each
alternative and why this design was chosen.

## Layout

```
crates/
  discovery/      Ed25519 identity, signed-UDP beacon, trust file, pair flow
  server-deck/    Linux binary: sysfs busid lookup + usbip bind state machine
  client-win/     Windows binary: tray app + usbip.exe attach state machine
scripts/
  install-deck.sh          Deck-side installer (pacman + systemd)
  install-windows.ps1      Windows-side installer (usbip-win2 + binary drop)
ARCHITECTURE.md   design history, component diagram, open risks
```

## Build

The Rust workspace builds anywhere `cargo` runs:

```sh
cargo build --workspace
cargo test --workspace
```

Per-binary platform support:

| Binary | Real platform | Other platforms |
|---|---|---|
| `server-deck` | Linux (shells out to `usbip`) | builds, exits with "Linux only" |
| `client-win` | Windows (shells out to `usbip.exe`, Win32 tray) | builds, no-ops tray calls |

## Install

### On the Deck (SteamOS)

```
sudo ./scripts/install-deck.sh
```

This installs `usbip` from pacman, enables `usbipd.service`, drops
`server-deck` into `/usr/local/bin`, and installs the systemd unit.

### On Windows

From an elevated PowerShell:

```
.\scripts\install-windows.ps1
```

Installs usbip-win2 (signed driver, accept the Windows driver dialog),
drops `client-win.exe` into `%LOCALAPPDATA%\NetworkDeck`, and the tray
self-registers for autostart on first run.

### Pair

One-shot. Run on each side at the same time:

```
sudo /usr/local/bin/server-deck pair --state-dir /var/lib/network-deck   # Deck
& "$env:LOCALAPPDATA\NetworkDeck\client-win.exe" pair                    # Windows
```

Confirm matching fingerprints on both prompts within 120 s. After pair,
enable the Deck service:

```
sudo systemctl enable --now network-deck-server.service
```

The Windows tray will pick up the Deck's beacon and auto-attach.

## Troubleshooting

**Tray stays "Searching":** broadcasts may be filtered (guest VLAN, AP
isolation). Try a wired connection or join both devices to the same SSID
without isolation.

**Controller drops mid-game:** a Wi-Fi blip causes a visible USB unplug
under TCP transport. Some games recover; some require a restart. Keep
the bridge on 5 GHz. The attach state machine retries automatically so
the next game session will pick up without manual intervention.

**vhci not available after suspend:** run `.\scripts\install-windows.ps1`
again (idempotent) or restart the usbip-win2 service; the tray's attach
state machine will reconnect once the driver is ready.

## License

The Rust workspace is dual-licensed under MIT OR Apache-2.0 (see the
`license` field in `Cargo.toml`).
