//! Daemon-side hotkey listener for "release controller temporarily".
//!
//! Scans `/dev/input/event*` once at daemon startup. For each device that
//! advertises any of our recognised chord keys, spawns a thread that reads
//! input events and toggles the `paused` flag in the control dir when the
//! chord is held simultaneously.
//!
//! Chord (toggles paused):
//!   * `KEY_VOLUMEUP` + `KEY_VOLUMEDOWN` — volume side buttons. Lives on a
//!     separate ACPI/i2c-hid device that `usbip-host` doesn't touch, so
//!     the chord still fires while the controller is bridged.
//!
//! (Steam + QAM was tried and discarded — those buttons come through the
//! controller HID, which the bridge owns mid-session, so the daemon never
//! sees them.)
//!
//! Threads are detached. They terminate when the daemon process exits.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use evdev::{Device, EventSummary, KeyCode};

/// Pairs of keys whose simultaneous press toggles paused state.
const CHORDS: &[(KeyCode, KeyCode)] = &[
    (KeyCode::KEY_VOLUMEUP, KeyCode::KEY_VOLUMEDOWN),
];

/// Debounce: ignore another toggle until this much time has passed since
/// the last one. Prevents rapid flicker while the chord is held.
const TOGGLE_COOLDOWN: Duration = Duration::from_millis(800);

pub fn spawn(control_dir: PathBuf) {
    let entries = match std::fs::read_dir("/dev/input") {
        Ok(e) => e,
        Err(e) => {
            eprintln!("hotkey: read /dev/input: {e}");
            return;
        }
    };
    let mut watched_count = 0_usize;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else { continue };
        if !name.starts_with("event") { continue; }
        let dev = match Device::open(&path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let supported = match dev.supported_keys() {
            Some(k) => k,
            None => continue,
        };
        let watched: Vec<(KeyCode, KeyCode)> = CHORDS
            .iter()
            .filter(|(a, b)| supported.contains(*a) && supported.contains(*b))
            .copied()
            .collect();
        if watched.is_empty() { continue; }
        watched_count += 1;
        let dev_name = dev.name().unwrap_or("?").to_owned();
        eprintln!(
            "hotkey: watching {} ({}): {} chord(s)",
            path.display(),
            dev_name,
            watched.len(),
        );
        let cd = control_dir.clone();
        let _ = std::thread::Builder::new()
            .name("hotkey-listener".into())
            .spawn(move || run_listener(dev, watched, cd));
    }
    if watched_count == 0 {
        eprintln!("hotkey: no input device exposes any of the configured chords");
    }
}

fn run_listener(mut dev: Device, chords: Vec<(KeyCode, KeyCode)>, control_dir: PathBuf) {
    use std::collections::HashSet;
    let mut pressed: HashSet<KeyCode> = HashSet::new();
    let mut last_toggle = Instant::now() - TOGGLE_COOLDOWN;

    loop {
        let events = match dev.fetch_events() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("hotkey: fetch_events failed: {e}; thread exiting");
                return;
            }
        };
        for ev in events {
            if let EventSummary::Key(_, key, value) = ev.destructure() {
                match value {
                    1 => { pressed.insert(key); }
                    0 => { pressed.remove(&key); }
                    _ => {} // 2 = autorepeat — keep state as-is.
                }
            }
        }

        for (a, b) in &chords {
            if pressed.contains(a) && pressed.contains(b)
                && last_toggle.elapsed() >= TOGGLE_COOLDOWN
            {
                toggle_paused(&control_dir);
                last_toggle = Instant::now();
                break;
            }
        }
    }
}

fn toggle_paused(control_dir: &Path) {
    let path = control_dir.join("paused");
    if path.exists() {
        match std::fs::remove_file(&path) {
            Ok(()) => eprintln!("hotkey: resumed"),
            Err(e) => eprintln!("hotkey: remove paused: {e}"),
        }
    } else {
        match std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
        {
            Ok(_) => eprintln!("hotkey: paused"),
            Err(e) => eprintln!("hotkey: create paused: {e}"),
        }
    }
}
