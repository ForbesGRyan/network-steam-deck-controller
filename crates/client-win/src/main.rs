//! Windows-side tray app for the network-deck bridge.
//!
//! Watches the discovery beacon for a paired Deck. When the Deck appears,
//! shells out to `usbip.exe attach` (usbip-win2). Auto-reattaches on
//! network blips. Tray menu lets the user override.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(windows)]
mod attach;
#[cfg(windows)]
mod autostart;
#[cfg(windows)]
mod tray;
#[cfg(windows)]
mod usbip_cli;

const DEFAULT_PORT: u16 = 49152;
const TICK_INTERVAL: Duration = Duration::from_millis(500);

enum Mode {
    Run,
    Pair,
}

struct ParsedArgs {
    mode: Mode,
    state_dir: PathBuf,
}

fn parse_args() -> ParsedArgs {
    let mut args = std::env::args().skip(1);
    let mut mode = Mode::Run;
    let mut state_dir_override: Option<PathBuf> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "pair" => mode = Mode::Pair,
            "--state-dir" => {
                state_dir_override = args.next().map(PathBuf::from);
                if state_dir_override.is_none() {
                    eprintln!("--state-dir requires a value");
                    std::process::exit(2);
                }
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

    #[cfg(windows)]
    {
        if autostart::current().is_none() {
            if let Err(e) = autostart::enable() {
                eprintln!("autostart register failed (code {e})");
            } else {
                eprintln!("autostart: registered for HKCU\\...\\Run");
            }
        }
    }

    run_normal(&identity, &args.state_dir);
}

fn run_normal(identity: &Arc<discovery::Identity>, state_dir: &std::path::Path) {
    let trusted = match discovery::trust::load(state_dir) {
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

    let bind_addr = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), DEFAULT_PORT);
    let bound = UdpSocket::bind(bind_addr).unwrap_or_else(|e| {
        eprintln!("bind {bind_addr}: {e}");
        std::process::exit(1);
    });
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

    let beacon_recv = beacon.clone();
    std::thread::Builder::new()
        .name("discovery-recv".into())
        .spawn(move || {
            let mut buf = [0_u8; discovery::packet::PACKET_LEN];
            loop {
                if let Ok((n, src)) = bound.recv_from(&mut buf) {
                    if n >= 4 && buf[0..4] == discovery::BEACON_MAGIC {
                        beacon_recv.handle_packet(src, &buf[..n]);
                    }
                }
            }
        })
        .ok();

    eprintln!(
        "client-win running — paired with {} (fingerprint {})",
        trusted.name,
        identity.fingerprint_str(),
    );

    #[cfg(windows)]
    run_attach_loop(&beacon);

    #[cfg(not(windows))]
    {
        eprintln!("attach loop requires Windows; idling.");
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
}

#[cfg(windows)]
fn run_attach_loop(beacon: &Arc<discovery::Beacon>) {
    use crate::attach::{Attach, State, UsbipDriver};
    use crate::tray::TrayEvent;
    use crate::usbip_cli::{CliError, UsbipCli};

    let cli = match UsbipCli::discover() {
        Ok(c) => c,
        Err(CliError::NotInstalled) => {
            eprintln!("usbip.exe not found. Install usbip-win2 first.");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("usbip locate: {e:?}");
            std::process::exit(1);
        }
    };

    struct CliDriver {
        cli: UsbipCli,
    }
    impl UsbipDriver for CliDriver {
        fn discover_busid(&mut self, host: &str) -> Option<String> {
            self.cli
                .list_remote(host)
                .ok()?
                .into_iter()
                .find(|d| d.vid == "28de" && d.pid == "1205")
                .map(|d| d.busid)
        }
        fn attach(&mut self, host: &str, busid: &str) -> bool {
            self.cli.attach(host, busid).is_ok()
        }
        fn ported_busids(&mut self) -> Vec<String> {
            self.cli.port().unwrap_or_default()
        }
    }
    let mut driver = CliDriver { cli };

    let (tray_rx, tray_handle) = tray::spawn();
    let mut sm = Attach::default();
    let mut paused = false;

    loop {
        // Drain tray events.
        while let Ok(event) = tray_rx.try_recv() {
            match event {
                TrayEvent::Connect => {
                    paused = false;
                    eprintln!("tray: connect");
                }
                TrayEvent::Disconnect => {
                    paused = true;
                    eprintln!("tray: disconnect (paused until Connect)");
                }
                TrayEvent::Pair => {
                    eprintln!("tray: pair (run `client-win pair` from a shell — TODO inline UI)");
                }
                TrayEvent::Quit => {
                    eprintln!("tray: quit");
                    return;
                }
            }
        }

        let peer_with_age = beacon.current_peer_with_age();
        let peer_present = !paused
            && peer_with_age
                .is_some_and(|(_, age)| age <= discovery::beacon::STALE_AFTER);
        let peer_host = peer_with_age.map(|(addr, _)| addr.ip().to_string());

        if let Some(action) =
            sm.tick(peer_present, peer_host.as_deref(), Instant::now(), &mut driver)
        {
            eprintln!("attach: {action:?} (state={:?})", sm.state());
        }

        let tooltip = match (sm.state(), peer_with_age) {
            (State::Attached, _) => "Network Deck — connected".to_owned(),
            (State::Idle, Some((addr, age))) if age <= discovery::beacon::STALE_AFTER => {
                format!("Network Deck — connecting ({})", addr.ip())
            }
            (State::Idle, _) if paused => "Network Deck — paused".to_owned(),
            (State::Idle, _) => "Network Deck — searching".to_owned(),
        };
        tray_handle.set_tooltip(&tooltip);

        std::thread::sleep(TICK_INTERVAL);
    }
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
