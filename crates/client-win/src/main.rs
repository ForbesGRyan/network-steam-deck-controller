//! Windows-side tray app for the network-deck bridge.
//!
//! Watches the discovery beacon for a paired Deck. When the Deck appears,
//! shells out to `usbip.exe attach` (usbip-win2). Auto-reattaches on
//! network blips. Tray menu lets the user override.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long to reuse a `usbip port` result before re-shelling. The attach
/// loop ticks at 500 ms; without this, we'd `CreateProcess` on every tick
/// just to detect remote drop.
#[cfg(windows)]
const PORT_CACHE_TTL: Duration = Duration::from_secs(2);

/// Stoppable beacon recv thread. Holds the bound `UdpSocket` for its
/// lifetime; on `stop()` returns the socket back to the caller so the pair
/// flow can take it over without re-binding 49152 (which fails — we already
/// hold it). Recv uses a 500 ms read timeout so the stop flag is observed
/// promptly.
struct RecvThread {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<UdpSocket>,
}

impl RecvThread {
    fn spawn(sock: UdpSocket, beacon: Arc<discovery::Beacon>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = std::thread::Builder::new()
            .name("discovery-recv".into())
            .spawn(move || {
                let _ = sock.set_read_timeout(Some(Duration::from_millis(500)));
                let mut buf = [0_u8; discovery::packet::PACKET_LEN];
                while !stop2.load(Ordering::Relaxed) {
                    match sock.recv_from(&mut buf) {
                        Ok((n, src)) => {
                            if n >= 4 && buf[0..4] == discovery::BEACON_MAGIC {
                                beacon.handle_packet(src, &buf[..n]);
                            }
                        }
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) => {}
                        Err(_) => {}
                    }
                }
                sock
            })
            .expect("spawn discovery-recv");
        Self { stop, handle }
    }

    fn stop(self) -> UdpSocket {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.join().expect("recv thread panicked")
    }
}

#[cfg(windows)]
mod attach;
#[cfg(windows)]
mod autostart;
#[cfg(windows)]
mod pair_dialog;
#[cfg(windows)]
mod tray;
#[cfg(windows)]
mod usbip_cli;
#[cfg(windows)]
mod util;

use discovery::BEACON_PORT as DEFAULT_PORT;

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
            #[cfg(windows)]
            {
                first_run_pair(identity.clone(), state_dir);
            }
            #[cfg(not(windows))]
            {
                eprintln!("no trusted peer; run `client-win pair` to pair first");
                std::process::exit(1);
            }
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
            discovery::netifs::broadcast_targets(DEFAULT_PORT),
            hostname(),
            DEFAULT_PORT,
        )
        .unwrap_or_else(|e| {
            eprintln!("beacon init: {e:?}");
            std::process::exit(1);
        }),
    );
    discovery::beacon::spawn_broadcast(beacon.clone());

    let recv = RecvThread::spawn(bound, beacon.clone());

    eprintln!(
        "client-win running — paired with {} (fingerprint {})",
        trusted.name,
        identity.fingerprint_str(),
    );

    #[cfg(windows)]
    run_attach_loop(&beacon, recv, identity, state_dir);

    #[cfg(not(windows))]
    let _ = recv;

    #[cfg(not(windows))]
    {
        eprintln!("attach loop requires Windows; idling.");
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
}

#[cfg(windows)]
struct CliDriver {
    cli: crate::usbip_cli::UsbipCli,
    port_cache: Option<(Instant, Vec<String>)>,
}

#[cfg(windows)]
impl crate::attach::UsbipDriver for CliDriver {
    fn discover_busid(&mut self, host: &str) -> Option<String> {
        self.cli
            .list_remote(host)
            .ok()?
            .into_iter()
            .find(|d| d.vid == discovery::DECK_VID && d.pid == discovery::DECK_PID)
            .map(|d| d.busid)
    }
    fn attach(&mut self, host: &str, busid: &str) -> bool {
        // Attaching changes the port table; invalidate so the next
        // `ported_busids` call re-shells and the state machine sees the
        // new busid in <PORT_CACHE_TTL.
        self.port_cache = None;
        self.cli.attach(host, busid).is_ok()
    }
    fn ported_busids(&mut self) -> Vec<String> {
        if let Some((at, ref ports)) = self.port_cache {
            if at.elapsed() < PORT_CACHE_TTL {
                return ports.clone();
            }
        }
        let ports = self.cli.port().unwrap_or_default();
        self.port_cache = Some((Instant::now(), ports.clone()));
        ports
    }
}

#[cfg(windows)]
#[allow(clippy::too_many_lines)]
fn run_attach_loop(
    beacon: &Arc<discovery::Beacon>,
    mut recv: RecvThread,
    identity: &Arc<discovery::Identity>,
    state_dir: &Path,
) {
    use crate::attach::{Attach, State};
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

    let mut driver = CliDriver { cli, port_cache: None };

    let (tray_rx, tray_handle) = tray::spawn();
    let mut sm = Attach::default();
    let mut paused = false;
    let mut last_tooltip = String::new();

    loop {
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
                    eprintln!("tray: pair — handing socket to pair dialog");
                    tray_handle.set_tooltip("Network Deck — pairing…");
                    // Stop the recv thread so the pair flow can use the
                    // 49152 socket without conflict. recv.stop() returns
                    // the socket; we hand it to pair_dialog.
                    let sock = recv.stop();
                    let outcome = pair_dialog::run(
                        sock,
                        Arc::clone(identity),
                        hostname(),
                        state_dir,
                    );
                    match outcome {
                        discovery::pair::PairOutcome::Paired(_) => {
                            // Re-exec to pick up the new trust file cleanly.
                            // Drop the tray + driver state by exiting.
                            eprintln!("pair complete — restarting");
                            // Drop the tray handle BEFORE the re-exec so
                            // NIM_DELETE on our icon completes before the
                            // new process paints its own (avoids a brief
                            // double-icon flash) and so process::exit
                            // doesn't skip the tray destructor.
                            drop(tray_handle);
                            util::reexec_self();
                        }
                        other => {
                            eprintln!("pair did not complete: {other:?}");
                            // Re-bind & resume normal operation. We can't
                            // reuse the socket (pair_dialog::run consumed
                            // it) so re-bind 49152.
                            let bind_addr = SocketAddr::new(
                                Ipv4Addr::UNSPECIFIED.into(),
                                DEFAULT_PORT,
                            );
                            match UdpSocket::bind(bind_addr) {
                                Ok(s) => {
                                    recv = RecvThread::spawn(s, beacon.clone());
                                    tray_handle.set_tooltip("Network Deck — searching");
                                }
                                Err(e) => {
                                    eprintln!("re-bind {bind_addr}: {e}");
                                    std::process::exit(1);
                                }
                            }
                        }
                    }
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
        if tooltip != last_tooltip {
            tray_handle.set_tooltip(&tooltip);
            last_tooltip.clone_from(&tooltip);
        }

        std::thread::sleep(TICK_INTERVAL);
    }
}

/// First-run pair. Binds 49152, hands the socket to `pair_dialog`, and on
/// success re-execs ourselves so the freshly-written trust file is picked
/// up cleanly on the next launch. Always diverges.
#[cfg(windows)]
fn first_run_pair(identity: Arc<discovery::Identity>, state_dir: &std::path::Path) -> ! {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONINFORMATION, MB_OK,
    };

    // Heads-up dialog so the user can put the Deck in pair mode before we
    // start broadcasting. The pair flow has its own 120 s timeout once we
    // proceed; this dialog is the chance to hit Cancel via window close.
    let title = util::wide("Network Deck — first-time pair");
    let body = util::wide(
        "No paired Deck found.\n\n\
         On the Deck, launch Network Deck and tap \"Start pairing\".\n\
         Click OK to start pairing on this PC.",
    );
    // SAFETY: NUL-terminated UTF-16 strings, null hwnd, valid flags.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            body.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONINFORMATION,
        );
    }

    let bind_addr = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), DEFAULT_PORT);
    let sock = UdpSocket::bind(bind_addr).unwrap_or_else(|e| {
        eprintln!("bind {bind_addr}: {e}");
        std::process::exit(1);
    });

    let outcome = pair_dialog::run(sock, identity, hostname(), state_dir);
    match outcome {
        discovery::pair::PairOutcome::Paired(_) => util::reexec_self(),
        other => {
            eprintln!("pair did not complete: {other:?}");
            std::process::exit(1);
        }
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
        targets: discovery::netifs::broadcast_targets(DEFAULT_PORT),
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
