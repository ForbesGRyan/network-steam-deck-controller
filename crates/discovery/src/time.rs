//! Tiny wall-clock helpers used by beacon + pair packets.

use std::time::{SystemTime, UNIX_EPOCH};

/// Microseconds since the Unix epoch, saturating to 0 on a clock that's
/// pre-epoch (which we'd never see on a sane host but the API tolerates it).
#[allow(clippy::cast_possible_truncation)]
pub fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

/// `now()` formatted as `YYYY-MM-DDTHH:MM:SSZ`. Used for the `paired_at`
/// stamp in trusted-peers files; precision-to-the-second is fine.
#[must_use]
pub fn now_iso8601() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs();
    #[allow(clippy::cast_possible_wrap)]
    let (y, mo, d, h, mi, s) = civil_from_secs(secs as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn civil_from_secs(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let day_secs = 86_400_i64;
    let days = secs.div_euclid(day_secs);
    let tod = secs.rem_euclid(day_secs);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe + era * 400) as i32;
    let doy = (doe - (365 * yoe + yoe / 4 - yoe / 100)) as u32;
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, (tod / 3600) as u32, ((tod / 60) % 60) as u32, (tod % 60) as u32)
}
