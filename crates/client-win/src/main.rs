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
//! Usage: `client-win [port]` (default port 49152).

use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use deck_protocol::hid;
use deck_protocol::wire::{self, Channel, Header, HEADER_LEN, INPUT_PACKET_LEN};
use deck_protocol::ControllerState;

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
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PORT);

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
    let mut hid_buf = [0_u8; hid::REPORT_LEN];
    let mut last_print = Instant::now();
    let mut stdout = std::io::stdout();

    loop {
        let n = match sock.recv_from(&mut buf) {
            Ok((n, _src)) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
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

        if hid::encode_input_report(&state, &mut hid_buf).is_err() {
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
