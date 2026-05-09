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
```

## Install

Grab the prebuilt binaries from
[Releases](https://github.com/ForbesGRyan/network-steam-deck-controller/releases/latest)
or build from source (see [Build from source](#build-from-source)).

### Steam Deck

In **Desktop Mode**, open Konsole:

```sh
# Download the latest release binary (or scp it over from your PC).
curl -L -o network-deck \
  https://github.com/ForbesGRyan/network-steam-deck-controller/releases/latest/download/network-deck-v0.1.0-x86_64-linux
chmod +x network-deck
sudo ./network-deck install
```

The installer is idempotent and:

- installs `usbip` userspace (briefly disables SteamOS readonly to
  `pacman -S usbip` the first time),
- loads `usbip-core` / `usbip-host` / `vhci-hcd` and persists them in
  `/etc/modules-load.d/usbip.conf`,
- enables `usbipd.service` (auto-starts at boot, listens on TCP 3240),
- copies itself to `/var/lib/network-deck/network-deck` (root-owned),
- writes `/etc/sudoers.d/network-deck` (NOPASSWD for the kiosk → daemon
  hop only — the binary is root-owned so it can't be swapped out),
- drops `~/.local/share/applications/network-deck-kiosk.desktop`.

Then add the kiosk to Steam so it appears in Game Mode:

1. Still in Desktop Mode, open Steam.
2. Games → **Add a Non-Steam Game to My Library**.
3. Browse to `/var/lib/network-deck/network-deck`, tick it, **Add Selected
   Programs**.
4. Switch back to Game Mode — **Network Deck** is in your library.

### Windows

Download `client-win-v0.1.0-x86_64-windows.exe` from the release page.
Move it somewhere permanent **before** running (e.g.
`%LOCALAPPDATA%\NetworkDeck\`) — autostart is registered with the path
the binary launched from, so moving it later breaks autostart.

Double-click to run. SmartScreen will warn (the binary isn't code-signed
yet); click **More info** → **Run anyway**.

First launch:

1. If `usbip-win2` is missing, prompts to install. Accept the UAC prompt;
   the Inno Setup wizard runs silently. Windows then shows a
   driver-install dialog for the vhci kernel driver — accept that too.
   (It sometimes opens behind the progress window — check the taskbar.)
2. The pair heads-up appears next. Put the Deck in pair mode, then click
   OK to start broadcasting.
3. After successful pair, the tray app re-launches and starts the bridge.

The tray icon lives in the system tray with menu entries **Connect**,
**Disconnect**, **Pair new Deck...**, **Quit**.

## Pair

First launch on each side handles it automatically. To re-pair later
(swapped Deck, reset trust file, etc.):

- Deck: open the kiosk → it shows the pair screen if there's no trust
  file. Or run `sudo /var/lib/network-deck/network-deck pair`.
- Windows: tray icon → **Pair new Deck...**.

Both sides have 120 s. Confirm the fingerprints match before accepting.

## Use

Tap **Network Deck** in your Game Mode library. The kiosk spawns the
daemon and binds the controller; the Windows tray sees the beacon and
auto-attaches via `usbip.exe`. Pause/resume from the kiosk's big button.
Closing the kiosk SIGTERMs the daemon, which unbinds the controller
before exiting — no leftover background process.

**Pause/resume hotkey:** hold **Volume Up + Volume Down** together on
the Deck to toggle paused. Pause unbinds the controller so it works
locally on the Deck again (handy for typing or navigating SteamOS
without alt-tabbing); resume rebinds it to the PC. Volume buttons live
on a separate ACPI device that `usbip-host` doesn't touch, so the chord
still works mid-session.

Wi-Fi blip looks like a USB unplug to the host, but the state machine
reattaches automatically once the beacon comes back.

## Build from source

```sh
cargo build --workspace
cargo test --workspace
```

`cargo make build-deck` / `build-win` / `verify` are also wired up
(`cargo install cargo-make` first).

Then for the Deck, `sudo ./target/release/network-deck install`. For
Windows, copy `target/release/client-win.exe` to a permanent path and
run it.

## Troubleshooting

- **Tray stuck on "Searching":** broadcasts blocked (guest VLAN, AP
  isolation). Same SSID, no isolation, ideally 5 GHz.
- **Controller drops mid-game:** Wi-Fi blip looks like a USB unplug under
  TCP. The state machine reattaches automatically; some games need a
  restart to pick the controller back up.
- **vhci stuck after suspend:** restart the usbip-win2 service, or
  reinstall it (the tray app will offer this if `usbip.exe` is missing).
- **Kiosk stuck on "First-time setup":** tap **Install** (uses pkexec).
  If pkexec isn't available, run `sudo network-deck install` from
  Konsole instead.

## License

MIT OR Apache-2.0.
