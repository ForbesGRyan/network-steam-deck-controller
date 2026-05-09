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
//!      `/var/lib/` is writable on `SteamOS` and avoids `$HOME` collisions
//!      (a binary-named-`network-deck` already in your home would otherwise
//!      block creating a directory with the same name).
//!   6. Writes `/etc/sudoers.d/network-deck` (NOPASSWD for daemon launch).
//!   7. Drops a `.desktop` entry in `~deck/.local/share/applications/`.
//!
//! Must be run as root: `sudo network-deck install`. The "deck user" is
//! taken from `$SUDO_USER` (the user who invoked sudo) — falls back to
//! `deck` if missing.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Path-bearing install configuration. Lets the install/uninstall file-side
/// run against any directory tree (real `/etc`, `/var/lib`, `~/.local/...`
/// or a tempdir for tests). All paths are absolute.
#[derive(Debug, Clone)]
pub(super) struct InstallContext {
    pub sudoers_path: PathBuf,
    pub install_dir: PathBuf,
    pub install_bin: PathBuf,
    pub modules_load_path: PathBuf,
    pub app_dir: PathBuf,
    pub desktop_path: PathBuf,
    /// Sudoers files from older releases that should be removed on install
    /// or uninstall to keep the sudoers.d/ tree clean across upgrades.
    pub legacy_sudoers_paths: Vec<PathBuf>,
}

impl InstallContext {
    /// The production layout. Used by `run` and `uninstall`. Tests build
    /// their own contexts pointed at a tempdir.
    fn production(home: &Path) -> Self {
        Self {
            sudoers_path: PathBuf::from(SUDOERS_PATH),
            install_dir: PathBuf::from(INSTALL_DIR),
            install_bin: PathBuf::from(INSTALL_BIN),
            modules_load_path: PathBuf::from(MODULES_LOAD_PATH),
            app_dir: home.join(".local/share/applications"),
            desktop_path: home
                .join(".local/share/applications/network-deck-kiosk.desktop"),
            legacy_sudoers_paths: LEGACY_SUDOERS_PATHS
                .iter()
                .map(PathBuf::from)
                .collect(),
        }
    }
}

/// File-system side of `install`: writes sudoers, modules-load, .desktop;
/// removes legacy sudoers paths. Idempotent — re-running on top of an
/// existing layout overwrites in place. Does NOT chown/chmod, run visudo,
/// invoke systemctl, or copy `argv[0]` — those are wrapped around this in
/// `run`. Pure on the file system; testable against a tempdir.
pub(super) fn install_files(ctx: &InstallContext) -> std::io::Result<()> {
    if let Some(parent) = ctx.sudoers_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&ctx.sudoers_path, sudoers_body(&ctx.install_bin))?;

    if let Some(parent) = ctx.modules_load_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&ctx.modules_load_path, MODULES_LOAD_BODY)?;

    std::fs::create_dir_all(&ctx.install_dir)?;

    std::fs::create_dir_all(&ctx.app_dir)?;
    std::fs::write(&ctx.desktop_path, desktop_body(&ctx.install_bin))?;

    for legacy in &ctx.legacy_sudoers_paths {
        if legacy == &ctx.sudoers_path {
            continue;
        }
        match std::fs::remove_file(legacy) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }

    Ok(())
}

/// File-system side of `uninstall`: removes sudoers, modules-load,
/// install dir, .desktop, plus all legacy sudoers paths. Idempotent —
/// missing files are not errors. Does NOT touch systemctl or remove the
/// usbip package; production `uninstall` wraps this with those.
pub(super) fn uninstall_files(ctx: &InstallContext) -> std::io::Result<()> {
    let remove_file = |p: &Path| -> std::io::Result<()> {
        match std::fs::remove_file(p) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    };
    let remove_dir_all = |p: &Path| -> std::io::Result<()> {
        match std::fs::remove_dir_all(p) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    };

    remove_file(&ctx.sudoers_path)?;
    remove_file(&ctx.modules_load_path)?;
    remove_dir_all(&ctx.install_dir)?;
    remove_file(&ctx.desktop_path)?;
    for legacy in &ctx.legacy_sudoers_paths {
        remove_file(legacy)?;
    }
    Ok(())
}

/// Sudoers file path. The `zz-` prefix forces this file to load AFTER
/// `SteamOS`'s `/etc/sudoers.d/wheel` (`%wheel ALL=(ALL) ALL`, no NOPASSWD)
/// in alphabetical scan order. Sudoers' last-match-wins evaluation means a
/// filename earlier than `wheel` gets overridden by the wheel rule, and our
/// NOPASSWD line stops working — surfaced as `sudo: a password is required`
/// when the kiosk tries to spawn the daemon. `SteamOS`'s own
/// `wheel-prepare-oobe-test` uses the same trick.
const SUDOERS_PATH: &str = "/etc/sudoers.d/zz-network-deck";

/// Sudoers paths from older releases that we should remove on install or
/// uninstall, so a user upgrading doesn't end up with a stale rule that
/// hits the same ordering bug. Leave entries here forever; they're cheap.
const LEGACY_SUDOERS_PATHS: &[&str] = &["/etc/sudoers.d/network-deck"];

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
    if !is_valid_username(&user) {
        eprintln!("install: refusing untrusted SUDO_USER={user:?}");
        std::process::exit(1);
    }
    let Some(home) = home_for(&user) else {
        eprintln!("install: user {user:?} not found in passwd");
        std::process::exit(1);
    };
    let ctx = InstallContext::production(&home);

    eprintln!(">> install user={user} home={}", home.display());
    eprintln!(">> install dir={}", ctx.install_dir.display());

    ensure_usbip_userspace()?;
    load_kernel_modules()?;
    enable_usbipd();
    disable_old_systemd_unit();
    copy_self_to(&ctx.install_bin)?;
    chown(&ctx.install_dir, "root", "root")?;
    chmod(&ctx.install_dir, 0o755)?;
    // Plain 0o755. Setuid root was removed: paired with bare-name
    // Command::new() calls inside the daemon, it lets any local user
    // hijack PATH and execute as root. The sudoers NOPASSWD entry below
    // is the sole privilege-escalation path.
    chmod(&ctx.install_bin, 0o755)?;
    // Write all files (sudoers, modules-load, .desktop, remove legacy sudoers).
    // chmod/chown and visudo validation happen after for the sudoers file.
    eprintln!(">> Writing {}", ctx.sudoers_path.display());
    eprintln!(">> Writing {}", ctx.modules_load_path.display());
    eprintln!(">> Writing {}", ctx.desktop_path.display());
    install_files(&ctx)?;
    // Secure the sudoers file and validate it after install_files writes it.
    write_sudoers_post_files(&ctx.sudoers_path)?;
    // chown the desktop entry and app dir to the user.
    chown_warn(&ctx.app_dir, &user, &user);
    chown_warn(&ctx.desktop_path, &user, &user);

    eprintln!();
    eprintln!("Done.");
    eprintln!();
    eprintln!("Next: pair with your Windows PC (run on each side at once):");
    eprintln!("  {} pair", ctx.install_bin.display());
    eprintln!("  client-win.exe pair    # on Windows");
    eprintln!();
    eprintln!("Use:");
    eprintln!("  Add {} to Steam as a non-Steam game", ctx.install_bin.display());
    eprintln!("  (one-time, in Desktop Mode). Tap from Game Mode to start;");
    eprintln!("  closing the window stops the daemon and unbinds the controller.");
    Ok(())
}

/// Reverse of `run`: remove every file and unit `install` touched, in the
/// reverse order so a partial uninstall leaves the system in a sane shape.
/// Idempotent — missing files / disabled units are not errors.
///
/// Deliberately leaves alone:
///   - the `usbip` userspace package (pacman-installed; user can keep it).
///   - the trust file + identity in `$HOME/.local/state/network-deck`.
///     That's user data, not install state. `rm -rf` it manually if you
///     want a fully clean test slate.
#[allow(clippy::unnecessary_wraps)]
pub fn uninstall() -> std::io::Result<()> {
    if !is_root() {
        eprintln!("uninstall must be run as root: sudo network-deck uninstall");
        std::process::exit(1);
    }
    let user = std::env::var("SUDO_USER").unwrap_or_else(|_| "deck".to_owned());
    let home = if is_valid_username(&user) {
        home_for(&user)
    } else {
        None
    };

    eprintln!(">> Disabling usbipd.service");
    if !run_ok("systemctl", &["disable", "--now", "usbipd.service"]) {
        eprintln!("warning: systemctl disable usbipd.service failed (already off?)");
    }

    // Build a context: if home lookup failed, use a placeholder that points
    // nowhere real so uninstall_files still cleans the system paths.
    let home_path = home.unwrap_or_else(|| PathBuf::from("/nonexistent"));
    let ctx = InstallContext::production(&home_path);

    eprintln!(">> Removing {}", ctx.sudoers_path.display());
    eprintln!(">> Removing {}", ctx.modules_load_path.display());
    eprintln!(">> Removing {}", ctx.install_dir.display());
    eprintln!(">> Removing {}", ctx.desktop_path.display());

    if let Err(e) = uninstall_files(&ctx) {
        eprintln!("warning: uninstall_files: {e}");
    }

    eprintln!();
    eprintln!("Done. The trust file + identity were preserved under");
    eprintln!("  ~/.local/state/network-deck/");
    eprintln!("Remove that directory manually for a fully clean slate.");
    Ok(())
}

#[cfg(target_os = "linux")]
fn is_root() -> bool {
    // SAFETY: getuid() is signal-safe and trivially correct.
    unsafe { libc::getuid() == 0 }
}

#[cfg(not(target_os = "linux"))]
fn is_root() -> bool {
    false
}

/// Allow `^[a-z0-9_][a-z0-9_-]*$`. The classic POSIX rule reserves leading
/// digits, but `SteamOS` Family Share creates per-account local users with
/// fully-numeric names (e.g. `496325425`), so refusing them locks those
/// users out of install. The constraint that matters for sudoers safety
/// — no whitespace, no newlines, no shell or sudoers metas — still holds.
#[must_use]
pub fn is_valid_username(user: &str) -> bool {
    if user.is_empty() || user.len() > 32 {
        return false;
    }
    let mut chars = user.chars();
    let Some(first) = chars.next() else { return false };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn home_for(user: &str) -> Option<PathBuf> {
    let getent = absolute_path_for("getent")?;
    Command::new(getent)
        .args(["passwd", user])
        .output()
        .ok()
        .and_then(|out| if out.status.success() { Some(out.stdout) } else { None })
        .and_then(|stdout| {
            let line = std::str::from_utf8(&stdout).ok()?.trim_end().to_owned();
            line.split(':').nth(5).map(PathBuf::from)
        })
}

#[allow(clippy::unnecessary_wraps)]
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

#[allow(clippy::unnecessary_wraps)]
fn load_kernel_modules() -> std::io::Result<()> {
    eprintln!(">> Loading kernel modules...");
    for m in ["usbip-core", "usbip-host", "vhci-hcd"] {
        if !run_ok("modprobe", &[m]) {
            eprintln!("modprobe {m} failed (module may already be loaded; continuing)");
        }
    }
    Ok(())
}

fn enable_usbipd() {
    eprintln!(">> Enabling usbipd.service...");
    if !run_ok("systemctl", &["enable", "--now", "usbipd.service"]) {
        eprintln!("warning: enable usbipd.service failed");
    }
}

fn disable_old_systemd_unit() {
    let unit = "network-deck-server.service";
    let Some(systemctl) = absolute_path_for("systemctl") else { return };
    let exists = Command::new(systemctl)
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

/// The contents of `/etc/sudoers.d/network-deck` as written by `install`.
/// Pure — no I/O. Tested in `mod tests`. The visudo validation step in
/// `write_sudoers` will reject anything malformed at install time, but
/// the unit tests here pin the structure (user-agnostic ALL rule,
/// NOPASSWD bound to the daemon subcommand, comment block intact).
#[must_use]
pub(super) fn sudoers_body(install_bin: &Path) -> String {
    format!(
        "# Allow any local user to launch the network-deck daemon without a\n\
         # password prompt. SteamOS Family Share + Game Mode users vary, and\n\
         # pkexec is unreliable in gamescope-session, so user-keyed rules\n\
         # silently lock those accounts out of the kiosk. The grant is bound\n\
         # to a root-owned binary at a fixed path (see install.rs).\n\
         ALL ALL=(root) NOPASSWD: {bin} daemon\n",
        bin = install_bin.display(),
    )
}

/// chmod 0o440 + visudo validation step, run after `install_files` has
/// already written the sudoers file. Keeps sudo-breaking partial states
/// off disk: any failure removes the file before propagating.
///
/// The file-write itself is in `install_files` so it can be exercised in
/// tempdir tests without needing root or visudo on the test host.
#[allow(clippy::unnecessary_wraps)]
fn write_sudoers_post_files(sudoers_path: &std::path::Path) -> std::io::Result<()> {
    // Wrap chmod + visudo so any failure removes the partial file before
    // propagating. A broken file under /etc/sudoers.d/ makes sudo refuse
    // to load any rules in the directory — users get "not in sudoers"
    // with no recovery path.
    let result = (|| -> std::io::Result<()> {
        chmod(sudoers_path, 0o440)?;
        let path_str = sudoers_path.display().to_string();
        if !run_ok("visudo", &["-c", "-f", &path_str]) {
            return Err(std::io::Error::other(format!(
                "visudo validation failed for {}",
                sudoers_path.display()
            )));
        }
        Ok(())
    })();
    if let Err(e) = result {
        eprintln!("{e}");
        let _ = std::fs::remove_file(sudoers_path);
        eprintln!("removed {} to keep sudo functional", sudoers_path.display());
        std::process::exit(1);
    }
    Ok(())
}

/// The contents of `network-deck-kiosk.desktop` as written by `install`.
/// Pure — no I/O. Tested in `mod tests`. Pins the Exec line to the
/// install-time binary path so the kiosk launches the same root-owned
/// binary that the sudoers grant trusts.
#[must_use]
pub(super) fn desktop_body(install_bin: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Network Deck\n\
         Comment=Wireless controller bridge to PC\n\
         Exec={bin}\n\
         Icon=input-gaming\n\
         Terminal=false\n\
         Categories=Game;\n",
        bin = install_bin.display(),
    )
}

/// Hard-failing chown: returns Err on subprocess failure. Used for
/// security-bearing paths (anything whose ownership the sudoers grant
/// relies on); a silent failure there means the install looks succeeded
/// while leaving a hijackable binary or directory.
fn chown(path: &std::path::Path, user: &str, group: &str) -> std::io::Result<()> {
    if !run_ok("chown", &[&format!("{user}:{group}"), &path.display().to_string()]) {
        return Err(std::io::Error::other(format!(
            "chown {user}:{group} {} failed",
            path.display()
        )));
    }
    Ok(())
}

/// Hard-failing chmod: returns Err on subprocess failure. Critical for
/// the sudoers file (must be 0o440 for sudo to load it — wrong mode and
/// sudo silently refuses) and the install dir/binary (root-owned 0o755
/// keeps the deck user from swapping the binary out under NOPASSWD).
fn chmod(path: &std::path::Path, mode: u32) -> std::io::Result<()> {
    if !run_ok("chmod", &[&format!("{mode:o}"), &path.display().to_string()]) {
        return Err(std::io::Error::other(format!(
            "chmod {mode:o} {} failed",
            path.display()
        )));
    }
    Ok(())
}

/// Warn-only chown for non-security-bearing paths (user-owned desktop
/// entries etc.). A failure here doesn't compromise install integrity,
/// so we don't unwind a successful sudoers + binary install over it.
fn chown_warn(path: &std::path::Path, user: &str, group: &str) {
    if !run_ok("chown", &[&format!("{user}:{group}"), &path.display().to_string()]) {
        eprintln!("warning: chown {user}:{group} {} failed", path.display());
    }
}

/// Resolve a privileged tool name to an absolute path on `KNOWN_BIN_DIRS`.
/// Returns `None` if no candidate exists. Bare-name `Command::new(...)` calls
/// would inherit the caller's `$PATH`, which is attacker-controlled.
const KNOWN_BIN_DIRS: &[&str] = &["/usr/sbin", "/usr/bin", "/sbin", "/bin"];

#[must_use]
pub fn absolute_path_for(cmd: &str) -> Option<PathBuf> {
    for dir in KNOWN_BIN_DIRS {
        let candidate = Path::new(dir).join(cmd);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn run_ok(cmd: &str, args: &[&str]) -> bool {
    let Some(abs) = absolute_path_for(cmd) else {
        eprintln!("warning: {cmd} not found in any of {KNOWN_BIN_DIRS:?}");
        return false;
    };
    Command::new(abs)
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn which_present(cmd: &str) -> bool {
    absolute_path_for(cmd).is_some()
}

#[must_use]
pub fn is_installed() -> bool {
    std::path::Path::new(SUDOERS_PATH).exists()
}

/// Root-owned prefix trees that are safe install sources on every supported distro.
/// `/usr/local/` is intentionally absent: on Arch / `SteamOS`-derived distros it's
/// often user-writable for ad-hoc installs, which would defeat the check.
const SAFE_PREFIXES: &[&str] = &["/usr/bin/", "/usr/sbin/", "/usr/lib/", "/usr/libexec/"];

/// `true` iff `path` sits in a tree that's root-owned on every supported
/// distro. Used to gate `pkexec` invocations: a user-writable tree means
/// any local user can trojan the binary and ride the elevation.
///
/// Excludes `/usr/local/`: on Arch / `SteamOS`-derived distros it's often
/// user-writable for ad-hoc installs, which would defeat the check.
#[must_use]
pub fn is_safe_install_source(path: &Path) -> bool {
    if path == Path::new(INSTALL_BIN) {
        return true;
    }
    SAFE_PREFIXES.iter().any(|p| path.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_usernames_accepted() {
        assert!(is_valid_username("deck"));
        assert!(is_valid_username("_admin"));
        assert!(is_valid_username("a"));
        assert!(is_valid_username("user-1"));
        assert!(is_valid_username("u_2"));
        // SteamOS Family Share accounts.
        assert!(is_valid_username("496325425"));
        assert!(is_valid_username("1user"));
    }

    #[test]
    fn invalid_usernames_rejected() {
        assert!(!is_valid_username(""));
        assert!(!is_valid_username("-user"));
        assert!(!is_valid_username("Deck"));
        assert!(!is_valid_username("deck\nroot ALL=(ALL) NOPASSWD:ALL"));
        assert!(!is_valid_username("user name"));
        assert!(!is_valid_username("user$"));
        assert!(!is_valid_username("a".repeat(33).as_str()));
    }

    #[test]
    fn sudoers_body_grants_only_daemon_subcommand_to_all_users() {
        let body = sudoers_body(Path::new("/var/lib/network-deck/network-deck"));
        // The whole point of the user-agnostic rule.
        assert!(body.contains("ALL ALL=(root) NOPASSWD: /var/lib/network-deck/network-deck daemon\n"));
        // The grant must NOT extend to other subcommands (`pair`, `install`,
        // `uninstall`) — pairing runs in-process as the user, install/uninstall
        // require explicit sudo. A wildcard like `*` here would silently broaden
        // the privilege.
        assert!(!body.contains("NOPASSWD: ALL"));
        assert!(!body.contains("daemon *"));
    }

    #[test]
    fn sudoers_body_ends_with_newline() {
        // sudo ignores the last line if it has no terminating LF, so a
        // missing newline silently breaks the install. Pin it.
        let body = sudoers_body(Path::new("/var/lib/network-deck/network-deck"));
        assert!(body.ends_with('\n'), "got: {body:?}");
    }

    #[test]
    fn sudoers_body_substitutes_install_bin_path() {
        let body = sudoers_body(Path::new("/usr/local/bin/nd"));
        assert!(body.contains("/usr/local/bin/nd daemon"), "got: {body}");
    }

    #[test]
    fn desktop_body_has_required_desktop_entry_keys() {
        let body = desktop_body(Path::new("/var/lib/network-deck/network-deck"));
        assert!(body.starts_with("[Desktop Entry]\n"));
        for required in ["Type=Application", "Name=", "Exec=", "Categories="] {
            assert!(body.contains(required), "missing {required:?} in: {body}");
        }
    }

    #[test]
    fn desktop_body_exec_points_at_install_bin() {
        let body = desktop_body(Path::new("/opt/nd/bin"));
        assert!(body.contains("Exec=/opt/nd/bin\n"), "got: {body}");
    }

    #[test]
    fn desktop_body_terminal_false_so_kiosk_does_not_open_konsole() {
        // If Terminal=true, KDE/Plasma launches the kiosk inside a Konsole
        // window — visible breakage in Game Mode where there is no terminal
        // emulator. Pin the flag.
        let body = desktop_body(Path::new("/var/lib/network-deck/network-deck"));
        assert!(body.contains("Terminal=false"), "got: {body}");
    }

    #[test]
    fn modules_load_body_lists_all_three_usbip_modules() {
        // Lost from the constant means usbip-host won't auto-load on boot;
        // the daemon then fails its first bind. Pin the list.
        for m in ["usbip-core", "usbip-host", "vhci-hcd"] {
            assert!(MODULES_LOAD_BODY.contains(m), "missing {m} in {MODULES_LOAD_BODY:?}");
        }
        assert!(MODULES_LOAD_BODY.ends_with('\n'));
    }

    #[test]
    fn is_safe_install_source_accepts_canonical_root_owned_dirs() {
        for ok in [
            "/usr/bin/network-deck",
            "/usr/sbin/foo",
            "/usr/lib/some/binary",
            "/usr/libexec/whatever",
        ] {
            assert!(is_safe_install_source(Path::new(ok)), "should accept: {ok}");
        }
    }

    #[test]
    fn is_safe_install_source_accepts_install_bin_exactly() {
        // The install destination itself is always safe (root-chowned during install).
        assert!(is_safe_install_source(Path::new(INSTALL_BIN)));
    }

    #[test]
    fn is_safe_install_source_rejects_user_writable_locations() {
        for bad in [
            "/tmp/network-deck",
            "/home/deck/network-deck",
            "/var/tmp/foo",
            // Per the doc-comment, /usr/local is excluded on Arch / SteamOS
            // because it's commonly user-writable.
            "/usr/local/bin/network-deck",
            "/dev/shm/x",
        ] {
            assert!(!is_safe_install_source(Path::new(bad)), "should reject: {bad}");
        }
    }

    #[test]
    fn absolute_path_for_returns_none_for_impossible_name() {
        // No command on PATH should plausibly be named like this; verifies
        // the lookup loop terminates with None on every supported host.
        assert!(absolute_path_for("definitely-not-a-real-binary-xyzzy123").is_none());
    }

    #[test]
    fn sudoers_path_sorts_after_wheel_files_on_steamos() {
        // SteamOS ships /etc/sudoers.d/wheel with `%wheel ALL=(ALL) ALL`
        // (no NOPASSWD). Sudoers' last-match-wins eval means our NOPASSWD
        // line is only effective if our file loads ALPHABETICALLY AFTER
        // every `wheel*` file. Pin the prefix so a refactor can't drop us
        // back into the override zone.
        let leaf = Path::new(SUDOERS_PATH).file_name().unwrap().to_str().unwrap();
        assert!(leaf > "wheel-zzz", "got {leaf:?} — must sort after wheel*");
        // Plus a hard string check so we don't drift to e.g. `zz_network-deck`
        // (underscore < dash, would still sort after `wheel-*` but is a
        // surprise waiting to bite if we ever change wheel-prepare-oobe-test
        // patterns).
        assert_eq!(SUDOERS_PATH, "/etc/sudoers.d/zz-network-deck");
    }

    #[test]
    fn legacy_sudoers_paths_includes_original_v0_filename() {
        // Anyone upgrading from a release before this rename has a stale
        // /etc/sudoers.d/network-deck file. Install + uninstall both clean
        // it via remove_legacy_sudoers; pin the entry so a careless edit
        // can't drop migrations on the floor.
        assert!(
            LEGACY_SUDOERS_PATHS.contains(&"/etc/sudoers.d/network-deck"),
            "got {LEGACY_SUDOERS_PATHS:?} — must include the v0 filename",
        );
    }

    #[test]
    fn sudoers_path_is_not_the_legacy_path() {
        // Sanity: the install destination and the legacy migration list
        // must not overlap, or install would write+then-remove its own
        // file. Cheap regression guard.
        assert!(
            !LEGACY_SUDOERS_PATHS.contains(&SUDOERS_PATH),
            "SUDOERS_PATH {SUDOERS_PATH:?} must not also be in LEGACY_SUDOERS_PATHS",
        );
    }

    // ── tempdir-based install/uninstall round-trip tests ─────────────────────

    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Build a self-contained tempdir-rooted `InstallContext` mimicking the
    /// production layout. Caller holds the `TempDir` to keep it alive.
    fn ctx_in(td: &TempDir) -> InstallContext {
        let root = td.path();
        InstallContext {
            sudoers_path: root.join("etc/sudoers.d/zz-network-deck"),
            install_dir: root.join("var/lib/network-deck"),
            install_bin: root.join("var/lib/network-deck/network-deck"),
            modules_load_path: root.join("etc/modules-load.d/usbip.conf"),
            app_dir: root.join("home/deck/.local/share/applications"),
            desktop_path: root
                .join("home/deck/.local/share/applications/network-deck-kiosk.desktop"),
            legacy_sudoers_paths: vec![root.join("etc/sudoers.d/network-deck")],
        }
    }

    #[test]
    fn install_files_writes_full_layout_into_tempdir() {
        let td = tempfile::tempdir().unwrap();
        let ctx = ctx_in(&td);
        install_files(&ctx).unwrap();

        let sudoers = std::fs::read_to_string(&ctx.sudoers_path).unwrap();
        assert!(sudoers.contains("ALL ALL=(root) NOPASSWD:"));
        assert!(sudoers.contains(ctx.install_bin.to_str().unwrap()));

        let modules = std::fs::read_to_string(&ctx.modules_load_path).unwrap();
        for m in ["usbip-core", "usbip-host", "vhci-hcd"] {
            assert!(modules.contains(m), "missing {m} in: {modules}");
        }

        let desktop = std::fs::read_to_string(&ctx.desktop_path).unwrap();
        assert!(desktop.starts_with("[Desktop Entry]\n"));
        assert!(desktop.contains(&format!("Exec={}", ctx.install_bin.display())));

        // install_dir created (but binary self-copy is NOT done by
        // install_files — that's a `run` shellout).
        assert!(ctx.install_dir.is_dir(), "install_dir not created");
    }

    #[test]
    fn install_files_is_idempotent() {
        let td = tempfile::tempdir().unwrap();
        let ctx = ctx_in(&td);

        install_files(&ctx).unwrap();
        let sudoers_first = std::fs::read_to_string(&ctx.sudoers_path).unwrap();

        install_files(&ctx).unwrap();
        let sudoers_second = std::fs::read_to_string(&ctx.sudoers_path).unwrap();

        assert_eq!(sudoers_first, sudoers_second, "re-run must be byte-identical");

        // Only one sudoers file under etc/sudoers.d/ — idempotency must not
        // create a duplicate at a different path.
        let sudoers_dir = ctx.sudoers_path.parent().unwrap();
        let entries: Vec<_> = std::fs::read_dir(sudoers_dir).unwrap().collect();
        assert_eq!(entries.len(), 1, "expected exactly 1 sudoers file, got {entries:?}");
    }

    #[test]
    fn install_files_removes_legacy_sudoers_path() {
        let td = tempfile::tempdir().unwrap();
        let ctx = ctx_in(&td);
        let legacy = ctx.legacy_sudoers_paths[0].clone();

        // Pre-create a stale legacy sudoers file.
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, "stale content from older release\n").unwrap();
        assert!(legacy.exists());

        install_files(&ctx).unwrap();

        assert!(!legacy.exists(), "install_files must remove legacy sudoers");
        assert!(ctx.sudoers_path.exists(), "new sudoers must be present");
    }

    #[test]
    fn install_files_legacy_removal_skips_when_legacy_equals_canonical() {
        // Defense-in-depth: if a future maintainer accidentally lists the
        // canonical path among legacies, install_files must NOT remove the
        // file it just wrote. The skip is implemented; lock the behaviour
        // against regression.
        let td = tempfile::tempdir().unwrap();
        let mut ctx = ctx_in(&td);
        ctx.legacy_sudoers_paths.push(ctx.sudoers_path.clone());

        install_files(&ctx).unwrap();
        assert!(
            ctx.sudoers_path.exists(),
            "canonical sudoers path must survive even if listed as legacy",
        );
    }

    #[test]
    fn uninstall_files_removes_everything_install_wrote() {
        let td = tempfile::tempdir().unwrap();
        let ctx = ctx_in(&td);
        install_files(&ctx).unwrap();
        assert!(ctx.sudoers_path.exists());
        assert!(ctx.modules_load_path.exists());
        assert!(ctx.desktop_path.exists());
        assert!(ctx.install_dir.is_dir());

        uninstall_files(&ctx).unwrap();

        assert!(!ctx.sudoers_path.exists(), "sudoers should be gone");
        assert!(!ctx.modules_load_path.exists(), "modules-load should be gone");
        assert!(!ctx.desktop_path.exists(), ".desktop should be gone");
        assert!(!ctx.install_dir.exists(), "install_dir should be gone");
    }

    #[test]
    fn uninstall_files_removes_legacy_sudoers_even_after_migration() {
        let td = tempfile::tempdir().unwrap();
        let ctx = ctx_in(&td);
        let legacy = ctx.legacy_sudoers_paths[0].clone();

        // Simulate a half-migrated state: legacy file exists, new one
        // doesn't (someone manually nuked it). Uninstall must still clean
        // the legacy.
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, "stale\n").unwrap();
        assert!(legacy.exists());

        uninstall_files(&ctx).unwrap();
        assert!(!legacy.exists(), "uninstall must remove legacy sudoers");
    }

    #[test]
    fn install_uninstall_round_trip_leaves_no_files() {
        let td = tempfile::tempdir().unwrap();
        let ctx = ctx_in(&td);

        install_files(&ctx).unwrap();
        uninstall_files(&ctx).unwrap();

        for p in [
            &ctx.sudoers_path,
            &ctx.modules_load_path,
            &ctx.desktop_path,
        ] {
            assert!(!p.exists(), "{} should not exist", p.display());
        }
        assert!(!ctx.install_dir.exists());
    }

    #[test]
    fn uninstall_files_is_idempotent_on_clean_tempdir() {
        let td = tempfile::tempdir().unwrap();
        let ctx = ctx_in(&td);
        // No install — just call uninstall on a clean tempdir.
        uninstall_files(&ctx).unwrap();
        // And again — must still not error.
        uninstall_files(&ctx).unwrap();
    }

    #[test]
    fn production_context_uses_canonical_paths() {
        let home = PathBuf::from("/home/deck");
        let ctx = InstallContext::production(&home);
        assert_eq!(ctx.sudoers_path, PathBuf::from(SUDOERS_PATH));
        assert_eq!(ctx.install_dir, PathBuf::from(INSTALL_DIR));
        assert_eq!(ctx.install_bin, PathBuf::from(INSTALL_BIN));
        assert_eq!(ctx.modules_load_path, PathBuf::from(MODULES_LOAD_PATH));
        assert_eq!(
            ctx.desktop_path,
            home.join(".local/share/applications/network-deck-kiosk.desktop"),
        );
        assert!(
            ctx.legacy_sudoers_paths
                .iter()
                .any(|p| p == Path::new("/etc/sudoers.d/network-deck")),
            "production context must include the v0 sudoers path for migration",
        );
    }
}
