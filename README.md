# network-usb

Use a Steam Deck as a wireless controller for a Windows PC.

## Why this exists

A stepping stone to a real Steam Controller. I couldn't grab one on launch
day, like a lot of people, so I wanted to use my Steam Deck as the
controller for my PC in the meantime. Steam Link had too much latency and
drained the Deck's battery too fast to be usable. This sips battery and
adds less latency, because it's only a USB-bus tunnel — fewer round
trips back to the PC, no video stream, no encode/decode.

Built on stock USB/IP: `usbipd` on the Deck, `usbip-win2` on Windows.
This repo is the discovery + lifecycle glue that finds the peer, attaches
the controller, and reattaches when Wi-Fi blips. Steam Input sees a real
Deck controller, so every game with Deck support just works.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the design and tradeoffs.

## Layout

```
crates/discovery/      Ed25519 identity, signed UDP beacon, pair flow
crates/network-deck/   Single Deck binary: GUI + daemon + pair + installer
crates/client-win/     Windows tray app driving usbip.exe attach
scripts/install-windows.ps1
```

## Build

```sh
cargo build --workspace
cargo test --workspace
```

`cargo make build-deck` / `build-win` / `verify` are also wired up
(`cargo install cargo-make` first).

## Install

**Deck (SteamOS):**

```sh
cargo build --release -p network-deck
sudo ./target/release/network-deck install
```

Idempotent: installs `usbip`, loads the kernel modules, enables
`usbipd.service`, copies itself into `~/network-deck/`, writes a sudoers
entry for the GUI→daemon hop, drops a `.desktop` file. Add
`~/network-deck/network-deck` to Steam from Desktop Mode and it shows up
in your Game Mode library.

**Windows (elevated PowerShell):**

```powershell
.\scripts\install-windows.ps1
```

Installs usbip-win2 (accept the driver dialog), drops `client-win.exe`
into `%LOCALAPPDATA%\NetworkDeck`, registers tray autostart on first run.

## Pair

One-shot. Run on each side at the same time, confirm matching
fingerprints within 120 s:

```
sudo ~/network-deck/network-deck pair                          # Deck
& "$env:LOCALAPPDATA\NetworkDeck\client-win.exe" pair          # Windows
```

## Use

Tap **Network Deck** in your Game Mode library. The kiosk spawns the
daemon; the Windows tray sees the beacon and auto-attaches. Pause/resume
from the kiosk button. Closing the kiosk SIGTERMs the daemon, which
unbinds the controller before exiting — no leftover background process.

## Troubleshooting

- **Tray stuck on "Searching":** broadcasts blocked (guest VLAN, AP
  isolation). Same SSID, no isolation, ideally 5 GHz.
- **Controller drops mid-game:** Wi-Fi blip looks like a USB unplug under
  TCP. The state machine reattaches automatically; some games need a
  restart to pick the controller back up.
- **vhci stuck after suspend:** rerun `install-windows.ps1` (idempotent)
  or restart the usbip-win2 service.
- **Kiosk stuck on "First-time setup":** tap **Install** (uses pkexec).
  If pkexec isn't available, run `sudo network-deck install` from
  Konsole instead.

## License

MIT OR Apache-2.0.
