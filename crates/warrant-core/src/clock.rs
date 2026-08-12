//! Time.
//!
//! Timestamps are recorded in the ledger, so replay reproduces them exactly
//! rather than re-reading the wall clock. Everything that needs the current
//! time takes it as an argument; only the outermost layer calls [`now_ms`].

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch.
///
/// Returns 0 if the system clock is set before 1970, which is a broken
/// machine rather than a case worth propagating as an error.
pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Format epoch milliseconds as RFC 3339 in UTC.
pub fn format_rfc3339(ms: u64) -> String {
    // Civil-from-days, per Howard Hinnant's algorithm. Avoids pulling a date
    // library into the crate every other crate depends on.
    let secs = (ms / 1000) as i64;
    let millis = ms % 1000;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_formats_correctly() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn known_instants_format_correctly() {
        // 2026-08-12T00:00:00Z == 1786492800 seconds since the epoch.
        assert_eq!(format_rfc3339(1_786_492_800_000), "2026-08-12T00:00:00.000Z");
        // A leap day, to exercise the civil-from-days path.
        assert_eq!(format_rfc3339(1_709_164_800_000), "2024-02-29T00:00:00.000Z");
    }

    #[test]
    fn the_clock_is_after_the_blueprint_was_written() {
        assert!(now_ms() > 1_700_000_000_000);
    }
}
