//! Spike: open a Deck hidraw device, parse input reports, optionally send
//! them as wire packets to the Windows client.
//!
//! Usage:
//! ```text
//! server-deck <hidraw-device>                         # read-only, print only
//! server-deck <hidraw-device> <target-addr:port>      # read + send via UDP
//! ```
//!
//! Find the right hidraw node on a Deck:
//!
//! ```text
//! for f in /sys/class/hidraw/hidraw*/device/uevent; do
//!     grep -l "HID_ID=0003:000028DE:00001205" "$f" \
//!         && echo "  -> ${f%/device/uevent}"
//! done
//! ```
//!
//! Default permissions on `/dev/hidraw*` are root-only. Either run with
//! sudo, or drop a udev rule like:
//!
//! ```text
//! # /etc/udev/rules.d/70-steam-deck.rules
//! KERNEL=="hidraw*", ATTRS{idVendor}=="28de", ATTRS{idProduct}=="1205", \
//!     MODE="0660", TAG+="uaccess"
//! ```
//!
//! While Steam (or `hid-steam` in gamepad mode) has the controller open,
//! the Deck firmware streams reports it can read. To get raw frames here
//! without fighting either, kill Steam and `echo` the device path to
//! `/sys/bus/hid/drivers/hid-steam/unbind`. The spike intentionally does
//! not automate that — the goal is to validate the protocol crate against
//! real bytes, not to manage device ownership.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!(
        "server-deck requires Linux (hidraw). Built on: {}",
        std::env::consts::OS
    );
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn main() {
    linux::run();
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, UdpSocket};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use deck_protocol::hid::{self, HidError, REPORT_LEN};
    use deck_protocol::wire::{self, Channel, Header, HEADER_LEN, INPUT_PACKET_LEN};
    use deck_protocol::ControllerState;

    const PRINT_EVERY: Duration = Duration::from_millis(50);

    #[derive(Default)]
    struct Stats {
        frames: u64,
        skipped: u64,
        sent: u64,
        send_errors: u64,
    }

    pub fn run() {
        let (path, target) = parse_args();

        let mut file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("open {path}: {e}");
                std::process::exit(1);
            }
        };
        let socket = target.map(open_socket);

        match target {
            Some(t) => eprintln!("reading {path} -> {t} (Ctrl-C to quit)"),
            None => eprintln!("reading {path} (read-only) (Ctrl-C to quit)"),
        }

        let mut stats = Stats::default();
        let mut hid_buf = [0_u8; REPORT_LEN];
        let mut wire_buf = [0_u8; INPUT_PACKET_LEN];
        let mut sequence: u32 = 0;
        let mut last_state: Option<ControllerState> = None;
        let mut last_print = Instant::now();
        let mut stdout = std::io::stdout();

        loop {
            if !read_one(&mut file, &mut hid_buf) {
                continue;
            }
            if let Some(state) = decode_or_skip(&hid_buf, &mut stats) {
                stats.frames += 1;
                if let (Some(s), Some(t)) = (socket.as_ref(), target) {
                    send_packet(s, t, &state, sequence, &mut wire_buf, &mut stats);
                    sequence = sequence.wrapping_add(1);
                }
                last_state = Some(state);
            }

            // Always tick the print, even if every frame so far was the
            // wrong report type — surfaces "skipped" climbing so you can
            // tell the device is alive but not in gamepad mode.
            if last_print.elapsed() >= PRINT_EVERY {
                last_print = Instant::now();
                print_status(&mut stdout, &stats, last_state.as_ref());
            }
        }
    }

    /// Returns `true` if a full report was read into `buf`. On short reads
    /// or recoverable errors prints and returns `false` so the caller can
    /// loop. Fatal errors call `process::exit`.
    fn read_one(file: &mut std::fs::File, buf: &mut [u8; REPORT_LEN]) -> bool {
        match file.read(buf) {
            Ok(n) if n == REPORT_LEN => true,
            Ok(n) => {
                eprintln!("\nshort read: {n} bytes (expected {REPORT_LEN})");
                false
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => false,
            Err(e) => {
                eprintln!("\nread: {e}");
                std::process::exit(1);
            }
        }
    }

    fn decode_or_skip(buf: &[u8; REPORT_LEN], stats: &mut Stats) -> Option<ControllerState> {
        match hid::parse_input_report(buf) {
            Ok(state) => Some(state),
            Err(HidError::UnexpectedReportType(_)) => {
                stats.skipped += 1;
                None
            }
            Err(e) => {
                eprintln!("\nparse: {e:?}");
                None
            }
        }
    }

    fn send_packet(
        sock: &UdpSocket,
        target: SocketAddr,
        state: &ControllerState,
        sequence: u32,
        wire_buf: &mut [u8; INPUT_PACKET_LEN],
        stats: &mut Stats,
    ) {
        let hdr = Header {
            channel: Channel::Input,
            sequence,
            timestamp_us: now_us_low32(),
        };
        if wire::encode_header(&hdr, &mut wire_buf[..HEADER_LEN]).is_err()
            || wire::encode_input(state, &mut wire_buf[HEADER_LEN..]).is_err()
        {
            stats.send_errors += 1;
            return;
        }
        match sock.send_to(wire_buf, target) {
            Ok(_) => stats.sent += 1,
            Err(_) => stats.send_errors += 1,
        }
    }

    fn parse_args() -> (String, Option<SocketAddr>) {
        let args: Vec<String> = std::env::args().collect();
        match args.as_slice() {
            [_, path] => (path.clone(), None),
            [_, path, target] => match target.parse() {
                Ok(addr) => (path.clone(), Some(addr)),
                Err(e) => {
                    eprintln!("bad target {target}: {e}");
                    std::process::exit(2);
                }
            },
            _ => {
                eprintln!("usage: server-deck <hidraw-device> [target-addr:port]");
                eprintln!("  send mode:      server-deck /dev/hidraw3 192.168.1.50:49152");
                eprintln!("  read-only mode: server-deck /dev/hidraw3");
                std::process::exit(2);
            }
        }
    }

    fn open_socket(target: SocketAddr) -> UdpSocket {
        let bind = if target.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" };
        match UdpSocket::bind(bind) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("bind {bind}: {e}");
                std::process::exit(1);
            }
        }
    }

    /// Low 32 bits of wall-clock microseconds since Unix epoch.
    /// Truncation is intentional — the wire format defines exactly that.
    #[allow(clippy::cast_possible_truncation)]
    fn now_us_low32() -> u32 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as u32)
            .unwrap_or(0)
    }

    fn print_status<W: Write>(out: &mut W, stats: &Stats, state: Option<&ControllerState>) {
        let _ = write!(
            out,
            "\x1b[2K\rframes={:>7} skipped={:>5} sent={:>7} senderr={:>4}",
            stats.frames, stats.skipped, stats.sent, stats.send_errors,
        );
        if let Some(state) = state {
            let _ = write!(
                out,
                " seq={:>10} \
                 L({:>+6},{:>+6}) R({:>+6},{:>+6}) \
                 LT={:>5} RT={:>5} \
                 accel({:>+6},{:>+6},{:>+6}) \
                 gyro({:>+6},{:>+6},{:>+6}) \
                 btns={:?}",
                state.sequence,
                state.left_stick.x,
                state.left_stick.y,
                state.right_stick.x,
                state.right_stick.y,
                state.left_trigger,
                state.right_trigger,
                state.accel.x,
                state.accel.y,
                state.accel.z,
                state.gyro.x,
                state.gyro.y,
                state.gyro.z,
                state.buttons,
            );
        } else {
            let _ = write!(out, " (no Deck-state frames yet)");
        }
        let _ = out.flush();
    }
}
