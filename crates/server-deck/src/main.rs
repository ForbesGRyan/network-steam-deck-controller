//! Steam Deck server: find the internal controller in sysfs, start a
//! discovery beacon, and bind/unbind the device to `usbip-host` based on
//! whether the paired peer's beacon is fresh.
//!
//! Usage:
//! ```text
//! server-deck                          # normal mode
//! server-deck pair                     # one-shot pair
//! server-deck --state-dir <path>       # override state dir
//! ```

mod connection;
mod control;
mod sysfs;

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!(
        "server-deck requires Linux (usbip). Built on: {}",
        std::env::consts::OS
    );
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn main() {
    linux::run();
}

#[cfg(target_os = "linux")]
fn hostname() -> String {
    std::env::var("HOSTNAME").unwrap_or_else(|_| "deck".to_owned())
}

#[cfg(target_os = "linux")]
fn run_pair_mode(
    identity: &std::sync::Arc<discovery::Identity>,
    state_dir: &std::path::Path,
    port: u16,
) {
    use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
    use std::time::Duration;

    let sock = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)) {
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
        unicast_target: SocketAddr::new(Ipv4Addr::BROADCAST.into(), port),
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

#[cfg(target_os = "linux")]
mod linux {
    use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
    use std::path::PathBuf;
    use std::time::Duration;

    use super::connection::{Connection, RealRunner};
    use super::sysfs::{find_deck_busid, DECK_PID, DECK_VID};

    /// UDP port the discovery beacon shares with the (now defunct) data plane.
    /// Kept the same value for backward compatibility with paired peers that
    /// were paired pre-pivot — the trust file only stores the pubkey, but the
    /// beacon listen port has to match what `client-win` expects.
    const BEACON_PORT: u16 = 49152;

    /// How often we poll beacon state and drive the connection state machine.
    const TICK_INTERVAL: Duration = Duration::from_millis(500);

    enum Mode { Run, Pair }

    struct ParsedArgs {
        mode: Mode,
        state_dir: PathBuf,
        sysfs_root: PathBuf,
        control_dir: PathBuf,
    }

    fn parse_args() -> ParsedArgs {
        let mut args = std::env::args().skip(1);
        let mut mode = Mode::Run;
        let mut state_dir_override: Option<PathBuf> = None;
        let mut sysfs_root_override: Option<PathBuf> = None;
        let mut control_dir_override: Option<PathBuf> = None;
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
                "--sysfs-root" => {
                    // For testing only — points at a tempdir mocked sysfs.
                    sysfs_root_override = args.next().map(PathBuf::from);
                    if sysfs_root_override.is_none() {
                        eprintln!("--sysfs-root requires a value");
                        std::process::exit(2);
                    }
                }
                "--control-dir" => {
                    control_dir_override = args.next().map(PathBuf::from);
                    if control_dir_override.is_none() {
                        eprintln!("--control-dir requires a value");
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
        let sysfs_root = sysfs_root_override.unwrap_or_else(|| PathBuf::from("/sys"));
        let control_dir = control_dir_override.unwrap_or_else(|| PathBuf::from("/run/network-deck"));
        ParsedArgs { mode, state_dir, sysfs_root, control_dir }
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
            super::run_pair_mode(&identity, &args.state_dir, BEACON_PORT);
            return;
        }

        let trusted = match discovery::trust::load(&args.state_dir) {
            Ok(Some(p)) => std::sync::Arc::new(p),
            Ok(None) => {
                eprintln!("no trusted peer; run `server-deck pair` to pair first");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("trust load: {e:?}");
                std::process::exit(1);
            }
        };

        let busid = find_deck_busid(&args.sysfs_root, DECK_VID, DECK_PID).unwrap_or_else(|e| {
            eprintln!(
                "could not find Steam Deck controller (VID {DECK_VID} PID {DECK_PID}) in sysfs: {e:?}"
            );
            std::process::exit(1);
        });
        eprintln!("found Deck controller at busid {busid}");

        let bound = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, BEACON_PORT)).unwrap_or_else(|e| {
            eprintln!("bind 0.0.0.0:{BEACON_PORT}: {e}");
            std::process::exit(1);
        });

        let beacon = std::sync::Arc::new(
            discovery::Beacon::new(
                identity.clone(),
                trusted.clone(),
                SocketAddr::new(Ipv4Addr::BROADCAST.into(), BEACON_PORT),
                super::hostname(),
                BEACON_PORT,
            )
            .unwrap_or_else(|e| {
                eprintln!("beacon init: {e:?}");
                std::process::exit(1);
            }),
        );
        discovery::beacon::spawn_broadcast(beacon.clone());

        // Beacon recv thread: drains incoming beacons so the live-peer state
        // updates. Kept on its own thread because the recv is blocking.
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
            "supervising bind state for busid {busid} -> peer {} (fingerprint {})",
            trusted.name,
            identity.fingerprint_str(),
        );

        let mut conn = Connection::new(busid.clone());
        let mut runner = RealRunner;
        loop {
            let beacon_present = beacon.current_peer_with_age()
                .is_some_and(|(_, age)| age <= discovery::beacon::STALE_AFTER);
            let paused = crate::control::is_paused(&args.control_dir);
            let effective_peer_present = beacon_present && !paused;
            if let Some(action) = conn.tick(effective_peer_present, &mut runner) {
                eprintln!("connection: {action:?} (state={:?})", conn.state());
            }
            let status = crate::control::Status {
                peer_name: Some(trusted.name.clone()),
                peer_present: beacon_present, // raw beacon, NOT effective — UI distinguishes "Connected but paused" from "Searching"
                bound: matches!(conn.state(), crate::connection::State::Bound),
                paused,
            };
            if let Err(e) = crate::control::write_status(&args.control_dir, &status) {
                // Log once and keep going — kiosk just shows stale info, daemon must not crash on a missing /run dir
                eprintln!("control: write_status failed: {e}");
            }
            std::thread::sleep(TICK_INTERVAL);
        }
    }
}
