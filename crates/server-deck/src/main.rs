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

    use deck_protocol::auth::REPLAY_WINDOW_US;
    use deck_protocol::hid::{self, HidError, REPORT_LEN};
    use deck_protocol::wire::{self, Channel, Header, INPUT_PACKET_LEN, OUTPUT_BODY_LEN, OUTPUT_PACKET_LEN};
    use deck_protocol::{AuthKey, ControllerState};

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
        let auth_key = load_auth_key();

        let socket = target.map(open_socket);

        match target {
            Some(t) => eprintln!("reading {path} -> {t} (Ctrl-C to quit)"),
            None => eprintln!("reading {path} (read-only) (Ctrl-C to quit)"),
        }

        // Output thread: only spawned when we're in send mode. The Deck has
        // nothing useful to do with rumble traffic if there's no Windows
        // peer to drive it. Each thread opens its own hidraw fd so we can
        // re-open one side without disturbing the other.
        if target.is_some() {
            let path_for_output = path.clone();
            let key_for_output = auth_key.clone();
            std::thread::Builder::new()
                .name("deck-output".into())
                .spawn(move || run_output_loop(&path_for_output, key_for_output.as_ref()))
                .ok();
        }

        run_input_loop(&path, target, socket, auth_key.as_ref());
    }

    /// Load `NETWORK_DECK_KEY` from the environment. Empty / unset = no key
    /// (dev mode, plaintext). Bad hex makes us exit — the alternative
    /// (silently fall back to plaintext) is a footgun that has bitten every
    /// "either auth or no auth" config we've ever shipped.
    fn load_auth_key() -> Option<AuthKey> {
        match std::env::var("NETWORK_DECK_KEY") {
            Ok(s) if !s.trim().is_empty() => match AuthKey::from_hex(&s) {
                Ok(k) => {
                    eprintln!("auth: NETWORK_DECK_KEY loaded; secure mode");
                    Some(k)
                }
                Err(e) => {
                    eprintln!("auth: NETWORK_DECK_KEY parse error: {e:?}");
                    std::process::exit(2);
                }
            },
            _ => {
                eprintln!("auth: NETWORK_DECK_KEY unset; running in plaintext (dev mode)");
                None
            }
        }
    }

    /// Open hidraw read-write, falling back to read-only if RW fails (the
    /// caller may not be root). Returns `None` so the loop can decide
    /// whether to wait + retry — process exit on first failure makes
    /// systemd-supervised runs flap when the device is briefly absent
    /// (USB transient, kernel module reload).
    fn try_open_hidraw(path: &str) -> Option<File> {
        match OpenOptions::new().read(true).write(true).open(path) {
            Ok(f) => Some(f),
            Err(rw_err) => match File::open(path) {
                Ok(f) => {
                    eprintln!("\nopen RW {path}: {rw_err}; using read-only");
                    Some(f)
                }
                Err(ro_err) => {
                    eprintln!("\nopen {path}: {ro_err}");
                    None
                }
            },
        }
    }

    /// Wait until hidraw can be opened, with a small backoff. Used after a
    /// read failure surfaces a missing device (unplug, kernel reload).
    fn wait_for_hidraw(path: &str) -> File {
        let mut delay = Duration::from_millis(500);
        let max_delay = Duration::from_secs(5);
        loop {
            if let Some(f) = try_open_hidraw(path) {
                return f;
            }
            std::thread::sleep(delay);
            delay = (delay * 2).min(max_delay);
        }
    }

    fn run_input_loop(
        path: &str,
        target: Option<SocketAddr>,
        socket: Option<UdpSocket>,
        auth_key: Option<&AuthKey>,
    ) {
        let mut file = wait_for_hidraw(path);
        let mut stats = Stats::default();
        let mut hid_buf = [0_u8; REPORT_LEN];
        let mut wire_buf = [0_u8; INPUT_PACKET_LEN];
        let mut sequence: u32 = 0;
        let mut last_state: Option<ControllerState> = None;
        let mut last_print = Instant::now();
        let mut stdout = std::io::stdout();

        loop {
            match read_one(&mut file, &mut hid_buf) {
                ReadResult::Ok => {
                    if let Some(state) = decode_or_skip(&hid_buf, &mut stats) {
                        stats.frames += 1;
                        if let (Some(s), Some(t)) = (socket.as_ref(), target) {
                            send_packet(
                                s,
                                t,
                                &state,
                                sequence,
                                auth_key,
                                &mut wire_buf,
                                &mut stats,
                            );
                            sequence = sequence.wrapping_add(1);
                        }
                        last_state = Some(state);
                    }
                }
                ReadResult::ShortOrInterrupted => {}
                ReadResult::Reopen => {
                    eprintln!("\ninput: reopening hidraw {path}");
                    file = wait_for_hidraw(path);
                    eprintln!("\ninput: reopened hidraw {path}");
                }
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
    /// back into the Deck's hidraw via `HIDIOCSFEATURE`. Re-opens the
    /// hidraw fd if a write fails persistently — the same recovery story
    /// as the input loop, with its own state so neither blocks the other.
    fn run_output_loop(path: &str, auth_key: Option<&AuthKey>) {
        let bind = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), OUTPUT_LISTEN_PORT);
        let sock = match UdpSocket::bind(bind) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("\noutput: bind {bind}: {e}");
                return;
            }
        };
        eprintln!("output: listening on {bind} for rumble/haptic packets");

        let mut write_fd = wait_for_hidraw(path);
        let mut buf = [0_u8; OUTPUT_PACKET_LEN];
        // Feature reports go through HIDIOCSFEATURE with byte 0 reserved
        // for the report ID. Our HID descriptor declares no report IDs, so
        // byte 0 is always zero; bytes 1..65 carry the 64 payload bytes.
        let mut feature_buf = [0_u8; 1 + OUTPUT_BODY_LEN];
        let mut last_seq: Option<u32> = None;
        let mut received: u64 = 0;
        let mut applied: u64 = 0;
        let mut out_of_order: u64 = 0;
        let mut auth_drops: u64 = 0;
        let mut hidraw_errors: u64 = 0;
        // Threshold for treating HIDIOCSFEATURE failures as "the fd is
        // dead, reopen." A flaky single ioctl shouldn't tear down — but a
        // run of failures means the device went away.
        const REOPEN_AFTER: u64 = 32;
        let mut consecutive_errors: u64 = 0;
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
            let hdr = match wire::decode_output_packet(
                &buf,
                auth_key,
                now_us_low32(),
                REPLAY_WINDOW_US,
                &mut feature_buf[1..],
            ) {
                Ok(h) => h,
                Err(wire::WireError::AuthFailed | wire::WireError::Replay) => {
                    auth_drops += 1;
                    continue;
                }
                Err(_) => continue,
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

            // Report-id placeholder. Already zero from the array literal,
            // but reset explicitly so a future code change can't corrupt it.
            feature_buf[0] = 0;

            match hidiocsfeature(write_fd.as_raw_fd(), &feature_buf) {
                Ok(_) => {
                    applied += 1;
                    consecutive_errors = 0;
                }
                Err(_) => {
                    hidraw_errors += 1;
                    consecutive_errors += 1;
                    if consecutive_errors >= REOPEN_AFTER {
                        eprintln!("\noutput: reopening hidraw {path}");
                        write_fd = wait_for_hidraw(path);
                        consecutive_errors = 0;
                        eprintln!("\noutput: reopened hidraw {path}");
                    }
                }
            }

            if last_log.elapsed() >= Duration::from_secs(2) {
                last_log = Instant::now();
                eprintln!(
                    "\noutput: recv={received} applied={applied} ooo={out_of_order} \
                     auth_drops={auth_drops} hiderr={hidraw_errors} \
                     last_msg_id=0x{:02x}",
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

    enum ReadResult {
        Ok,
        ShortOrInterrupted,
        Reopen,
    }

    /// One non-fatal pass at reading from hidraw. Read errors that point at
    /// a dead fd (anything except EINTR) signal a re-open — the fd is
    /// likely stale (USB transient, kernel driver reload). EOF / short
    /// reads stay non-fatal; the controller occasionally interleaves
    /// shorter status reports with the 64-byte input ones.
    fn read_one(file: &mut File, buf: &mut [u8; REPORT_LEN]) -> ReadResult {
        match file.read(buf) {
            Ok(n) if n == REPORT_LEN => ReadResult::Ok,
            Ok(n) => {
                eprintln!("\nshort read: {n} bytes (expected {REPORT_LEN})");
                ReadResult::ShortOrInterrupted
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                ReadResult::ShortOrInterrupted
            }
            Err(e) => {
                eprintln!("\nread: {e}");
                ReadResult::Reopen
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
        auth_key: Option<&AuthKey>,
        wire_buf: &mut [u8; INPUT_PACKET_LEN],
        stats: &mut Stats,
    ) {
        let hdr = Header {
            channel: Channel::Input,
            flags: 0,
            sequence,
            timestamp_us: now_us_low32(),
        };
        if wire::encode_input_packet(&hdr, state, auth_key, wire_buf).is_err() {
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
