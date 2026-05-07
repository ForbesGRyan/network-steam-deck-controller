//! Spike: open a Deck hidraw device, parse input reports, optionally send
//! them as wire packets to the Windows client. When run with a target
//! address, also listens on `DEFAULT_PORT` for OUTPUT-channel packets and
//! writes them straight back into the controller as feature reports —
//! that's the haptic / rumble path.
//!
//! Usage:
//! ```text
//! server-deck <hidraw-device>                         # read-only, print only
//! server-deck <hidraw-device> <target-addr:port>      # full duplex: read +
//!                                                     # send via UDP, also
//!                                                     # accept OUTPUT pkts
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
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
    use std::os::unix::io::AsRawFd;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use deck_protocol::hid::{self, HidError, REPORT_LEN};
    use deck_protocol::wire::{
        self, Channel, Header, HEADER_LEN, INPUT_PACKET_LEN, OUTPUT_BODY_LEN, OUTPUT_PACKET_LEN,
    };
    use deck_protocol::ControllerState;

    const PRINT_EVERY: Duration = Duration::from_millis(50);
    /// UDP port the Windows client `client-win` ships output reports to.
    /// Must match `DEFAULT_PORT` there.
    const OUTPUT_LISTEN_PORT: u16 = 49152;

    #[derive(Default)]
    struct Stats {
        frames: u64,
        skipped: u64,
        sent: u64,
        send_errors: u64,
    }

    pub fn run() {
        let (path, target) = parse_args();

        let file = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(f) => f,
            Err(e) => {
                // Fall back to read-only — feature-report writes will fail
                // but the input path still works for diagnostic runs without
                // root. Log so the failure mode is visible.
                eprintln!("open RW {path}: {e}; retrying read-only");
                match File::open(&path) {
                    Ok(f) => f,
                    Err(e2) => {
                        eprintln!("open RO {path}: {e2}");
                        std::process::exit(1);
                    }
                }
            }
        };
        let socket = target.map(open_socket);

        match target {
            Some(t) => eprintln!("reading {path} -> {t} (Ctrl-C to quit)"),
            None => eprintln!("reading {path} (read-only) (Ctrl-C to quit)"),
        }

        // Output thread: only spawned when we're in send mode. The Deck has
        // nothing useful to do with rumble traffic if there's no Windows
        // peer to drive it.
        if target.is_some() {
            match file.try_clone() {
                Ok(write_fd) => {
                    std::thread::Builder::new()
                        .name("deck-output".into())
                        .spawn(move || run_output_loop(write_fd))
                        .ok();
                }
                Err(e) => eprintln!("hidraw try_clone for output thread: {e}"),
            }
        }

        run_input_loop(file, target, socket);
    }

    fn run_input_loop(mut file: File, target: Option<SocketAddr>, socket: Option<UdpSocket>) {
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

    /// Listen on `OUTPUT_LISTEN_PORT` for OUTPUT-channel wire packets from
    /// `client-win`, decode the 64-byte feature-report body, and write it
    /// back into the Deck's hidraw via `HIDIOCSFEATURE`.
    fn run_output_loop(write_fd: File) {
        let bind = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), OUTPUT_LISTEN_PORT);
        let sock = match UdpSocket::bind(bind) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("\noutput: bind {bind}: {e}");
                return;
            }
        };
        eprintln!("output: listening on {bind} for rumble/haptic packets");

        let mut buf = [0_u8; OUTPUT_PACKET_LEN];
        // Feature reports go through HIDIOCSFEATURE with byte 0 reserved
        // for the report ID. Our HID descriptor declares no report IDs, so
        // byte 0 is always zero; bytes 1..65 carry the 64 payload bytes.
        let mut feature_buf = [0_u8; 1 + OUTPUT_BODY_LEN];
        let mut last_seq: Option<u32> = None;
        let mut received: u64 = 0;
        let mut applied: u64 = 0;
        let mut out_of_order: u64 = 0;
        let mut hidraw_errors: u64 = 0;
        let mut last_log = Instant::now();

        loop {
            let n = match sock.recv_from(&mut buf) {
                Ok((n, _src)) => n,
                Err(e) => {
                    eprintln!("\noutput: recv: {e}");
                    continue;
                }
            };
            received += 1;
            if n != OUTPUT_PACKET_LEN {
                continue;
            }
            let Ok(hdr) = wire::decode_header(&buf[..HEADER_LEN]) else {
                continue;
            };
            if hdr.channel != Channel::Output {
                continue;
            }
            // Older Steam emits the same haptic command back-to-back; we
            // just drop equal-or-older sequences. Out-of-order isn't fatal,
            // just slightly stale, so we bump a counter and apply anyway —
            // the alternative (skipping) makes brief Wi-Fi reorder events
            // mute the rumble for whole-game stretches.
            match last_seq {
                Some(prev) if hdr.sequence < prev => out_of_order += 1,
                _ => {}
            }
            last_seq = Some(hdr.sequence);

            if wire::decode_output(&buf[HEADER_LEN..], &mut feature_buf[1..]).is_err() {
                continue;
            }
            // Report-id placeholder. Already zero from the array literal,
            // but reset explicitly so a future code change can't corrupt it.
            feature_buf[0] = 0;

            match hidiocsfeature(write_fd.as_raw_fd(), &feature_buf) {
                Ok(_) => applied += 1,
                Err(_) => hidraw_errors += 1,
            }

            if last_log.elapsed() >= Duration::from_secs(2) {
                last_log = Instant::now();
                eprintln!(
                    "\noutput: recv={received} applied={applied} ooo={out_of_order} \
                     hiderr={hidraw_errors} last_msg_id=0x{:02x}",
                    feature_buf[1],
                );
            }
        }
    }

    /// `ioctl(fd, HIDIOCSFEATURE(buf.len()), buf.as_ptr())`. Sends a feature
    /// report to a hidraw device.
    fn hidiocsfeature(fd: libc::c_int, buf: &[u8]) -> std::io::Result<libc::c_int> {
        // _IOC encoding: dir(2) | size(14) | type(8) | nr(8). HIDIOCSFEATURE
        // is _IOC(_IOC_WRITE | _IOC_READ, 'H', 0x06, len). The _IOC_READ
        // half captures the kernel's ability to return data on this op (set
        // feature returns the bytes actually written).
        const IOC_WRITE: libc::c_ulong = 1;
        const IOC_READ: libc::c_ulong = 2;
        const IOC_NRBITS: libc::c_ulong = 8;
        const IOC_TYPEBITS: libc::c_ulong = 8;
        const IOC_SIZEBITS: libc::c_ulong = 14;
        const IOC_NRSHIFT: libc::c_ulong = 0;
        const IOC_TYPESHIFT: libc::c_ulong = IOC_NRSHIFT + IOC_NRBITS;
        const IOC_SIZESHIFT: libc::c_ulong = IOC_TYPESHIFT + IOC_TYPEBITS;
        const IOC_DIRSHIFT: libc::c_ulong = IOC_SIZESHIFT + IOC_SIZEBITS;

        let dir = IOC_WRITE | IOC_READ;
        let typ: libc::c_ulong = b'H' as libc::c_ulong;
        let nr: libc::c_ulong = 0x06;
        let size = buf.len() as libc::c_ulong;
        let request = (dir << IOC_DIRSHIFT)
            | (size << IOC_SIZESHIFT)
            | (typ << IOC_TYPESHIFT)
            | (nr << IOC_NRSHIFT);

        // SAFETY: fd is borrowed for the call only; buf is a unique
        // borrowed slice with the asserted length.
        let rc = unsafe { libc::ioctl(fd, request, buf.as_ptr()) };
        if rc < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(rc)
        }
    }

    /// Returns `true` if a full report was read into `buf`. On short reads
    /// or recoverable errors prints and returns `false` so the caller can
    /// loop. Fatal errors call `process::exit`.
    fn read_one(file: &mut File, buf: &mut [u8; REPORT_LEN]) -> bool {
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
                eprintln!("  full-duplex:    server-deck /dev/hidraw3 192.168.1.50:49152");
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
