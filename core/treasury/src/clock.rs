//! The treasury's clock. The workspace bans clocks because the engine's only clock is the journal; the treasury is
//! outside the sequencer group, like a bank, and what it times — when a fake bank decides, when a transfer was made —
//! is never sequenced. Everything else takes the time as an argument, so tests choose it.

use std::time::{SystemTime, UNIX_EPOCH};

/// Wall-clock milliseconds since the Unix epoch.
#[allow(clippy::disallowed_methods)]
pub fn wall_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Milliseconds since the epoch as RFC 3339 in UTC, to the millisecond.
pub fn rfc3339(ms: u64) -> String {
    let (days, rest) = (ms / 86_400_000, ms % 86_400_000);
    let (hour, minute, second, milli) = (rest / 3_600_000, rest / 60_000 % 60, rest / 1000 % 60, rest % 1000);
    // Days to a civil date, after Howard Hinnant's `civil_from_days`, for dates after the epoch.
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{milli:03}Z")
}

#[cfg(test)]
mod tests {
    use super::rfc3339;

    #[test]
    fn milliseconds_read_as_dates() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(rfc3339(1_789_561_845_123), "2026-09-16T12:30:45.123Z");
    }
}
