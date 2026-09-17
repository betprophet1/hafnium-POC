//! The identity service's clock. The workspace bans clocks because the engine's only clock is the journal; a key's
//! creation, expiry and last use are about the edge, not the market, and are never sequenced. Everything else takes the
//! time as an argument, so tests choose it.

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

/// An RFC 3339 time in UTC (`Z`), from 1970 on, as milliseconds since the epoch; digits past the millisecond are
/// dropped. Offsets other than `Z` are refused rather than converted, so an expiry means one instant to everyone who
/// reads it.
pub fn parse_rfc3339(text: &str) -> Option<u64> {
    let b = text.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let number = |from: usize, to: usize| -> Option<u64> {
        let part = text.get(from..to)?;
        part.bytes().all(|c| c.is_ascii_digit()).then(|| part.parse().ok()).flatten()
    };
    let (year, month, day) = (number(0, 4)?, number(5, 7)?, number(8, 10)?);
    let (hour, minute, second) = (number(11, 13)?, number(14, 16)?, number(17, 19)?);
    let rest = &text[19..];
    let (fraction, zone) = match rest.strip_prefix('.') {
        Some(after) => {
            let digits = after.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 {
                return None;
            }
            (&after[..digits], &after[digits..])
        }
        None => ("", rest),
    };
    if !matches!(zone, "Z" | "z")
        || year < 1970
        || !(1..=12).contains(&month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if day == 0 || day > month_days[(month - 1) as usize] {
        return None;
    }
    // Hinnant's `days_from_civil`.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let millis = format!("{fraction:0<3}").get(..3)?.parse::<u64>().ok()?;
    Some(((days * 24 + hour) * 60 + minute) * 60_000 + second * 1000 + millis)
}

#[cfg(test)]
mod tests {
    use super::{parse_rfc3339, rfc3339};

    #[test]
    fn milliseconds_read_as_dates_and_back() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(rfc3339(1_789_561_845_123), "2026-09-16T12:30:45.123Z");
        for ms in [0, 951_782_400_000, 1_789_561_845_123, 4_102_444_799_999] {
            assert_eq!(parse_rfc3339(&rfc3339(ms)), Some(ms));
        }
        assert_eq!(parse_rfc3339("2026-09-16T12:30:45Z"), Some(1_789_561_845_000));
        assert_eq!(parse_rfc3339("2026-09-16T12:30:45.123456789Z"), Some(1_789_561_845_123));
        assert_eq!(parse_rfc3339("2026-09-16T12:30:45.1Z"), Some(1_789_561_845_100));
        for bad in
            ["2026-09-16T12:30:45+01:00", "2026-02-30T00:00:00Z", "2026-09-16 12:30:45Z", "2026-09-16T12:30:45.Z"]
        {
            assert_eq!(parse_rfc3339(bad), None, "{bad}");
        }
    }
}
