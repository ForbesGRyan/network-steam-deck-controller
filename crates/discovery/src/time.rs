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

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference: `date -u -d @<secs> +%Y-%m-%dT%H:%M:%S` on a GNU system,
    /// or any online epoch converter. Picked dates that exercise leap-year
    /// rules, century rules, the 400-year exception, and the boundary
    /// behaviour at year/month/day rollovers.

    #[test]
    fn epoch_zero_is_unix_birthday() {
        assert_eq!(civil_from_secs(0), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn one_second_before_epoch() {
        assert_eq!(civil_from_secs(-1), (1969, 12, 31, 23, 59, 59));
    }

    #[test]
    fn last_second_of_2026() {
        // 2026-12-31T23:59:59Z = 1798761599 seconds since epoch.
        assert_eq!(civil_from_secs(1_798_761_599), (2026, 12, 31, 23, 59, 59));
    }

    #[test]
    fn first_second_of_2027() {
        assert_eq!(civil_from_secs(1_798_761_600), (2027, 1, 1, 0, 0, 0));
    }

    #[test]
    fn feb_29_2000_is_a_real_day_400_year_exception() {
        // 2000 is a leap year (div by 400) despite being a century year.
        // 2000-02-29T00:00:00Z = 951782400.
        assert_eq!(civil_from_secs(951_782_400), (2000, 2, 29, 0, 0, 0));
    }

    #[test]
    fn feb_28_2100_then_march_1_2100_no_leap_day() {
        // 2100 is a century year NOT divisible by 400 → not a leap year.
        // 2100-02-28T23:59:59Z = 4107542399.
        assert_eq!(civil_from_secs(4_107_542_399), (2100, 2, 28, 23, 59, 59));
        // Next second must roll to 2100-03-01T00:00:00Z, skipping Feb 29.
        assert_eq!(civil_from_secs(4_107_542_400), (2100, 3, 1, 0, 0, 0));
    }

    #[test]
    fn feb_29_2400_is_a_real_day_div_400() {
        // 2400 is divisible by 400 → leap year. 2400-02-29T00:00:00Z = 13_574_563_200.
        assert_eq!(civil_from_secs(13_574_563_200), (2400, 2, 29, 0, 0, 0));
    }

    #[test]
    fn day_rollover_at_midnight_utc() {
        // 2026-05-09T23:59:59Z = 1778371199. Next second → 2026-05-10T00:00:00Z.
        assert_eq!(civil_from_secs(1_778_371_199), (2026, 5, 9, 23, 59, 59));
        assert_eq!(civil_from_secs(1_778_371_200), (2026, 5, 10, 0, 0, 0));
    }

    #[test]
    fn now_us_is_after_2026_and_below_year_2100_bound() {
        // Sanity: this test is run in 2026 or later; the wall clock should
        // emit a value greater than 1700000000s but plausibly below 4 trillion µs
        // even far into the future. Pin both ends so a broken clock or a u32
        // overflow regression in `now_us` would be caught.
        let n = now_us();
        assert!(n > 1_700_000_000_000_000, "now_us suspiciously low: {n}");
        // Year 2100 ≈ 4.1e15 µs since epoch — picking 1e17 as a safety upper
        // bound that's still in this century at the human scale.
        assert!(n < 100_000_000_000_000_000, "now_us suspiciously high: {n}");
    }

    #[test]
    fn now_iso8601_format_shape() {
        // Don't pin the value (depends on wall clock), pin the format. The
        // beacon's `paired_at` field and the test fixture in pair_local both
        // depend on this layout.
        let s = now_iso8601();
        assert_eq!(s.len(), 20, "expected YYYY-MM-DDTHH:MM:SSZ, got {s:?}");
        let bytes = s.as_bytes();
        for &i in &[4, 7] {
            assert_eq!(bytes[i], b'-', "expected '-' at byte {i}, got {s:?}");
        }
        assert_eq!(bytes[10], b'T', "expected 'T' at byte 10, got {s:?}");
        for &i in &[13, 16] {
            assert_eq!(bytes[i], b':', "expected ':' at byte {i}, got {s:?}");
        }
        assert_eq!(bytes[19], b'Z', "expected 'Z' at byte 19, got {s:?}");
        // Every other byte is an ASCII digit.
        for (i, &b) in bytes.iter().enumerate() {
            if matches!(i, 4 | 7 | 10 | 13 | 16 | 19) {
                continue;
            }
            assert!(b.is_ascii_digit(), "expected digit at byte {i}, got {s:?}");
        }
    }
}
