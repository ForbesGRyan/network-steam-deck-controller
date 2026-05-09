# Network Steam Deck Controller

Turn your Steam Deck into a wireless gamepad for a Windows PC.

## Why

Couldn't get a Steam Controller at launch — like a lot of people. The
Deck was sitting right there, but Steam Link was too laggy and chewed
through battery. So instead of streaming the game to the Deck, this
streams the Deck's controller to the PC: just the USB bus, no video, no
encode/decode. Lower latency, lighter on battery.

It's a thin layer of discovery and lifecycle glue around stock USB/IP
(`usbipd` on the Deck, `usbip-win2` on Windows): finds the peer,
attaches the controller, reattaches when Wi-Fi blips. Steam Input sees
a real Deck controller, so every game with Deck support just works.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the design and tradeoffs.

## Install

Grab the latest binaries from
[Releases](https://github.com/ForbesGRyan/network-steam-deck-controller/releases/latest).

**Steam Deck** — Desktop Mode → Konsole:

```sh
curl -LO https://github.com/ForbesGRyan/network-steam-deck-controller/releases/latest/download/network-deck-v0.1.0-x86_64-linux
chmod +x network-deck-v0.1.0-x86_64-linux
sudo ./network-deck-v0.1.0-x86_64-linux install
```

Then in Steam (still Desktop Mode), **Add a Non-Steam Game** → browse to
`/var/lib/network-deck/network-deck`. Switch to Game Mode to launch it.

**Windows** — download `client-win-v0.1.0-x86_64-windows.exe`, drop it in
`%LOCALAPPDATA%\NetworkDeck\`, and double-click. Accept the UAC + driver
prompts to auto-install `usbip-win2` on first run.

> Put the .exe in its final location *before* running — autostart records
> whatever path it launched from.

## Pair

Launch both sides at once and confirm matching fingerprints within 120 s.
First-run flow handles it automatically; re-pair via the kiosk's Setup
screen or the tray's **Pair new Deck...** entry.

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
