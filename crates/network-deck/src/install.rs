//! First-run installer: bootstraps the Deck so the kiosk can launch the
//! daemon without further setup. Idempotent. Touches the readonly fs only
//! for the one-shot `pacman -S usbip` (and only if it's missing).
//!
//! What it does:
//!   1. Verifies (and optionally installs via pacman) the `usbip` userspace.
//!   2. Loads usbip kernel modules + persists them in `/etc/modules-load.d/`.
//!   3. Enables `usbipd.service` (system-managed, auto-starts at boot).
//!   4. Disables the old `network-deck-server.service` if it exists.
//!   5. Copies `argv[0]` to `/var/lib/network-deck/network-deck` (root-owned).
//!      `/var/lib/` is writable on SteamOS and avoids `$HOME` collisions
//!      (a binary-named-`network-deck` already in your home would otherwise
//!      block creating a directory with the same name).
//!   6. Writes `/etc/sudoers.d/network-deck` (NOPASSWD for daemon launch).
//!   7. Drops a `.desktop` entry in `~deck/.local/share/applications/`.
//!
//! Must be run as root: `sudo network-deck install`. The "deck user" is
//! taken from `$SUDO_USER` (the user who invoked sudo) — falls back to
//! `deck` if missing.

use std::path::PathBuf;
use std::process::Command;

const SUDOERS_PATH: &str = "/etc/sudoers.d/network-deck";
const MODULES_LOAD_PATH: &str = "/etc/modules-load.d/usbip.conf";
const MODULES_LOAD_BODY: &str = "usbip-core\nusbip-host\nvhci-hcd\n";
/// Where the daemon binary is installed. Root-owned so the deck user can't
/// swap it out and abuse the NOPASSWD sudoers rule.
pub const INSTALL_DIR: &str = "/var/lib/network-deck";
pub const INSTALL_BIN: &str = "/var/lib/network-deck/network-deck";

pub fn run() -> std::io::Result<()> {
    if !is_root() {
        eprintln!("install must be run as root: sudo network-deck install");
        std::process::exit(1);
    }

    let user = std::env::var("SUDO_USER").unwrap_or_else(|_| "deck".to_owned());
    let home = home_for(&user).unwrap_or_else(|| PathBuf::from(format!("/home/{user}")));
    let install_dir = PathBuf::from(INSTALL_DIR);
    let install_bin = PathBuf::from(INSTALL_BIN);
    let app_dir = home.join(".local/share/applications");
    let desktop_path = app_dir.join("network-deck-kiosk.desktop");

    eprintln!(">> install user={user} home={}", home.display());
    eprintln!(">> install dir={}", install_dir.display());

    ensure_usbip_userspace()?;
    load_kernel_modules()?;
    persist_modules_load()?;
    enable_usbipd();
    disable_old_systemd_unit();
    copy_self_to(&install_bin)?;
    chown(&install_dir, "root", "root")?;
    chmod(&install_dir, 0o755)?;
    // 04755 = setuid + 0755. The kernel runs the binary as root regardless
    // of who invokes it, so the kiosk (running as the deck user) can spawn
    // `network-deck daemon` directly without sudo or pkexec. The binary
    // itself drops privs back to the real user for non-privileged
    // subcommands (gui, pair) — see `drop_privs_if_setuid` in main.
    chmod(&install_bin, 0o4755)?;
    write_sudoers(&user, &install_bin)?;
    write_desktop(&user, &app_dir, &desktop_path, &install_bin)?;

    eprintln!();
    eprintln!("Done.");
    eprintln!();
    eprintln!("Next: pair with your Windows PC (run on each side at once):");
    eprintln!("  sudo {}", install_bin.display());
    eprintln!("    pair");
    eprintln!("  client-win.exe pair    # on Windows");
    eprintln!();
    eprintln!("Use:");
    eprintln!("  Add {} to Steam as a non-Steam game", install_bin.display());
    eprintln!("  (one-time, in Desktop Mode). Tap from Game Mode to start;");
    eprintln!("  closing the window stops the daemon and unbinds the controller.");
    Ok(())
}

fn is_root() -> bool {
    // SAFETY: getuid() is signal-safe and trivially correct.
    unsafe { libc::getuid() == 0 }
}

fn home_for(user: &str) -> Option<PathBuf> {
    Command::new("getent")
        .args(["passwd", user])
        .output()
        .ok()
        .and_then(|out| if out.status.success() { Some(out.stdout) } else { None })
        .and_then(|stdout| {
            let line = std::str::from_utf8(&stdout).ok()?.trim_end().to_owned();
            line.split(':').nth(5).map(PathBuf::from)
        })
}

fn ensure_usbip_userspace() -> std::io::Result<()> {
    if which_present("usbip") {
        return Ok(());
    }
    let has_steamos_readonly = which_present("steamos-readonly");
    if !has_steamos_readonly {
        eprintln!("usbip not found. Install it via your distro's package manager and re-run.");
        std::process::exit(1);
    }
    eprintln!(">> SteamOS detected. Disabling readonly + pacman -S usbip ...");
    if !run_ok("steamos-readonly", &["disable"]) {
        eprintln!("steamos-readonly disable failed");
        std::process::exit(1);
    }

    let trustdb = std::path::Path::new("/etc/pacman.d/gnupg/trustdb.gpg");
    if !trustdb.exists() {
        let _ = run_ok("pacman-key", &["--init"]);
        let _ = run_ok("pacman-key", &["--populate"]);
    }
    let pacman_ok = run_ok("pacman", &["-S", "--noconfirm", "usbip"]);

    let _ = run_ok("steamos-readonly", &["enable"]);
    if !pacman_ok {
        eprintln!("pacman -S usbip failed");
        std::process::exit(1);
    }
    Ok(())
}

fn load_kernel_modules() -> std::io::Result<()> {
    eprintln!(">> Loading kernel modules...");
    for m in ["usbip-core", "usbip-host", "vhci-hcd"] {
        if !run_ok("modprobe", &[m]) {
            eprintln!("modprobe {m} failed (module may already be loaded; continuing)");
        }
    }
    Ok(())
}

fn persist_modules_load() -> std::io::Result<()> {
    eprintln!(">> Writing {MODULES_LOAD_PATH}");
    std::fs::write(MODULES_LOAD_PATH, MODULES_LOAD_BODY)
}

fn enable_usbipd() {
    eprintln!(">> Enabling usbipd.service...");
    if !run_ok("systemctl", &["enable", "--now", "usbipd.service"]) {
        eprintln!("warning: enable usbipd.service failed");
    }
}

fn disable_old_systemd_unit() {
    let unit = "network-deck-server.service";
    let exists = Command::new("systemctl")
        .args(["list-unit-files", unit])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(unit))
        .unwrap_or(false);
    if exists {
        eprintln!(">> Disabling old {unit}");
        let _ = run_ok("systemctl", &["disable", "--now", unit]);
    }
}

fn copy_self_to(dest: &std::path::Path) -> std::io::Result<()> {
    let src = std::env::current_exe()?;
    if src == dest {
        eprintln!(">> Already running from install location; skipping self-copy");
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    eprintln!(">> Copying {} -> {}", src.display(), dest.display());
    std::fs::copy(&src, dest)?;
    Ok(())
}

fn write_sudoers(user: &str, install_bin: &std::path::Path) -> std::io::Result<()> {
    // Strict sudoers form — exact arg match. Daemon and pair both take no
    // CLI args; the daemon derives its control dir from $SUDO_UID and its
    // state dir from HOME (which sudo resets to /root). This keeps the
    // privilege scope narrow and predictable.
    let body = format!(
        "# Allow {user} to launch the network-deck daemon without a password\n\
         # prompt. The daemon needs root for usbip bind/unbind on sysfs.\n\
         {user} ALL=(root) NOPASSWD: {bin} daemon, {bin} pair\n",
        bin = install_bin.display(),
    );
    eprintln!(">> Writing {SUDOERS_PATH}");
    std::fs::write(SUDOERS_PATH, body)?;
    chmod(std::path::Path::new(SUDOERS_PATH), 0o440)?;
    if !run_ok("visudo", &["-c", "-f", SUDOERS_PATH]) {
        eprintln!("visudo validation failed for {SUDOERS_PATH}");
        std::process::exit(1);
    }
    Ok(())
}

fn write_desktop(
    user: &str,
    app_dir: &std::path::Path,
    desktop_path: &std::path::Path,
    install_bin: &std::path::Path,
) -> std::io::Result<()> {
    std::fs::create_dir_all(app_dir)?;
    let body = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Network Deck\n\
         Comment=Wireless controller bridge to PC\n\
         Exec={bin}\n\
         Icon=input-gaming\n\
         Terminal=false\n\
         Categories=Game;\n",
        bin = install_bin.display(),
    );
    eprintln!(">> Writing {}", desktop_path.display());
    std::fs::write(desktop_path, body)?;
    chown(app_dir, user, user)?;
    chown(desktop_path, user, user)?;
    Ok(())
}

fn chown(path: &std::path::Path, user: &str, group: &str) -> std::io::Result<()> {
    if !run_ok("chown", &[&format!("{user}:{group}"), &path.display().to_string()]) {
        eprintln!("warning: chown {user}:{group} {} failed", path.display());
    }
    Ok(())
}

fn chmod(path: &std::path::Path, mode: u32) -> std::io::Result<()> {
    if !run_ok("chmod", &[&format!("{mode:o}"), &path.display().to_string()]) {
        eprintln!("warning: chmod {mode:o} {} failed", path.display());
    }
    Ok(())
}

fn run_ok(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn which_present(cmd: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {cmd}")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[must_use]
pub fn is_installed() -> bool {
    std::path::Path::new(SUDOERS_PATH).exists()
}
