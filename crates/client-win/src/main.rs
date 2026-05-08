//! Windows-side service.
//!
//! Bind UDP, receive [`deck_protocol::wire`] input packets, decode to
//! [`deck_protocol::ControllerState`], encode to a Deck HID input report,
//! and (when the driver is installed) push the report into the kernel via
//! `IOCTL_DECK_PUSH_INPUT_REPORT`. A background thread parks on
//! `IOCTL_DECK_PEND_OUTPUT_REPORT` and reports any rumble/haptic feedback
//! the host writes to the virtual device.
//!
//! When the driver is not yet installed, runs in listen-only mode (the
//! HID-encode path still executes as a self-test of the protocol crate).
//!
//! Usage:
//!   client-win                   - normal mode (requires prior pairing)
//!   client-win pair              - one-shot pairing flow
//!   client-win --state-dir <p>   - override state directory
//!   client-win --test            - drive the IOCTL with a canned state
//!                                  pattern (alternates neutral / A
//!                                  pressed each second). Useful for
//!                                  end-to-end driver bring-up without a
//!                                  real Deck server attached.
//!   client-win --replay <path>   - replay a hidraw capture file
//!                                  recorded on a Deck (e.g.
//!                                  `cat /dev/hidrawN > deck.bin`).
//!                                  Reads 60-byte reports, pads to 64,
//!                                  pushes via IOCTL at ~250 Hz. Loops
//!                                  forever.

use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use deck_protocol::auth::REPLAY_WINDOW_US;
use deck_protocol::hid;
use deck_protocol::wire::{self, Channel, Header, INPUT_PACKET_LEN, OUTPUT_PACKET_LEN};
use deck_protocol::{AuthKey, Buttons, ControllerState};

#[cfg(windows)]
mod attach;
#[cfg(windows)]
mod autostart;
#[cfg(windows)]
mod driver;
#[cfg(windows)]
mod tray;
#[cfg(windows)]
mod usbip_cli;

const DEFAULT_PORT: u16 = 49152;
const PRINT_EVERY: Duration = Duration::from_millis(50);
const RECV_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Default)]
struct Stats {
    packets: u64,
    dropped: u64,
    wire_errors: u64,
    auth_drops: u64,
    pushed: u64,
    push_errors: u64,
    last_seq: Option<u32>,
}

enum Mode {
    Run,
    Pair,
    Test,
    Replay(String),
}

struct ParsedArgs {
    mode: Mode,
    state_dir: std::path::PathBuf,
}

fn parse_args() -> ParsedArgs {
    let mut args = std::env::args().skip(1);
    let mut mode = Mode::Run;
    let mut state_dir_override: Option<std::path::PathBuf> = None;

    while let Some(a) = args.next() {
        match a.as_str() {
            "--test" => mode = Mode::Test,
            "--replay" => {
                let Some(path) = args.next() else {
                    eprintln!("usage: client-win --replay <path-to-hidraw-capture>");
                    std::process::exit(2);
                };
                mode = Mode::Replay(path);
            }
            "pair" => mode = Mode::Pair,
            "--state-dir" => {
                state_dir_override = args.next().map(std::path::PathBuf::from).or_else(|| {
                    eprintln!("--state-dir requires a value");
                    std::process::exit(2);
                });
            }
            other => {
                eprintln!("unexpected argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let state_dir = state_dir_override.unwrap_or_else(|| {
        discovery::state_dir::default_state_dir().unwrap_or_else(|e| {
            eprintln!("cannot resolve state dir: {e:?}");
            std::process::exit(1);
        })
    });

    ParsedArgs { mode, state_dir }
}

fn main() {
    let args = parse_args();

    // --test and --replay skip identity/trust/network entirely.
    match args.mode {
        Mode::Test => {
            run_test_mode();
            return;
        }
        Mode::Replay(ref path) => {
            run_replay_mode(path);
            return;
        }
        _ => {}
    }

    let identity = Arc::new(
        discovery::identity::load_or_generate(&args.state_dir).unwrap_or_else(|e| {
            eprintln!("identity load: {e:?}");
            std::process::exit(1);
        }),
    );

    if matches!(args.mode, Mode::Pair) {
        run_pair_mode(&identity, &args.state_dir);
        return;
    }

    // Normal mode: require a trusted peer.
    let trusted = match discovery::trust::load(&args.state_dir) {
        Ok(Some(p)) => Arc::new(p),
        Ok(None) => {
            eprintln!("no trusted peer; run `client-win pair` to pair first");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("trust load: {e:?}");
            std::process::exit(1);
        }
    };

    let bind = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), DEFAULT_PORT);
    let sock = match UdpSocket::bind(bind) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("bind {bind}: {e}");
            std::process::exit(1);
        }
    };
    let _ = sock.set_read_timeout(Some(RECV_TIMEOUT));

    let beacon = Arc::new(
        discovery::Beacon::new(
            identity.clone(),
            trusted.clone(),
            SocketAddr::new(Ipv4Addr::BROADCAST.into(), DEFAULT_PORT),
            hostname(),
            DEFAULT_PORT,
        )
        .unwrap_or_else(|e| {
            eprintln!("beacon init: {e:?}");
            std::process::exit(1);
        }),
    );
    discovery::beacon::spawn_broadcast(beacon.clone());
    let auth_key = Some(AuthKey::from_bytes(beacon.session_key()));

    eprintln!(
        "client-win listening on {bind} (Ctrl-C to quit) — peer {} fingerprint {}",
        trusted.name,
        identity.fingerprint_str(),
    );

    // Latest input-packet source address. The output (rumble) thread reads
    // this so it can ship feedback back to whichever Deck most recently
    // reached us — no separate config flag, the pairing is implicit in the
    // input flow direction.
    let latest_src: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));

    #[cfg(windows)]
    let driver = spawn_driver_with_pend_thread(&sock, &latest_src, auth_key.clone());
    #[cfg(not(windows))]
    eprintln!("driver IPC: requires Windows; running listen-only");

    let mut stats = Stats::default();
    // Buffer sized to accommodate either an INPUT_PACKET_LEN data packet or a
    // full beacon PACKET_LEN so the demux check can read the magic bytes.
    let buf_cap = INPUT_PACKET_LEN.max(discovery::packet::PACKET_LEN);
    let mut buf = vec![0_u8; buf_cap];
    // Driver IOCTL expects exactly DECK_INPUT_REPORT_SIZE (64) bytes; the
    // protocol crate's encode produces hid::REPORT_LEN (60) — pad with the
    // zero tail so the trailing 4 bytes match what real Deck firmware sends.
    let mut hid_buf = [0_u8; driver::INPUT_REPORT_SIZE];
    let mut last_print = Instant::now();
    let mut stdout = std::io::stdout();
    let mut diag = RecvDiag::new(bind);

    loop {
        let Some((n, src)) = diag.recv(&sock, &mut buf, &latest_src) else {
            continue;
        };

        // Beacon demux: if the packet starts with BEACON_MAGIC, hand it to
        // the beacon and continue — it's not a data packet.
        if n >= 4 && buf[0..4] == discovery::BEACON_MAGIC {
            beacon.handle_packet(src, &buf[..n]);
            continue;
        }

        let Some((hdr, state)) = parse_packet(&buf, n, auth_key.as_ref(), &mut stats) else {
            continue;
        };

        if hid::encode_input_report(&state, &mut hid_buf[..hid::REPORT_LEN]).is_err() {
            stats.wire_errors += 1;
            continue;
        }

        #[cfg(windows)]
        match driver.push_input(&hid_buf) {
            Ok(()) => stats.pushed += 1,
            Err(_) => stats.push_errors += 1,
        }

        stats.packets += 1;
        if last_print.elapsed() >= PRINT_EVERY {
            last_print = Instant::now();
            print_status(&mut stdout, &stats, &hdr, &state, beacon.current_peer_with_age());
        }
    }
}

/// Wraps `UdpSocket::recv_from` with the diagnostic counters/heartbeats the
/// listen loop wants — the silent-recv UX bug came back to bite once before,
/// so this consolidates the bookkeeping and updates `latest_src` while it's
/// at it (the rumble thread reads that to route output back).
struct RecvDiag {
    bind: SocketAddr,
    last_recv_log: Instant,
    idle_since: Instant,
    idle_logged: bool,
    total_received: u64,
    total_recv_bytes: u64,
    first_pkt_logged: bool,
}

impl RecvDiag {
    fn new(bind: SocketAddr) -> Self {
        let now = Instant::now();
        Self {
            bind,
            last_recv_log: now,
            idle_since: now,
            idle_logged: false,
            total_received: 0,
            total_recv_bytes: 0,
            first_pkt_logged: false,
        }
    }

    fn recv(
        &mut self,
        sock: &UdpSocket,
        buf: &mut [u8],
        latest_src: &Mutex<Option<SocketAddr>>,
    ) -> Option<(usize, SocketAddr)> {
        match sock.recv_from(buf) {
            Ok((n, src)) => {
                self.total_received += 1;
                self.total_recv_bytes += n as u64;
                self.idle_since = Instant::now();
                self.idle_logged = false;
                if let Ok(mut s) = latest_src.lock() {
                    *s = Some(src);
                }
                if !self.first_pkt_logged {
                    eprintln!("\nfirst UDP packet: {n} bytes from {src}");
                    self.first_pkt_logged = true;
                }
                if self.last_recv_log.elapsed() >= Duration::from_secs(2) {
                    self.last_recv_log = Instant::now();
                    eprintln!(
                        "\nrecv heartbeat: total_pkts={} bytes={} last_src={src} last_size={n}",
                        self.total_received, self.total_recv_bytes,
                    );
                }
                Some((n, src))
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                if !self.idle_logged && self.idle_since.elapsed() >= Duration::from_secs(5) {
                    eprintln!(
                        "\nidle: no UDP for 5 s. total_pkts={} (sock {})",
                        self.total_received, self.bind,
                    );
                    self.idle_logged = true;
                }
                None
            }
            Err(e) => {
                eprintln!("\nrecv: {e}");
                None
            }
        }
    }
}

fn parse_packet(
    buf: &[u8],
    n: usize,
    auth_key: Option<&AuthKey>,
    stats: &mut Stats,
) -> Option<(Header, ControllerState)> {
    if n != INPUT_PACKET_LEN {
        stats.wire_errors += 1;
        return None;
    }
    let (hdr, state) =
        match wire::decode_input_packet(&buf[..INPUT_PACKET_LEN], auth_key, now_us_low32(), REPLAY_WINDOW_US) {
            Ok(v) => v,
            Err(wire::WireError::AuthFailed | wire::WireError::Replay) => {
                stats.auth_drops += 1;
                return None;
            }
            Err(_) => {
                stats.wire_errors += 1;
                return None;
            }
        };
    if hdr.channel != Channel::Input {
        return None;
    }

    match stats.last_seq {
        Some(prev) if hdr.sequence <= prev => return None,
        Some(prev) => stats.dropped += u64::from(hdr.sequence - prev - 1),
        None => {}
    }
    stats.last_seq = Some(hdr.sequence);

    Some((hdr, state))
}

fn print_status<W: Write>(
    out: &mut W,
    stats: &Stats,
    hdr: &Header,
    state: &ControllerState,
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
        "\x1b[2K\r{peer_str} pkts={:>7} drop={:>5} err={:>4} push={:>7} perr={:>4} \
         seq_hdr={:>10} \
         L({:>+6},{:>+6}) R({:>+6},{:>+6}) \
         LT={:>5} RT={:>5} \
         accel({:>+6},{:>+6},{:>+6}) \
         gyro({:>+6},{:>+6},{:>+6}) \
         btns={:?}",
        stats.packets,
        stats.dropped,
        stats.wire_errors,
        stats.pushed,
        stats.push_errors,
        hdr.sequence,
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
    let _ = out.flush();
}

#[cfg(windows)]
fn spawn_driver_with_pend_thread(
    sock: &UdpSocket,
    latest_src: &Arc<Mutex<Option<SocketAddr>>>,
    auth_key: Option<AuthKey>,
) -> Arc<driver::DriverHolder> {
    let holder = Arc::new(driver::DriverHolder::try_init());
    let pend_handle = holder.clone();
    // Clone the bound socket so the output thread can send_to the Deck on
    // the same port we listen on. Replies come back via the OS source
    // address; we infer the Deck's listen port from the wire-protocol
    // convention (DEFAULT_PORT) rather than echoing to src.port — the
    // Deck's outgoing source port is ephemeral.
    let send_sock = match sock.try_clone() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("socket clone for output thread: {e}");
            return holder;
        }
    };
    let src_handle = latest_src.clone();
    std::thread::Builder::new()
        .name("deck-pend-output".into())
        .spawn(move || pend_output_loop(&pend_handle, &send_sock, &src_handle, auth_key.as_ref()))
        .ok();
    holder
}

/// Drive the kernel-side IOCTL with a synthetic input pattern so the full
/// chain can be exercised before the Deck server is wired up. Toggles the
/// `A` button each second; everything else stays at idle.
#[cfg(windows)]
fn run_test_mode() {
    let driver = driver::DriverHolder::try_init();
    if !driver.is_open() {
        eprintln!("test mode will keep retrying open every 5 s; install the driver to begin");
    }
    eprintln!("driver: test mode — synthesizing input @ ~250 Hz");

    let mut state = ControllerState::default();
    let mut hid_buf = [0_u8; driver::INPUT_REPORT_SIZE];
    let push_period = Duration::from_millis(4);
    let toggle_period = Duration::from_secs(1);

    let mut sequence: u32 = 0;
    let mut a_pressed = false;
    let mut last_toggle = Instant::now();
    let mut last_print = Instant::now();
    let mut pushed: u64 = 0;
    let mut errors: u64 = 0;
    let mut stdout = std::io::stdout();

    loop {
        if last_toggle.elapsed() >= toggle_period {
            last_toggle = Instant::now();
            a_pressed = !a_pressed;
        }
        state.sequence = sequence;
        sequence = sequence.wrapping_add(1);
        state.buttons = if a_pressed { Buttons::A } else { Buttons::empty() };

        if hid::encode_input_report(&state, &mut hid_buf[..hid::REPORT_LEN]).is_err() {
            errors += 1;
        } else {
            match driver.push_input(&hid_buf) {
                Ok(()) => pushed += 1,
                Err(_) => errors += 1,
            }
        }

        if last_print.elapsed() >= PRINT_EVERY {
            last_print = Instant::now();
            let _ = write!(
                stdout,
                "\x1b[2K\rpushed={pushed:>8} err={errors:>4} \
                 a={} seq={}",
                if a_pressed { '1' } else { '0' },
                sequence,
            );
            let _ = stdout.flush();
        }

        std::thread::sleep(push_period);
    }
}

#[cfg(not(windows))]
fn run_test_mode() {
    eprintln!("--test mode requires Windows (driver IPC); exiting");
    std::process::exit(1);
}

/// Replay a hidraw capture file (raw concatenated 60-byte Deck HID input
/// reports — what `cat /dev/hidrawN` produces on the Deck) through the
/// driver IOCTL. Each report is padded to 64 bytes, then pushed at ~250 Hz
/// to match real-Deck cadence. Loops forever so a few seconds of capture
/// makes a long-lived test session.
#[cfg(windows)]
fn run_replay_mode(path: &str) {
    let driver = driver::DriverHolder::try_init();
    if !driver.is_open() {
        eprintln!("replay mode will keep retrying open every 5 s; install the driver to begin");
    }
    eprintln!("driver: replay mode — {path} @ ~250 Hz, looping");

    let raw = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("read {path}: {e}");
            std::process::exit(1);
        }
    };
    if raw.len() < hid::REPORT_LEN {
        eprintln!(
            "{path}: only {} bytes; need at least {} for one Deck report",
            raw.len(),
            hid::REPORT_LEN,
        );
        std::process::exit(1);
    }
    let frames = raw.len() / hid::REPORT_LEN;
    eprintln!(
        "loaded {} bytes ({frames} reports of {} bytes each)",
        raw.len(),
        hid::REPORT_LEN,
    );

    let mut hid_buf = [0_u8; driver::INPUT_REPORT_SIZE];
    let push_period = Duration::from_millis(4);
    let mut last_print = Instant::now();
    let mut pushed: u64 = 0;
    let mut errors: u64 = 0;
    let mut stdout = std::io::stdout();

    let mut skipped: u64 = 0;
    loop {
        for chunk in raw.chunks_exact(hid::REPORT_LEN) {
            // Skip non-deck-state reports (status / wireless / debug) that
            // the firmware interleaves into the hidraw stream. Only frames
            // that parse to a ControllerState get pushed; otherwise Steam
            // sees garbage button bytes and the controller appears stuck.
            let parsed: [u8; hid::REPORT_LEN] = if let Ok(arr) = chunk.try_into() {
                arr
            } else {
                skipped += 1;
                continue;
            };
            if hid::parse_input_report(&parsed).is_err() {
                skipped += 1;
                continue;
            }

            // Padding tail stays zero — matches what the real Deck firmware
            // sends on its 64-byte interrupt-IN endpoint.
            hid_buf[..hid::REPORT_LEN].copy_from_slice(chunk);
            for b in &mut hid_buf[hid::REPORT_LEN..] { *b = 0; }

            match driver.push_input(&hid_buf) {
                Ok(()) => pushed += 1,
                Err(_) => errors += 1,
            }

            if last_print.elapsed() >= PRINT_EVERY {
                last_print = Instant::now();
                let _ = write!(
                    stdout,
                    "\x1b[2K\rpushed={pushed:>8} skipped={skipped:>6} err={errors:>4} \
                     frames_in_file={frames}",
                );
                let _ = stdout.flush();
            }

            std::thread::sleep(push_period);
        }
    }
}

#[cfg(not(windows))]
fn run_replay_mode(_path: &str) {
    eprintln!("--replay mode requires Windows (driver IPC); exiting");
    std::process::exit(1);
}

/// Park on the driver's output IOCTL; whenever Steam writes a haptic/rumble
/// feature report, ship it back over UDP to whichever address last sent us
/// input. Best-effort: we do not retransmit, do not buffer. A dropped
/// rumble frame is imperceptible at Steam's ~250 Hz cadence.
#[cfg(windows)]
fn pend_output_loop(
    driver: &driver::DriverHolder,
    send_sock: &UdpSocket,
    latest_src: &Arc<Mutex<Option<SocketAddr>>>,
    auth_key: Option<&AuthKey>,
) {
    let mut buf = [0_u8; driver::OUTPUT_REPORT_SIZE];
    let mut wire_buf = [0_u8; OUTPUT_PACKET_LEN];
    let mut sequence: u32 = 0;
    let mut sent: u64 = 0;
    let mut send_errors: u64 = 0;
    let mut no_target: u64 = 0;
    let mut last_log = Instant::now();

    loop {
        let n = match driver.pend_output(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("\npend_output: {e}");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        if n < driver::OUTPUT_REPORT_SIZE {
            // Driver currently always completes with the full size; guard
            // anyway so a future short-write would be visible rather than
            // silently shipping zeros.
            eprintln!("\npend_output short: {n} bytes");
            continue;
        }

        let Some(addr) = latest_src.lock().ok().and_then(|g| *g) else {
            no_target += 1;
            continue;
        };
        let target = SocketAddr::new(addr.ip(), DEFAULT_PORT);

        let hdr = Header {
            channel: Channel::Output,
            flags: 0,
            sequence,
            timestamp_us: now_us_low32(),
        };
        sequence = sequence.wrapping_add(1);

        if wire::encode_output_packet(&hdr, &buf, auth_key, &mut wire_buf).is_err() {
            send_errors += 1;
            continue;
        }
        match send_sock.send_to(&wire_buf, target) {
            Ok(_) => sent += 1,
            Err(_) => send_errors += 1,
        }

        if last_log.elapsed() >= Duration::from_secs(2) {
            last_log = Instant::now();
            eprintln!(
                "\noutput: sent={sent} senderr={send_errors} no_target={no_target} \
                 last_msg_id=0x{:02x}",
                buf[0],
            );
        }
    }
}

/// Low 32 bits of wall-clock microseconds since Unix epoch.
#[allow(clippy::cast_possible_truncation)]
fn now_us_low32() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u32)
        .unwrap_or(0)
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "windows".to_owned())
}

fn run_pair_mode(identity: &Arc<discovery::Identity>, state_dir: &std::path::Path) {
    let sock = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, DEFAULT_PORT)) {
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
        unicast_target: SocketAddr::new(Ipv4Addr::BROADCAST.into(), DEFAULT_PORT),
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
