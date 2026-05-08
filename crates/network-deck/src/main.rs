//! Single Deck-side binary. Subcommands:
//!
//!   network-deck                 GUI; spawns the daemon as a child via sudo
//!   network-deck daemon          Headless daemon (root). GUI invokes this.
//!   network-deck pair            One-shot pair flow (root).
//!   network-deck install         First-run bootstrap (root).
//!
//! On non-Linux the bin's `main` is a stub that never reaches the linux body
//! (which owns the eframe/signal-hook/libc deps). Modules stay compiled so
//! their unit tests run on every platform.

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod connection;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod control;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod sysfs;

#[cfg(target_os = "linux")]
mod app;
#[cfg(target_os = "linux")]
mod daemon;
#[cfg(target_os = "linux")]
mod daemon_child;
#[cfg(target_os = "linux")]
mod hotkey;
#[cfg(target_os = "linux")]
mod install;
#[cfg(target_os = "linux")]
mod pair_worker;

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("network-deck requires Linux. Built on: {}", std::env::consts::OS);
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn hostname() -> String {
    std::env::var("HOSTNAME").unwrap_or_else(|_| "deck".to_owned())
}

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let sub = args.next();

    // Subcommands take no further args — keeps the sudoers rule trivial.
    if args.next().is_some() {
        eprintln!("network-deck: subcommands take no extra arguments");
        std::process::exit(2);
    }

    // The installed binary is setuid root. `daemon` and `install` need root
    // (sysfs writes, /etc edits, /var/lib copy). Everything else (gui, pair)
    // runs as the invoking user — drop privs immediately so the eframe app
    // doesn't run as root, the trust file lands in the user's home, etc.
    let needs_root = matches!(sub.as_deref(), Some("daemon") | Some("install"));
    if !needs_root {
        drop_privs_if_setuid();
    }

    match sub.as_deref() {
        Some("daemon") => {
            daemon::run(daemon::Args {
                state_dir: default_state_dir(),
                sysfs_root: std::path::PathBuf::from("/sys"),
                control_dir: default_control_dir(),
            });
            Ok(())
        }
        Some("pair") => {
            daemon::run_pair(&default_state_dir());
            Ok(())
        }
        Some("install") => {
            install::run()?;
            Ok(())
        }
        Some(other) => {
            eprintln!("unknown subcommand: {other}");
            eprintln!("usage: network-deck [daemon|pair|install]");
            std::process::exit(2);
        }
        None => run_gui(),
    }
}

#[cfg(target_os = "linux")]
fn run_gui() -> Result<(), Box<dyn std::error::Error>> {
    let control_dir = default_control_dir();
    if let Err(e) = std::fs::create_dir_all(&control_dir) {
        eprintln!("create_dir_all {}: {e}", control_dir.display());
    }

    let installed = install::is_installed();
    let paired = is_paired();
    let self_exe = std::env::current_exe()?;
    // Spawn the daemon from the canonical install path when available so
    // it matches the sudoers / polkit rules — regardless of where the
    // kiosk binary itself was invoked from. (You can launch the kiosk via
    // a staging copy in $HOME without breaking auth.)
    let daemon_exe = if std::path::Path::new(install::INSTALL_BIN).exists() {
        std::path::PathBuf::from(install::INSTALL_BIN)
    } else {
        self_exe.clone()
    };
    let state_dir = default_state_dir();

    // Three startup screens, in priority order:
    //   1. Not installed   → setup screen, polkit-driven `install`.
    //   2. Installed but   → in-app pair flow. Don't spawn the daemon yet
    //      not paired         (it'd just exit with "no trusted peer").
    //   3. Installed +     → spawn daemon child, show normal status panel.
    //      paired
    let (daemon_child, daemon_state) = if installed && paired {
        match daemon_child::DaemonChild::spawn(&daemon_exe) {
            Ok((child, state)) => (Some(child), Some(state)),
            Err(e) => {
                eprintln!("spawn daemon: {e}");
                let state = std::sync::Arc::new(daemon_child::DaemonState::default());
                if let Ok(mut tail) = state.stderr_tail.lock() {
                    tail.push(format!("could not start daemon: {e}"));
                    tail.push(format!("tried: sudo -n {} daemon", daemon_exe.display()));
                }
                if let Ok(mut slot) = state.exit_status.lock() {
                    *slot = Some("spawn failed".into());
                }
                (None, Some(state))
            }
        }
    } else {
        (None, None)
    };

    let mut app = app::KioskApp::new(control_dir.clone());
    if !installed {
        app = app.with_setup_required(self_exe.clone());
    } else if !paired {
        let identity = std::sync::Arc::new(
            discovery::identity::load_or_generate(&state_dir)
                .map_err(|e| format!("identity load: {e:?}"))?,
        );
        app = app.with_pair_required(identity, hostname(), state_dir.clone());
    }
    if let Some(state) = daemon_state {
        app = app.with_daemon(daemon_exe.clone(), daemon_child, state);
    }

    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_maximized(true)
            .with_title("Network Deck"),
        ..Default::default()
    };
    eframe::run_native(
        "Network Deck",
        native_options,
        Box::new(move |_cc| Ok(Box::new(app))),
    )?;
    // KioskApp owns the daemon child; its Drop chain SIGTERMs the child.
    Ok(())
}

/// Per-user runtime control dir. The GUI runs as the deck user and finds it
/// via `$XDG_RUNTIME_DIR`. The daemon runs as root via sudo (which strips
/// `XDG_RUNTIME_DIR`), so we fall back to `/run/user/<SUDO_UID>/network-deck`.
/// Both paths resolve to the same on-disk directory (`/run/user/<uid>/`),
/// owned by the user, writable by root.
#[cfg(target_os = "linux")]
fn default_control_dir() -> std::path::PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR") {
        return std::path::PathBuf::from(xdg).join("network-deck");
    }
    if let Ok(uid) = std::env::var("SUDO_UID") {
        return std::path::PathBuf::from(format!("/run/user/{uid}/network-deck"));
    }
    std::path::PathBuf::from("/run/network-deck")
}

/// Resolve the state dir (where `trusted-peers.toml` and the identity live).
///
/// The GUI runs as the deck user; the daemon runs via `sudo -n`. We want both
/// to land on the same path so that pairing-as-user and reading-as-root see
/// the same trust file. Strategy:
///   * If `$SUDO_USER` is set (we're under sudo): look up that user's home
///     via `getent` and use `<home>/.local/state/network-deck`.
///   * Otherwise: use `$HOME/.local/state/network-deck` (the user's view).
#[cfg(target_os = "linux")]
fn default_state_dir() -> std::path::PathBuf {
    if let Ok(user) = std::env::var("SUDO_USER") {
        if let Some(home) = home_for_user(&user) {
            return home.join(".local/state/network-deck");
        }
    }
    discovery::state_dir::default_state_dir().unwrap_or_else(|e| {
        eprintln!("cannot resolve state dir: {e:?}");
        std::process::exit(1)
    })
}

/// `getent passwd <user>` → home dir. Returns `None` on any failure.
#[cfg(target_os = "linux")]
fn home_for_user(user: &str) -> Option<std::path::PathBuf> {
    let out = std::process::Command::new("getent")
        .args(["passwd", user])
        .output()
        .ok()?;
    if !out.status.success() { return None; }
    let line = std::str::from_utf8(&out.stdout).ok()?.trim_end().to_owned();
    line.split(':').nth(5).map(std::path::PathBuf::from)
}

/// Permanently drop to the real user/group ids if we entered as setuid
/// root. No-op when not setuid. Uses `setresuid`/`setresgid` so the saved
/// uid is also reset — there's no way to re-elevate after this.
#[cfg(target_os = "linux")]
fn drop_privs_if_setuid() {
    // SAFETY: bare libc calls; arguments are valid uid/gid_t values from
    // getuid/getgid/geteuid (which can't fail).
    unsafe {
        let ruid = libc::getuid();
        let rgid = libc::getgid();
        let euid = libc::geteuid();
        if euid == 0 && ruid != 0 {
            if libc::setresgid(rgid, rgid, rgid) != 0 {
                eprintln!("setresgid failed: {}", std::io::Error::last_os_error());
                std::process::exit(1);
            }
            if libc::setresuid(ruid, ruid, ruid) != 0 {
                eprintln!("setresuid failed: {}", std::io::Error::last_os_error());
                std::process::exit(1);
            }
        }
    }
}

/// True iff a `trusted-peers.toml` is already on disk (= we've paired).
#[cfg(target_os = "linux")]
fn is_paired() -> bool {
    discovery::trust::load(&default_state_dir())
        .map(|opt| opt.is_some())
        .unwrap_or(false)
}

