# network-usb

Wireless gamepad bridge: Steam Deck controls → Windows PC. Built on stock
USB/IP — `usbip-host` on the Deck, `usbip-win2` on Windows. This repo
provides the discovery + lifecycle glue that makes the two ends find each
other and stay attached.

> **Status:** complete pivot. `discovery` crate, `network-deck`
> (single Deck binary), and `client-win` are all rewritten around the
> usbip backend. Hardware-tested: `usbipd` + `usbip-win2 v0.9.7.7`
> produces a Deck that Steam recognizes and works in-game with no custom
> driver required. See [ARCHITECTURE.md](ARCHITECTURE.md) for design detail.

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
  network-deck/   Single Deck-side binary: GUI + daemon + pair + installer
  client-win/     Windows binary: tray app + usbip.exe attach state machine
scripts/
  install-windows.ps1            Windows-side installer (usbip-win2 + binary drop)
ARCHITECTURE.md   design history, component diagram, wire+IPC contracts, open risks
```

## Build

```sh
cargo build --workspace
cargo test --workspace
```

Per-binary platform support:

| Binary | Real platform | Other platforms |
|---|---|---|
| `network-deck` | Linux (eframe + usbip + signal handling) | builds, exits with "Linux only" |
| `client-win` | Windows (shells out to `usbip.exe`, Win32 tray) | builds, no-ops tray calls |

### cargo-make shortcuts

```sh
cargo install cargo-make    # one-time
cargo make build-deck       # release build of network-deck (Linux)
cargo make build-win        # release build of client-win (Windows)
cargo make install-deck     # build + run `sudo network-deck install`
cargo make install-win      # build + run scripts/install-windows.ps1 (admin)
cargo make pair-deck        # one-shot pair on the Deck
cargo make pair-win         # one-shot pair on Windows
cargo make verify           # test + clippy + fmt --check
```

## Install

### On the Deck (SteamOS)

Build and run the binary's own installer:

```sh
cargo build --release -p network-deck
sudo ./target/release/network-deck install
```

What `install` does (idempotent):

- `pacman -S usbip` if missing (one-time, briefly toggles `steamos-readonly`).
- Loads + persists `usbip-host` / `vhci-hcd` kernel modules.
- Enables `usbipd.service`.
- Copies itself into `~/network-deck/network-deck` (root-owned).
- Writes `/etc/sudoers.d/network-deck` (NOPASSWD for the kiosk → daemon hop).
- Drops a `.desktop` entry in `~/.local/share/applications/`.
- Disables the legacy `network-deck-server.service` if it exists.

#### Add to Game Mode (one-time)

1. Reboot to Desktop Mode (Power → Switch to Desktop).
2. Open Steam (the desktop client).
3. Games → Add a Non-Steam Game to My Library...
4. Browse to `~/network-deck/network-deck` → Add Selected Programs.
5. Switch back to Game Mode; "Network Deck" appears in your library.
6. Tap it whenever you want to share the controller. Closing the window
   stops the daemon and unbinds the controller — no background service.

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
sudo ~/network-deck/network-deck pair                                    # Deck
& "$env:LOCALAPPDATA\NetworkDeck\client-win.exe" pair                    # Windows
```

Confirm matching fingerprints on both prompts within 120 s.

## Use

- Tap **Network Deck** in your Steam library (Game Mode).
- The kiosk window opens and silently spawns the daemon as a child via
  `sudo -n` (allowed by the sudoers entry).
- The Windows tray picks up the Deck's beacon and auto-attaches.
- Pause/resume from the kiosk's button — stays paired, just stops binding.
- Close the kiosk → daemon gets SIGTERM → unbinds → exits cleanly.

## Troubleshooting

**Tray stays "Searching":** broadcasts may be filtered (guest VLAN, AP
isolation). Try a wired connection or join both devices to the same SSID
without isolation.

**Controller drops mid-game:** a Wi-Fi blip causes a visible USB unplug
under TCP transport. Some games recover; some require a restart. Keep
the bridge on 5 GHz. The attach state machine retries automatically so
the next game session picks up without manual intervention.

**vhci not available after suspend:** run `.\scripts\install-windows.ps1`
again (idempotent) or restart the usbip-win2 service; the tray's attach
state machine will reconnect once the driver is ready.

**Kiosk shows "First-time setup":** tap the **Install** button — it
runs `sudo network-deck install` via `pkexec` and prompts for your
password. Close and relaunch when it reports "Setup complete." If
`pkexec` is unavailable (some headless or Game Mode sessions), fall
back to running `sudo network-deck install` from Konsole.

**`sudo -n` fails / "Daemon not running" persists:** the sudoers entry
didn't write. Run `sudo -l` as the deck user — you should see a
`NOPASSWD` entry for `~/network-deck/network-deck daemon`. If not,
re-run `sudo network-deck install`.

## License

The Rust workspace is dual-licensed under MIT OR Apache-2.0 (see the
`license` field in `Cargo.toml`).
