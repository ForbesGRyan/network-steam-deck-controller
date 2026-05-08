//! Steam Deck server: open a hidraw device, parse input reports, send them
//! as wire packets to the Windows client. Listens on `OUTPUT_LISTEN_PORT`
//! for OUTPUT-channel packets (rumble/haptic) and writes them back into the
//! controller as HID feature reports. Peer discovery uses the `discovery`
//! crate: the binary broadcasts signed beacons and tracks the live peer
//! address without a hard-coded IP.
//!
//! Usage:
//! ```text
//! server-deck <hidraw-device>                         # normal mode
//! server-deck pair <hidraw-device>                    # one-shot pair
//! server-deck <hidraw-device> --state-dir <path>      # override state dir
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

mod connection;
mod sysfs;

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
    use super::sysfs;
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
    use std::os::unix::io::AsRawFd;
    use std::path::PathBuf;
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

    enum Mode { Run, Pair }

    struct ParsedArgs {
        mode: Mode,
        hidraw_path: String,
        state_dir: PathBuf,
    }

    fn parse_args() -> ParsedArgs {
        let mut args = std::env::args().skip(1);
        let mut mode = Mode::Run;
        let mut hidraw_path: Option<String> = None;
        let mut state_dir_override: Option<PathBuf> = None;
        while let Some(a) = args.next() {
            match a.as_str() {
                "pair" => mode = Mode::Pair,
                "--state-dir" => {
                    state_dir_override = args.next().map(PathBuf::from).or_else(|| {
                        eprintln!("--state-dir requires a value");
                        std::process::exit(2);
                    });
                }
                other => {
                    if hidraw_path.is_none() {
                        hidraw_path = Some(other.to_owned());
                    } else {
                        eprintln!("unexpected argument: {other}");
                        std::process::exit(2);
                    }
                }
            }
        }
        let hidraw_path = hidraw_path.unwrap_or_else(|| {
            eprintln!("usage: server-deck [pair] <hidraw-device> [--state-dir <path>]");
            std::process::exit(2);
        });
        let state_dir = state_dir_override.unwrap_or_else(|| {
            discovery::state_dir::default_state_dir().unwrap_or_else(|e| {
                eprintln!("cannot resolve state dir: {e:?}");
                std::process::exit(1);
            })
        });
        ParsedArgs { mode, hidraw_path, state_dir }
    }

    pub fn run() {
        let args = parse_args();
        let identity = std::sync::Arc::new(
            discovery::identity::load_or_generate(&args.state_dir).unwrap_or_else(|e| {
                eprintln!("identity load: {e:?}");
                std::process::exit(1);
            }),
        );

        if matches!(args.mode, Mode::Pair) {
            run_pair_mode(&identity, &args.state_dir);
            return;
        }

        let trusted = match discovery::trust::load(&args.state_dir) {
            Ok(Some(p)) => std::sync::Arc::new(p),
            Ok(None) => {
                eprintln!(
                    "no trusted peer; run `server-deck pair {}` to pair first",
                    args.hidraw_path,
                );
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("trust load: {e:?}");
                std::process::exit(1);
            }
        };

        let bound = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, OUTPUT_LISTEN_PORT)) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("bind 0.0.0.0:{OUTPUT_LISTEN_PORT}: {e}");
                std::process::exit(1);
            }
        };

        let beacon = std::sync::Arc::new(
            discovery::Beacon::new(
                identity.clone(),
                trusted.clone(),
                SocketAddr::new(Ipv4Addr::BROADCAST.into(), OUTPUT_LISTEN_PORT),
                hostname(),
                OUTPUT_LISTEN_PORT,
            )
            .unwrap_or_else(|e| {
                eprintln!("beacon init: {e:?}");
                std::process::exit(1);
            }),
        );
        discovery::beacon::spawn_broadcast(beacon.clone());
        let auth_key = Some(deck_protocol::AuthKey::from_bytes(beacon.session_key()));

        eprintln!(
            "reading {path} -> peer {name} ({fpr}) (Ctrl-C to quit)",
            path = args.hidraw_path,
            name = trusted.name,
            fpr = identity.fingerprint_str(),
        );

        // Output thread: dequeues output packets from `bound`. Pass the
        // beacon in too so its handle_packet() sees beacon traffic on the
        // shared port.
        let path_for_output = args.hidraw_path.clone();
        let beacon_for_output = beacon.clone();
        let bound_for_output = bound.try_clone().expect("clone bound socket");
        let auth_key_for_output = auth_key.clone();
        std::thread::Builder::new()
            .name("deck-output".into())
            .spawn(move || run_output_loop(
                &path_for_output,
                auth_key_for_output.as_ref(),
                bound_for_output,
                beacon_for_output,
            ))
            .ok();

        run_input_loop(&args.hidraw_path, beacon, auth_key.as_ref());
    }

    fn hostname() -> String {
        std::env::var("HOSTNAME").unwrap_or_else(|_| "deck".to_owned())
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
        beacon: std::sync::Arc<discovery::Beacon>,
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
        // Send socket: ephemeral; data plane sends to the live peer addr.
        let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("bind ephemeral send sock");

        loop {
            match read_one(&mut file, &mut hid_buf) {
                ReadResult::Ok => {
                    if let Some(state) = decode_or_skip(&hid_buf, &mut stats) {
                        stats.frames += 1;
                        if let Some(target) = beacon.current_peer() {
                            send_packet(&sock, target, &state, sequence, auth_key, &mut wire_buf, &mut stats);
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
                print_status(&mut stdout, &stats, last_state.as_ref(), beacon.current_peer_with_age());
            }
        }
    }

    /// Listen on `OUTPUT_LISTEN_PORT` for OUTPUT-channel wire packets from
    /// `client-win`, decode the 64-byte feature-report body, and write it
    /// back into the Deck's hidraw via `HIDIOCSFEATURE`. Re-opens the
    /// hidraw fd if a write fails persistently — the same recovery story
    /// as the input loop, with its own state so neither blocks the other.
    /// Also demuxes discovery beacon packets on the same port.
    fn run_output_loop(
        path: &str,
        auth_key: Option<&AuthKey>,
        sock: UdpSocket,
        beacon: std::sync::Arc<discovery::Beacon>,
    ) {
        eprintln!("output: listening on 0.0.0.0:{OUTPUT_LISTEN_PORT} for rumble/haptic packets");

        let mut write_fd = wait_for_hidraw(path);
        const BUF_LEN: usize = if OUTPUT_PACKET_LEN > discovery::packet::PACKET_LEN {
            OUTPUT_PACKET_LEN
        } else {
            discovery::packet::PACKET_LEN
        };
        let mut buf = [0_u8; BUF_LEN];
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
            let (n, src) = match sock.recv_from(&mut buf) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("\noutput: recv: {e}");
                    continue;
                }
            };

            // Demux beacon vs data.
            if n >= 4 && buf[0..4] == discovery::BEACON_MAGIC {
                beacon.handle_packet(src, &buf[..n]);
                continue;
            }

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

    /// Low 32 bits of wall-clock microseconds since Unix epoch.
    /// Truncation is intentional — the wire format defines exactly that.
    #[allow(clippy::cast_possible_truncation)]
    fn now_us_low32() -> u32 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as u32)
            .unwrap_or(0)
    }

    fn print_status<W: Write>(
        out: &mut W,
        stats: &Stats,
        state: Option<&ControllerState>,
        peer: Option<(SocketAddr, Duration)>,
    ) {
        let peer_str = match peer {
            None => "peer: searching".to_owned(),
            Some((a, age)) if age > discovery::beacon::STALE_AFTER => {
                format!("peer: {a} age {:.1}s STALE", age.as_secs_f64())
            }
            Some((a, age)) => format!("peer: {a} age {:.1}s", age.as_secs_f64()),
        };
        let _ = write!(
            out,
            "\x1b[2K\rframes={:>7} skipped={:>5} sent={:>7} senderr={:>4} {peer_str}",
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

    fn run_pair_mode(identity: &std::sync::Arc<discovery::Identity>, state_dir: &std::path::Path) {
        let sock = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, OUTPUT_LISTEN_PORT)) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("bind for pair: {e}");
                std::process::exit(1);
            }
        };
        sock.set_broadcast(true).ok();
        let cfg = discovery::pair::PairConfig {
            identity: identity.clone(),
            recv_sock: sock,
            unicast_target: SocketAddr::new(Ipv4Addr::BROADCAST.into(), OUTPUT_LISTEN_PORT),
            self_name: hostname(),
            state_dir: state_dir.to_path_buf(),
            timeout: Duration::from_secs(120),
        };
        eprintln!("pairing — fingerprint {}; waiting up to 120 s", identity.fingerprint_str());
        let mut stdin = std::io::stdin();
        let mut stderr = std::io::stderr();
        match discovery::pair::run_pair(&cfg, &mut stdin, &mut stderr) {
            discovery::pair::PairOutcome::Paired(p) => {
                eprintln!("paired with {} (fingerprint stored)", p.name);
            }
            other => {
                eprintln!("pair did not complete: {other:?}");
                std::process::exit(1);
            }
        }
    }
}
