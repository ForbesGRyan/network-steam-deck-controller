# network-steam-deck-controller

Wireless bridge that streams Steam Deck controller inputs over Wi-Fi to a
Windows PC, where they appear as a virtual Steam Deck controller. Steam Input
on Windows handles all gameplay-side mapping (gyro, trackpads, back paddles,
per-game profiles).

> **Status:** working scaffold. Rust crates compile and pass 30 unit
> tests. The Deck → hidraw → wire pipeline has been validated on real
> hardware. The Windows kernel driver enumerates as a real Steam Deck
> Controller, Steam Input loads its `controller_neptune` config, and live
> HID frames + rumble flow through. End-to-end network validation is
> still gated on a Hyper-V external-vSwitch UDP-drop issue we have not
> yet bypassed (USB ethernet adapter or bare-metal pivot). See
> [ARCHITECTURE.md](ARCHITECTURE.md) for the full design and remaining
> validations.

## Why

You have a Steam Deck and a Windows gaming PC. You want to use the Deck as a
wireless controller — including its gyro, trackpads, and back paddles —
without plugging anything in. This project does that by:

1. Reading the Deck's internal controller via `/dev/hidraw` on the Deck.
2. Shipping canonical state over UDP to your PC.
3. A small Windows kernel driver presents a virtual Steam Deck controller
   to the OS.
4. Steam Input recognizes it natively — every Steam game that already
   supports the Deck just works.

Why not VirtualHere / USB-over-IP / ViGEmBus / Steam Link?
[ARCHITECTURE.md](ARCHITECTURE.md#why-this-design-decision-history) walks
through each alternative and why this design was chosen.

## Layout

```
crates/
  deck-protocol/    types + HID codec + wire codec (no I/O, no_std-friendly)
  server-deck/      Linux binary: hidraw -> wire -> UDP
  client-win/       Windows binary: UDP -> wire -> HID -> driver IOCTL
driver/             KMDF + UdeCx kernel driver (C++) + INF
tools/              fetch-on-demand reference materials
ARCHITECTURE.md     design history, build sequence, pending validations
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
| `server-deck` | Linux (uses `/dev/hidraw*`) | builds, exits with "Linux only" |
| `client-win` | Windows (uses Win32 + driver IOCTL) | builds, runs in listen-only mode |

The kernel driver is **not** in the Cargo workspace. It needs Visual Studio
+ the Windows Driver Kit; see [driver/README.md](driver/README.md) for
setup.

## Run

### Build

```sh
cargo build --workspace --release
cargo test --workspace
```

### Windows side (PC running games)

1. **Build and install the kernel driver.** The driver is *not* part of
   the Cargo workspace; it needs Visual Studio + the WDK. See
   [driver/README.md](driver/README.md). After building:

   ```powershell
   # Elevated PowerShell.
   bcdedit /set testsigning on   # one-time, until you have an EV cert
   # Reboot, then:
   pwsh -ExecutionPolicy Bypass -File driver\scripts\install.ps1
   ```

   `install.ps1` wraps `pnputil /add-driver` plus the
   `devcon install root\NetworkDeckController` call needed to instantiate
   the root-enumerated PnP node. Uninstall via `driver\scripts\uninstall.ps1`.

2. **Run the user-mode service.** Listens on UDP 49152 by default, attaches
   to the kernel driver, parks an output IOCTL for rumble feedback. If the
   driver isn't installed yet, runs in listen-only mode and reattaches the
   moment it appears (`DriverHolder` retries every 5 s).

   ```powershell
   .\target\release\client-win.exe 49152
   # or with HMAC packet auth:
   $env:NETWORK_DECK_KEY = "<64 hex chars>"
   .\target\release\client-win.exe 49152
   ```

   Useful test modes:
   - `client-win.exe --test` synthesizes alternating-A-button reports at
     ~250 Hz so Steam's Controller Layout test screen can verify the
     virtual device works without a Deck attached.
   - `client-win.exe --replay <hidraw-capture.bin>` replays a captured
     hidraw stream from a real Deck (`cat /dev/hidrawN > foo.bin` on the
     Deck) at full cadence.

### Deck side (Linux)

```sh
# One-time: udev rule so the deck user can read hidraw without sudo.
sudo cp crates/server-deck/scripts/70-steam-deck.rules /etc/udev/rules.d/
sudo udevadm control --reload && sudo udevadm trigger

# Run server. Match port to client-win's listen port.
./target/release/server-deck /dev/hidrawN <windows-ip>:49152
```

For unattended setups, install the systemd unit:

```sh
sudo cp crates/server-deck/scripts/network-deck-server.service /etc/systemd/system/
# Edit the Environment= lines for your hidraw path + Windows IP, then:
sudo systemctl daemon-reload
sudo systemctl enable --now network-deck-server.service
```

To enable HMAC packet auth, write the same hex key on both ends:

```sh
echo 'NETWORK_DECK_KEY=<64 hex chars>' | sudo tee /etc/network-deck.env
sudo systemctl restart network-deck-server.service
```

### What you'll see

Both binaries print live stats. Steam's Controller Layout test screen
mirrors button presses and stick / trackpad state with single-frame
latency over a clean Wi-Fi link.

To find the right hidraw node on the Deck:
```sh
for f in /sys/class/hidraw/hidraw*/device/uevent; do
    grep -l "HID_ID=0003:000028DE:00001205" "$f" \
        && echo "  -> ${f%/device/uevent}"
done
```

While Steam owns the controller in `hid-steam` mode, hidraw won't see
gamepad-state frames. To get raw frames, kill Steam and unbind hid-steam:
```sh
echo -n "<phys-id>" | sudo tee /sys/bus/hid/drivers/hid-steam/unbind
```

## What's left

Tracked in detail in [ARCHITECTURE.md](ARCHITECTURE.md#build-sequence):

1. Lift Deck HID layout into `deck-protocol`. — done
2. Hidraw spike: validate `BUTTON_MAP` against real hardware. — done
3. Static UDE driver: descriptors, plug-in flow, "Steam Deck Controls"
   appears in Steam. — done (Hyper-V VM with test signing)
4. Feature-report path: lizard-mode disable, haptics-config ack. — done
   (Steam recognizes as Deck, `controller_neptune` config loads)
5. User-mode IPC + live HID frames over IOCTL. — done
   (`client-win --test` toggles the A button visible in Steam at 1 Hz)
6. Real Deck bytes → driver → Steam (protocol). — done
   (replayed 1249 captured frames via `client-win --replay`, button
   transitions show in Steam Controller Layout)
7. Output channel: rumble/haptics back to Deck. — done (path wired;
   end-to-end validation pending network and a Deck running `server-deck`)
8. Polish: reconnect, pairing, packaging. — done
   (driver/hidraw reopen on transient failure, optional HMAC-SHA256
   per-packet auth + 30 s replay window via `NETWORK_DECK_KEY`,
   PowerShell install scripts, systemd unit + udev rule)

## License

The Rust workspace is dual-licensed under MIT OR Apache-2.0 (see the
`license` field in `Cargo.toml`). The driver will follow the same once
its bodies are written.

Reference materials under `tools/reference/` (Linux kernel sources used to
derive the HID codec) are GPL-2.0+ and intentionally not committed —
fetch them on demand per [tools/README.md](tools/README.md).
