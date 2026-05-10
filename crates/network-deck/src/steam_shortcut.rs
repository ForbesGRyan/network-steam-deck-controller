//! Auto-add the kiosk binary to Steam as a non-Steam game.
//!
//! Steam stores non-Steam shortcuts in a binary `KeyValues` file at
//! `~/.steam/steam/userdata/<SteamID3>/config/shortcuts.vdf`. There is no
//! live API: edits are picked up on the next Steam restart. This module
//! reads the file (or creates an empty one), inserts/updates a single
//! entry pinned to our install path, and writes it back.
//!
//! Pure functions for the binary VDF codec live at the bottom and have
//! their own tempdir-free unit tests; the public entrypoints
//! `find_userdata_dirs` and `add_to_all_userdata` are thin wrappers that
//! shell that codec into the on-disk Steam tree.
//!
//! Failure mode: if Steam is running while we write, Steam may overwrite
//! shortcuts.vdf on exit. We don't try to detect this — the user is
//! instructed in the setup-done screen to (re)start Steam, which gives
//! them a clean window where our write sticks. Idempotent: a re-run with
//! the same Exe path replaces the existing entry rather than duplicating.
//!
//! This file deliberately has no `eframe`/`egui`/Linux-only deps so it
//! compiles and unit-tests on any host.
//!
//! [Steam binary VDF reference]:
//!   <https://developer.valvesoftware.com/wiki/Steam_Browser_Protocol>
//!   The format is a small key-value tree: 0x00 = nested object, 0x01 =
//!   string, 0x02 = int32 LE, 0x08 = end-of-object. Unknown types are
//!   rejected so we don't silently lose data on unfamiliar files.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Description of the non-Steam shortcut we want Steam to launch.
#[derive(Debug, Clone)]
pub struct ShortcutSpec {
    /// Absolute path to the binary Steam should run. Must match the
    /// installed root-owned path so Game Mode can re-launch it after a
    /// reboot regardless of where the kiosk was first invoked from.
    pub exe: PathBuf,
    /// Display name in the Steam library.
    pub app_name: String,
    /// Working directory Steam should chdir to before launch.
    pub start_dir: PathBuf,
    /// Optional collection tags ("Network Deck", "Utilities", …). Empty
    /// vec means no tags. Steam picks them up as a flat list under the
    /// `tags` nested object.
    pub tags: Vec<String>,
}

/// Outcome of writing one userdata directory's shortcuts.vdf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// File didn't exist (or had no entries) and we created/seeded it.
    Created,
    /// Existing file had no entry with our `Exe`; we appended one.
    Added,
    /// Existing entry matched our `Exe`; we replaced it in place.
    Updated,
}

/// Top-level result for a userdata directory.
#[derive(Debug, Clone)]
pub struct UserdataResult {
    pub config_dir: PathBuf,
    pub outcome: WriteOutcome,
}

/// Locate every `<home>/.steam/steam/userdata/<id>/config/` directory.
///
/// Steam allows multiple accounts on one machine, each with its own
/// shortcuts file. We touch all of them — the user picked the checkbox
/// once, so adding to "the account I'll log into" is the conservative
/// interpretation. Returns an empty Vec if Steam has never been launched
/// (the userdata tree only exists after first login).
pub fn find_userdata_dirs(home: &Path) -> Vec<PathBuf> {
    let userdata = home.join(".steam").join("steam").join("userdata");
    let Ok(entries) = fs::read_dir(&userdata) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        // Userdata accounts are named with the SteamID3 (decimal). Filter
        // to all-digit names so non-account directories ("anonymous",
        // "__steamcmd__", crash dumps, etc.) don't get phantom entries.
        // Also drop the "0" template directory: it's a Steam scratch slot
        // shared across logins, never the user's actual account.
        .filter(|e| {
            let name = e.file_name();
            let Some(s) = name.to_str() else { return false };
            s != "0" && !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
        })
        .map(|e| e.path().join("config"))
        .collect()
}

/// Add (or replace) the shortcut in every userdata directory under
/// `home`. Each directory is handled independently — a failure on one
/// is reported in the returned Vec but does not abort the others.
///
/// Empty result Vec ⇒ no Steam userdata at all (user has not signed
/// into Steam yet). The setup screen surfaces that as a hint.
pub fn add_to_all_userdata(
    home: &Path,
    spec: &ShortcutSpec,
) -> Vec<(PathBuf, io::Result<WriteOutcome>)> {
    find_userdata_dirs(home)
        .into_iter()
        .map(|config_dir| {
            let path = config_dir.join("shortcuts.vdf");
            let res = upsert_shortcut(&path, spec);
            (config_dir, res)
        })
        .collect()
}

/// Read `path`, upsert our shortcut, write back. Creates the parent
/// directory and an empty shortcuts file if neither exists. Atomic:
/// writes to a sibling tempfile and renames into place.
pub fn upsert_shortcut(path: &Path, spec: &ShortcutSpec) -> io::Result<WriteOutcome> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let (mut entries, base_outcome) = match fs::read(path) {
        Ok(bytes) if !bytes.is_empty() => (parse_shortcuts(&bytes)?, None),
        Ok(_) => (Vec::new(), Some(WriteOutcome::Created)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => (Vec::new(), Some(WriteOutcome::Created)),
        Err(e) => return Err(e),
    };

    let exe_str = exe_to_string(&spec.exe);
    let target = build_entry(spec, &exe_str);

    let outcome = if let Some(o) = base_outcome {
        entries.push(target);
        o
    } else if let Some(idx) = entries.iter().position(|e| entry_exe(e) == Some(&exe_str)) {
        entries[idx] = target;
        WriteOutcome::Updated
    } else {
        entries.push(target);
        WriteOutcome::Added
    };

    let bytes = serialize_shortcuts(&entries);

    let tmp = path.with_extension("vdf.tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(outcome)
}

/// Steam's "legacy" 32-bit non-Steam shortcut id. Used for the `appid`
/// field (cast to i32, often appears negative) and as the basename for
/// grid artwork at `userdata/<id>/config/grid/<appid>.png`.
///
/// Steam: `crc32_ieee(exe + app_name) | 0x8000_0000`.
#[must_use]
pub fn shortcut_appid(exe: &str, app_name: &str) -> u32 {
    let mut input = Vec::with_capacity(exe.len() + app_name.len());
    input.extend_from_slice(exe.as_bytes());
    input.extend_from_slice(app_name.as_bytes());
    crc32_ieee(&input) | 0x8000_0000
}

fn exe_to_string(exe: &Path) -> String {
    // Steam stores Exe wrapped in double quotes on every platform we care
    // about. The unquoted form launches under common Linux Steam builds
    // too, but quoting matches what the official "Add a Non-Steam Game"
    // dialog produces — keeps round-trips through Steam's own UI clean.
    format!("\"{}\"", exe.display())
}

// ── VDF entry construction ───────────────────────────────────────────────

fn build_entry(spec: &ShortcutSpec, exe_str: &str) -> Vec<(String, Value)> {
    let appid = shortcut_appid(exe_str, &spec.app_name);
    // Steam writes appid as a signed int32; reinterpret the high-bit-set
    // u32 as i32 so the bit pattern survives round-trip.
    let appid_i32 = appid.cast_signed();

    let tags: Vec<(String, Value)> = spec
        .tags
        .iter()
        .enumerate()
        .map(|(i, t)| (i.to_string(), Value::Str(t.clone())))
        .collect();

    vec![
        ("appid".into(), Value::Int(appid_i32)),
        ("AppName".into(), Value::Str(spec.app_name.clone())),
        ("Exe".into(), Value::Str(exe_str.to_owned())),
        (
            "StartDir".into(),
            Value::Str(format!("\"{}\"", spec.start_dir.display())),
        ),
        ("icon".into(), Value::Str(String::new())),
        ("ShortcutPath".into(), Value::Str(String::new())),
        ("LaunchOptions".into(), Value::Str(String::new())),
        ("IsHidden".into(), Value::Int(0)),
        ("AllowDesktopConfig".into(), Value::Int(1)),
        ("AllowOverlay".into(), Value::Int(1)),
        ("OpenVR".into(), Value::Int(0)),
        ("Devkit".into(), Value::Int(0)),
        ("DevkitGameID".into(), Value::Str(String::new())),
        ("DevkitOverrideAppID".into(), Value::Int(0)),
        ("LastPlayTime".into(), Value::Int(0)),
        ("FlatpakAppID".into(), Value::Str(String::new())),
        ("tags".into(), Value::Object(tags)),
    ]
}

fn entry_exe(entry: &[(String, Value)]) -> Option<&str> {
    entry.iter().find_map(|(k, v)| {
        if k.eq_ignore_ascii_case("Exe") {
            if let Value::Str(s) = v {
                Some(s.as_str())
            } else {
                None
            }
        } else {
            None
        }
    })
}

// ── Binary VDF codec ────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Value {
    Object(Vec<(String, Value)>),
    Str(String),
    Int(i32),
}

const TY_OBJECT: u8 = 0x00;
const TY_STR: u8 = 0x01;
const TY_INT: u8 = 0x02;
const END: u8 = 0x08;

/// Parse the entry list out of a shortcuts.vdf. The file's outer shape is
/// `{ shortcuts: { "0": {...}, "1": {...}, ... } }`; we return the inner
/// list of entry objects in their original order.
fn parse_shortcuts(buf: &[u8]) -> io::Result<Vec<Vec<(String, Value)>>> {
    let mut pos = 0;
    let root =
        parse_object(buf, &mut pos).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let shortcuts = root
        .into_iter()
        .find_map(|(k, v)| {
            if k.eq_ignore_ascii_case("shortcuts") {
                if let Value::Object(o) = v {
                    Some(o)
                } else {
                    None
                }
            } else {
                None
            }
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "shortcuts.vdf missing top-level 'shortcuts' object",
            )
        })?;

    let mut out = Vec::with_capacity(shortcuts.len());
    for (_idx, val) in shortcuts {
        match val {
            Value::Object(entry) => out.push(entry),
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("expected object entry, got {other:?}"),
                ));
            }
        }
    }
    Ok(out)
}

fn serialize_shortcuts(entries: &[Vec<(String, Value)>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(TY_OBJECT);
    write_cstr(&mut out, "shortcuts");
    for (i, entry) in entries.iter().enumerate() {
        out.push(TY_OBJECT);
        write_cstr(&mut out, &i.to_string());
        write_object_body(&mut out, entry);
    }
    out.push(END); // close 'shortcuts'
    out.push(END); // close root
    out
}

fn parse_object(buf: &[u8], pos: &mut usize) -> Result<Vec<(String, Value)>, String> {
    let mut out = Vec::new();
    loop {
        if *pos >= buf.len() {
            return Err("unexpected EOF inside object".into());
        }
        let ty = buf[*pos];
        *pos += 1;
        if ty == END {
            return Ok(out);
        }
        let name = read_cstr(buf, pos)?;
        let val = match ty {
            TY_OBJECT => Value::Object(parse_object(buf, pos)?),
            TY_STR => Value::Str(read_cstr(buf, pos)?),
            TY_INT => {
                if *pos + 4 > buf.len() {
                    return Err("EOF in int32 value".into());
                }
                let v = i32::from_le_bytes(buf[*pos..*pos + 4].try_into().unwrap());
                *pos += 4;
                Value::Int(v)
            }
            other => return Err(format!("unknown VDF type byte 0x{other:02x}")),
        };
        out.push((name, val));
    }
}

fn write_object_body(out: &mut Vec<u8>, fields: &[(String, Value)]) {
    for (k, v) in fields {
        match v {
            Value::Object(o) => {
                out.push(TY_OBJECT);
                write_cstr(out, k);
                write_object_body(out, o);
            }
            Value::Str(s) => {
                out.push(TY_STR);
                write_cstr(out, k);
                write_cstr(out, s);
            }
            Value::Int(n) => {
                out.push(TY_INT);
                write_cstr(out, k);
                out.extend_from_slice(&n.to_le_bytes());
            }
        }
    }
    out.push(END);
}

fn read_cstr(buf: &[u8], pos: &mut usize) -> Result<String, String> {
    let start = *pos;
    while *pos < buf.len() && buf[*pos] != 0 {
        *pos += 1;
    }
    if *pos >= buf.len() {
        return Err("unterminated C string".into());
    }
    let s = std::str::from_utf8(&buf[start..*pos])
        .map_err(|e| format!("non-UTF8 string: {e}"))?
        .to_owned();
    *pos += 1; // skip NUL
    Ok(s)
}

fn write_cstr(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(s.as_bytes());
    out.push(0);
}

fn crc32_ieee(bytes: &[u8]) -> u32 {
    // Bit-reflected IEEE 802.3 polynomial. Tiny + dependency-free —
    // shortcuts.vdf appid generation runs once per install, not in any
    // hot path, so we don't bother with a 256-entry lookup table.
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_field<'a>(e: &'a [(String, Value)], name: &str) -> Option<&'a Value> {
        e.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v)
    }

    #[test]
    fn crc32_known_vector() {
        // Standard IEEE 802.3 CRC32 test vector.
        assert_eq!(crc32_ieee(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32_ieee(b""), 0);
    }

    #[test]
    fn shortcut_appid_has_high_bit_set() {
        let id = shortcut_appid("\"/var/lib/network-deck/network-deck\"", "Network Deck");
        assert!(id & 0x8000_0000 != 0, "high bit must be set: {id:#x}");
    }

    #[test]
    fn round_trip_empty_shortcuts_file() {
        let bytes = serialize_shortcuts(&[]);
        let parsed = parse_shortcuts(&bytes).unwrap();
        assert_eq!(parsed.len(), 0);
        // Confirm exact wire form: 0x00 'shortcuts' \0 0x08 0x08
        assert_eq!(bytes, b"\x00shortcuts\x00\x08\x08");
    }

    #[test]
    fn round_trip_single_entry() {
        let spec = ShortcutSpec {
            exe: PathBuf::from("/var/lib/network-deck/network-deck"),
            app_name: "Network Deck".into(),
            start_dir: PathBuf::from("/var/lib/network-deck"),
            tags: vec!["Utilities".into()],
        };
        let entry = build_entry(&spec, &exe_to_string(&spec.exe));
        let bytes = serialize_shortcuts(std::slice::from_ref(&entry));
        let parsed = parse_shortcuts(&bytes).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0], entry);

        // Spot-check that the canonical fields survived.
        assert!(
            matches!(entry_field(&parsed[0], "AppName"), Some(Value::Str(s)) if s == "Network Deck")
        );
        assert!(
            matches!(entry_field(&parsed[0], "Exe"), Some(Value::Str(s)) if s.contains("network-deck"))
        );
        assert!(matches!(
            entry_field(&parsed[0], "AllowDesktopConfig"),
            Some(Value::Int(1))
        ));
        assert!(matches!(
            entry_field(&parsed[0], "tags"),
            Some(Value::Object(_))
        ));
    }

    #[test]
    fn upsert_creates_file_when_missing() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("config/shortcuts.vdf");
        let spec = ShortcutSpec {
            exe: PathBuf::from("/var/lib/network-deck/network-deck"),
            app_name: "Network Deck".into(),
            start_dir: PathBuf::from("/var/lib/network-deck"),
            tags: Vec::new(),
        };
        let outcome = upsert_shortcut(&path, &spec).unwrap();
        assert_eq!(outcome, WriteOutcome::Created);
        assert!(path.exists());

        let bytes = fs::read(&path).unwrap();
        let parsed = parse_shortcuts(&bytes).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(
            entry_exe(&parsed[0]),
            Some(exe_to_string(&spec.exe).as_str()),
        );
    }

    #[test]
    fn upsert_appends_when_existing_file_has_no_match() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("shortcuts.vdf");

        // Seed with an unrelated entry.
        let other = vec![
            (
                "appid".to_string(),
                Value::Int(0x8000_0001_u32.cast_signed()),
            ),
            ("AppName".into(), Value::Str("Some Other Game".into())),
            ("Exe".into(), Value::Str("\"/usr/bin/other\"".into())),
        ];
        fs::write(&path, serialize_shortcuts(std::slice::from_ref(&other))).unwrap();

        let spec = ShortcutSpec {
            exe: PathBuf::from("/var/lib/network-deck/network-deck"),
            app_name: "Network Deck".into(),
            start_dir: PathBuf::from("/var/lib/network-deck"),
            tags: Vec::new(),
        };
        let outcome = upsert_shortcut(&path, &spec).unwrap();
        assert_eq!(outcome, WriteOutcome::Added);

        let parsed = parse_shortcuts(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(parsed.len(), 2, "must keep the pre-existing entry");
        // First entry preserved verbatim.
        assert_eq!(entry_exe(&parsed[0]), Some("\"/usr/bin/other\""));
        // Second entry is ours.
        assert_eq!(
            entry_exe(&parsed[1]),
            Some(exe_to_string(&spec.exe).as_str()),
        );
    }

    #[test]
    fn upsert_replaces_in_place_on_re_run() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("shortcuts.vdf");
        let spec = ShortcutSpec {
            exe: PathBuf::from("/var/lib/network-deck/network-deck"),
            app_name: "Network Deck".into(),
            start_dir: PathBuf::from("/var/lib/network-deck"),
            tags: Vec::new(),
        };

        let first = upsert_shortcut(&path, &spec).unwrap();
        assert_eq!(first, WriteOutcome::Created);
        let parsed_after_first = parse_shortcuts(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(parsed_after_first.len(), 1);

        // Re-run with a different display name. Same Exe ⇒ Updated, count unchanged.
        let mut spec2 = spec.clone();
        spec2.app_name = "Network Deck (renamed)".into();
        let second = upsert_shortcut(&path, &spec2).unwrap();
        assert_eq!(second, WriteOutcome::Updated);

        let parsed = parse_shortcuts(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(parsed.len(), 1, "no duplicate entry on re-run");
        assert!(matches!(
            entry_field(&parsed[0], "AppName"),
            Some(Value::Str(s)) if s == "Network Deck (renamed)",
        ));
    }

    #[test]
    fn find_userdata_dirs_returns_empty_when_steam_never_run() {
        let td = tempfile::tempdir().unwrap();
        // No ~/.steam at all — fresh user, never logged into Steam.
        assert!(find_userdata_dirs(td.path()).is_empty());
    }

    #[test]
    fn find_userdata_dirs_skips_template_account_zero() {
        let td = tempfile::tempdir().unwrap();
        let userdata = td.path().join(".steam/steam/userdata");
        fs::create_dir_all(userdata.join("0/config")).unwrap();
        fs::create_dir_all(userdata.join("123456789/config")).unwrap();

        let dirs = find_userdata_dirs(td.path());
        assert_eq!(dirs.len(), 1);
        assert!(dirs[0].ends_with("123456789/config"));
    }

    #[test]
    fn add_to_all_userdata_writes_one_file_per_account() {
        let td = tempfile::tempdir().unwrap();
        let userdata = td.path().join(".steam/steam/userdata");
        fs::create_dir_all(userdata.join("111111111/config")).unwrap();
        fs::create_dir_all(userdata.join("222222222/config")).unwrap();

        let spec = ShortcutSpec {
            exe: PathBuf::from("/var/lib/network-deck/network-deck"),
            app_name: "Network Deck".into(),
            start_dir: PathBuf::from("/var/lib/network-deck"),
            tags: vec!["Utilities".into()],
        };

        let results = add_to_all_userdata(td.path(), &spec);
        assert_eq!(results.len(), 2);
        for (_, r) in &results {
            assert_eq!(r.as_ref().unwrap(), &WriteOutcome::Created);
        }

        for sub in ["111111111", "222222222"] {
            let path = userdata.join(sub).join("config/shortcuts.vdf");
            assert!(path.exists(), "missing {}", path.display());
            let parsed = parse_shortcuts(&fs::read(&path).unwrap()).unwrap();
            assert_eq!(parsed.len(), 1);
        }
    }

    #[test]
    fn parse_rejects_unknown_type_byte() {
        // Confirm we don't silently swallow a corrupt or future-extended file.
        let mut bytes = b"\x00shortcuts\x00".to_vec();
        bytes.push(0xFF); // unknown type
        bytes.extend_from_slice(b"junk\x00");
        bytes.push(END);
        bytes.push(END);
        let err = parse_shortcuts(&bytes).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
