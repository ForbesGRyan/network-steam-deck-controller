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
//!   client-win [port]            - listen on UDP `port` for Deck input
//!                                  packets (default 49152)
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
use std::time::{Duration, Instant};

use deck_protocol::hid;
use deck_protocol::wire::{self, Channel, Header, HEADER_LEN, INPUT_PACKET_LEN};
use deck_protocol::{Buttons, ControllerState};

#[cfg(windows)]
mod driver;

#[cfg(windows)]
use std::sync::Arc;

const DEFAULT_PORT: u16 = 49152;
const PRINT_EVERY: Duration = Duration::from_millis(50);
const RECV_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Default)]
struct Stats {
    packets: u64,
    dropped: u64,
    wire_errors: u64,
    pushed: u64,
    push_errors: u64,
    last_seq: Option<u32>,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let arg = args.next();
    if matches!(arg.as_deref(), Some("--test")) {
        run_test_mode();
        return;
    }
    if matches!(arg.as_deref(), Some("--replay")) {
        let Some(path) = args.next() else {
            eprintln!("usage: client-win --replay <path-to-hidraw-capture>");
            std::process::exit(2);
        };
        run_replay_mode(&path);
        return;
    }

    let port: u16 = arg.and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_PORT);

    let bind = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port);
    let sock = match UdpSocket::bind(bind) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("bind {bind}: {e}");
            std::process::exit(1);
        }
    };
    let _ = sock.set_read_timeout(Some(RECV_TIMEOUT));

    eprintln!("client-win listening on {bind} (Ctrl-C to quit)");

    #[cfg(windows)]
    let driver = open_driver_with_pend_thread();
    #[cfg(not(windows))]
    eprintln!("driver IPC: requires Windows; running listen-only");

    let mut stats = Stats::default();
    let mut buf = [0_u8; INPUT_PACKET_LEN];
    // Driver IOCTL expects exactly DECK_INPUT_REPORT_SIZE (64) bytes; the
    // protocol crate's encode produces hid::REPORT_LEN (60) — pad with the
    // zero tail so the trailing 4 bytes match what real Deck firmware sends.
    let mut hid_buf = [0_u8; driver::INPUT_REPORT_SIZE];
    let mut last_print = Instant::now();
    let mut last_recv_log = Instant::now();
    let mut stdout = std::io::stdout();
    let mut idle_since = Instant::now();
    let mut idle_logged = false;
    let mut total_received: u64 = 0;
    let mut total_recv_bytes: u64 = 0;
    let mut first_pkt_logged = false;

    loop {
        let n = match sock.recv_from(&mut buf) {
            Ok((n, src)) => {
                total_received += 1;
                total_recv_bytes += n as u64;
                idle_since = Instant::now();
                idle_logged = false;
                if !first_pkt_logged {
                    eprintln!(
                        "\nfirst UDP packet: {n} bytes from {src} \
                         (will print one-time per-source log every 2 s)"
                    );
                    first_pkt_logged = true;
                }
                if last_recv_log.elapsed() >= Duration::from_secs(2) {
                    last_recv_log = Instant::now();
                    eprintln!(
                        "\nrecv heartbeat: total_pkts={total_received} bytes={total_recv_bytes} \
                         last_src={src} last_size={n}"
                    );
                }
                n
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                if !idle_logged && idle_since.elapsed() >= Duration::from_secs(5) {
                    eprintln!(
                        "\nidle: no UDP for 5 s. total_pkts={total_received} \
                         (sock {bind})"
                    );
                    idle_logged = true;
                }
                continue;
            }
            Err(e) => {
                eprintln!("\nrecv: {e}");
                continue;
            }
        };

        let Some((hdr, state)) = parse_packet(&buf, n, &mut stats) else {
            continue;
        };

        if hid::encode_input_report(&state, &mut hid_buf[..hid::REPORT_LEN]).is_err() {
            stats.wire_errors += 1;
            continue;
        }

        #[cfg(windows)]
        if let Some(d) = driver.as_ref() {
            match d.push_input(&hid_buf) {
                Ok(()) => stats.pushed += 1,
                Err(_) => stats.push_errors += 1,
            }
        }

        stats.packets += 1;
        if last_print.elapsed() >= PRINT_EVERY {
            last_print = Instant::now();
            print_status(&mut stdout, &stats, &hdr, &state);
        }
    }
}

fn parse_packet(buf: &[u8], n: usize, stats: &mut Stats) -> Option<(Header, ControllerState)> {
    if n < HEADER_LEN {
        stats.wire_errors += 1;
        return None;
    }
    let Ok(hdr) = wire::decode_header(&buf[..HEADER_LEN]) else {
        stats.wire_errors += 1;
        return None;
    };
    if hdr.channel != Channel::Input {
        return None;
    }
    if n != INPUT_PACKET_LEN {
        stats.wire_errors += 1;
        return None;
    }

    match stats.last_seq {
        Some(prev) if hdr.sequence <= prev => return None,
        Some(prev) => stats.dropped += u64::from(hdr.sequence - prev - 1),
        None => {}
    }
    stats.last_seq = Some(hdr.sequence);

    let Ok(state) = wire::decode_input(&buf[HEADER_LEN..INPUT_PACKET_LEN]) else {
        stats.wire_errors += 1;
        return None;
    };
    Some((hdr, state))
}

fn print_status<W: Write>(out: &mut W, stats: &Stats, hdr: &Header, state: &ControllerState) {
    let _ = write!(
        out,
        "\x1b[2K\rpkts={:>7} drop={:>5} err={:>4} push={:>7} perr={:>4} \
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
fn open_driver_with_pend_thread() -> Option<Arc<driver::DeckDriver>> {
    match driver::DeckDriver::open() {
        Ok(d) => {
            eprintln!("driver: opened");
            let arc = Arc::new(d);
            let pend_handle = arc.clone();
            std::thread::Builder::new()
                .name("deck-pend-output".into())
                .spawn(move || pend_output_loop(&pend_handle))
                .ok();
            Some(arc)
        }
        Err(e) => {
            eprintln!("driver: not available ({e}); running listen-only");
            None
        }
    }
}

/// Drive the kernel-side IOCTL with a synthetic input pattern so the full
/// chain can be exercised before the Deck server is wired up. Toggles the
/// `A` button each second; everything else stays at idle.
#[cfg(windows)]
fn run_test_mode() {
    let driver = match driver::DeckDriver::open() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("driver: not available ({e}); test mode requires the driver");
            std::process::exit(1);
        }
    };
    eprintln!("driver: opened (test mode — synthesizing input @ ~250 Hz)");

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
    let driver = match driver::DeckDriver::open() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("driver: not available ({e}); replay mode requires the driver");
            std::process::exit(1);
        }
    };
    eprintln!("driver: opened (replay mode — {path} @ ~250 Hz, looping)");

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
            let parsed: [u8; hid::REPORT_LEN] = match chunk.try_into() {
                Ok(arr) => arr,
                Err(_) => { skipped += 1; continue; }
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

#[cfg(windows)]
fn pend_output_loop(driver: &driver::DeckDriver) {
    let mut buf = [0_u8; driver::OUTPUT_REPORT_SIZE];
    loop {
        match driver.pend_output(&mut buf) {
            Ok(n) => {
                eprintln!("\noutput report ({n} bytes): {:02x?}", &buf[..n]);
            }
            Err(e) => {
                eprintln!("\npend_output: {e}");
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}
