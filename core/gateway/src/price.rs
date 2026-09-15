//! Prices, stakes and timestamps at the edge (blueprint §6; interfaces/gateway/README.md). Every price form becomes an
//! exact rational matched to a ladder row, and a stake becomes a contract count exactly or not at all.

use serde_json::{json, Value};
use venue_link::view::Row;

#[derive(Debug, PartialEq, Eq)]
pub enum PriceError {
    Malformed(String),
    NotOnLadder,
}

fn malformed(message: &str) -> PriceError {
    PriceError::Malformed(message.to_string())
}

const fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

/// A probability strictly between 0 and 1, in lowest terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ratio {
    pub n: u128,
    pub d: u128,
}

impl Ratio {
    pub fn new(n: u128, d: u128) -> Option<Self> {
        if n == 0 || n >= d {
            return None;
        }
        let g = gcd(n, d);
        Some(Self { n: n / g, d: d / g })
    }
}

pub fn ratio(row: &Row) -> Option<Ratio> {
    Ratio::new(u128::from(row.numerator), u128::from(row.denominator))
}

/// A price as the client gave it.
#[derive(Debug, PartialEq, Eq)]
pub enum Wanted {
    Tick(u64),
    Probability(Ratio),
}

/// At most 18 decimal digits, so products of two stay inside `u128`.
fn digits(s: &str) -> Option<u128> {
    (!s.is_empty() && s.len() <= 18 && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok()).flatten()
}

pub fn parse(price: &Value) -> Result<Wanted, PriceError> {
    let fields = price.as_object().filter(|o| o.len() == 1).ok_or_else(|| {
        malformed("price must be an object with exactly one of tick, probability, cents and american")
    })?;
    let Some((form, value)) = fields.iter().next() else { return Err(malformed("price is empty")) };
    let text = || value.as_str().ok_or_else(|| malformed("probability, cents and american are strings"));
    let between = || malformed("a price must be strictly between 0 and 1");
    match form.as_str() {
        "tick" => value.as_u64().map(Wanted::Tick).ok_or_else(|| malformed("tick must be a non-negative integer")),
        "probability" => {
            let (n, d) = text()?
                .split_once('/')
                .and_then(|(n, d)| Some((digits(n)?, digits(d)?)))
                .ok_or_else(|| malformed("probability must look like 53/100"))?;
            Ratio::new(n, d).map(Wanted::Probability).ok_or_else(between)
        }
        "cents" => {
            let s = text()?;
            let bad = || malformed("cents must look like 53 or 0.5");
            let (whole, fraction) = s.split_once('.').unwrap_or((s, "0"));
            let scale = 10u128.pow(u32::try_from(fraction.len()).map_err(|_| bad())?.min(12));
            if fraction.len() > 12 {
                return Err(bad());
            }
            let n = digits(whole).ok_or_else(bad)?.checked_mul(scale).and_then(|w| w.checked_add(digits(fraction)?));
            Ratio::new(n.ok_or_else(bad)?, 100 * scale).map(Wanted::Probability).ok_or_else(between)
        }
        "american" => {
            let s = text()?;
            let bad = || malformed("american must look like -110 or +150");
            let a = s.get(1..).and_then(digits).filter(|a| *a > 0).ok_or_else(bad)?;
            let p = match s.get(..1) {
                Some("+") => Ratio::new(100, a + 100),
                Some("-") => Ratio::new(a, a + 100),
                _ => return Err(bad()),
            };
            p.map(Wanted::Probability).ok_or_else(between)
        }
        _ => Err(malformed("price must be one of tick, probability, cents and american")),
    }
}

/// The ordinal of the ladder row a price names.
pub fn row_of(ladder: &[Row], wanted: &Wanted) -> Result<u64, PriceError> {
    let found = match wanted {
        Wanted::Tick(t) => usize::try_from(*t).ok().filter(|t| *t < ladder.len()),
        Wanted::Probability(p) => ladder.iter().position(|r| ratio(r) == Some(*p)),
    };
    found.map(|t| t as u64).ok_or(PriceError::NotOnLadder)
}

/// Cents per $1 contract to four decimal places: exact when it terminates there, otherwise rounded for display.
fn cents(p: Ratio) -> String {
    let scaled = 1_000_000 * p.n / p.d;
    let rounded = if (1_000_000 * p.n % p.d) * 2 >= p.d { scaled + 1 } else { scaled };
    let (whole, fraction) = (rounded / 10_000, rounded % 10_000);
    if fraction == 0 {
        whole.to_string()
    } else {
        format!("{whole}.{}", format!("{fraction:04}").trim_end_matches('0'))
    }
}

/// American odds when they are a whole number.
fn american(p: Ratio) -> Option<String> {
    match (2 * p.n).cmp(&p.d) {
        std::cmp::Ordering::Greater => {
            let (num, den) = (100 * p.n, p.d - p.n);
            (num % den == 0).then(|| format!("-{}", num / den))
        }
        std::cmp::Ordering::Less => {
            let (num, den) = (100 * (p.d - p.n), p.n);
            (num % den == 0).then(|| format!("+{}", num / den))
        }
        std::cmp::Ordering::Equal => Some("+100".to_string()),
    }
}

/// A `PriceQuote` for a row of a ladder.
pub fn quote(ladder: &[Row], tick: u64) -> Value {
    match usize::try_from(tick).ok().and_then(|t| ladder.get(t)).and_then(ratio) {
        Some(p) => {
            json!({"tick": tick, "probability": format!("{}/{}", p.n, p.d), "cents": cents(p), "american": american(p)})
        }
        None => json!({"tick": tick, "probability": null, "cents": null, "american": null}),
    }
}

/// The contracts a stake buys at `price` in whole lots, or `(step cents, contracts per step)` when it does not.
pub fn contracts_for_stake(stake_cents: u64, price: Ratio, lot: u64) -> Result<u64, (u128, u128)> {
    let lot = u128::from(lot.max(1));
    let unit = lot * 100 * price.n;
    let g = gcd(unit, price.d);
    let (step, per_step) = (unit / g, lot * price.d / g);
    let stake = u128::from(stake_cents);
    if stake % step != 0 {
        return Err((step, per_step));
    }
    u64::try_from(stake / step * per_step).map_err(|_| (step, per_step))
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let (era, yoe) = (y.div_euclid(400), y.rem_euclid(400));
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Nanoseconds since the Unix epoch for an RFC 3339 timestamp with `Z` or a numeric offset.
pub fn parse_timestamp(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let number =
        |part: &str| -> Option<i64> { part.bytes().all(|c| c.is_ascii_digit()).then(|| part.parse().ok()).flatten() };
    let field = |from: usize, to: usize| s.get(from..to).and_then(number);
    let (year, month, day) = (field(0, 4)?, field(5, 7)?, field(8, 10)?);
    let (hour, minute, second) = (field(11, 13)?, field(14, 16)?, field(17, 19)?);
    if !(1..=12).contains(&month) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    if civil_from_days(days) != (year, month, day) {
        return None;
    }
    let mut rest = s.get(19..)?;
    let mut nanos = 0i64;
    if let Some(fraction) = rest.strip_prefix('.') {
        let count = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if count == 0 || count > 9 {
            return None;
        }
        nanos = number(&fraction[..count])? * 10i64.pow(9 - count as u32);
        rest = &fraction[count..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            if rest.len() != 6 || rest.as_bytes()[3] != b':' {
                return None;
            }
            sign * (number(rest.get(1..3)?)? * 3600 + number(rest.get(4..6)?)? * 60)
        }
    };
    let seconds = days.checked_mul(86_400)? + hour * 3600 + minute * 60 + second - offset;
    u64::try_from(seconds).ok()?.checked_mul(1_000_000_000)?.checked_add(u64::try_from(nanos).ok()?)
}

/// RFC 3339 in UTC with milliseconds.
pub fn format_timestamp(ns: u64) -> String {
    let seconds = ns / 1_000_000_000;
    let (y, m, d) = civil_from_days((seconds / 86_400) as i64);
    let rest = seconds % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3600,
        rest / 60 % 60,
        rest % 60,
        ns / 1_000_000 % 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cent_ladder() -> Vec<Row> {
        (0..99).map(|t| Row { numerator: t + 1, denominator: 100, complement: 98 - t }).collect()
    }

    #[test]
    fn every_price_form_names_the_same_row_and_nothing_is_rounded() {
        let ladder = cent_ladder();
        for p in
            [json!({"tick": 52}), json!({"probability": "53/100"}), json!({"cents": "53"}), json!({"cents": "53.00"})]
        {
            assert_eq!(row_of(&ladder, &parse(&p).unwrap()), Ok(52), "{p}");
        }
        assert_eq!(row_of(&ladder, &parse(&json!({"american": "+100"})).unwrap()), Ok(49));
        assert_eq!(row_of(&ladder, &parse(&json!({"cents": "53.5"})).unwrap()), Err(PriceError::NotOnLadder));
        assert_eq!(row_of(&ladder, &parse(&json!({"tick": 99})).unwrap()), Err(PriceError::NotOnLadder));
        for bad in
            [json!({"cents": "53", "tick": 1}), json!({"probability": "1/1"}), json!({"american": "110"}), json!(53)]
        {
            assert!(matches!(parse(&bad), Err(PriceError::Malformed(_))), "{bad}");
        }
    }

    #[test]
    fn quotes_carry_the_exact_probability_and_whole_american_odds() {
        let minus_110 = [Row { numerator: 11, denominator: 21, complement: 0 }];
        assert_eq!(parse(&json!({"american": "-110"})).unwrap(), Wanted::Probability(Ratio::new(11, 21).unwrap()));
        let q = quote(&minus_110, 0);
        assert_eq!(
            (q["probability"].as_str(), q["cents"].as_str(), q["american"].as_str()),
            (Some("11/21"), Some("52.381"), Some("-110"))
        );
        assert_eq!(quote(&cent_ladder(), 52)["american"], Value::Null);
        assert_eq!(quote(&cent_ladder(), 39)["american"], "+150");
        assert_eq!(quote(&cent_ladder(), 52)["cents"], "53");
    }

    #[test]
    fn a_stake_buys_whole_lots_or_names_its_step() {
        assert_eq!(contracts_for_stake(530, Ratio::new(53, 100).unwrap(), 1), Ok(10));
        assert_eq!(contracts_for_stake(100, Ratio::new(53, 100).unwrap(), 1), Err((53, 1)));
        assert_eq!(contracts_for_stake(1000, Ratio::new(40, 100).unwrap(), 1), Ok(25));
        // Risk $11 to win $10 at -110: $11 buys 21 contracts, and nothing smaller buys a whole one.
        assert_eq!(contracts_for_stake(1100, Ratio::new(11, 21).unwrap(), 1), Ok(21));
        assert_eq!(contracts_for_stake(150, Ratio::new(11, 21).unwrap(), 1), Err((1100, 21)));
        // A lot step of 5 at 53¢: lots cost 265¢.
        assert_eq!(contracts_for_stake(530, Ratio::new(53, 100).unwrap(), 5), Ok(10));
        assert_eq!(contracts_for_stake(300, Ratio::new(53, 100).unwrap(), 5), Err((265, 5)));
    }

    #[test]
    fn timestamps_parse_with_offsets_and_reject_impossible_dates() {
        assert_eq!(parse_timestamp("1970-01-01T00:00:01Z"), Some(1_000_000_000));
        assert_eq!(parse_timestamp("2026-09-15T08:00:00.5+01:00"), parse_timestamp("2026-09-15T07:00:00.500Z"));
        assert_eq!(parse_timestamp("2026-02-29T00:00:00Z"), None);
        assert_eq!(parse_timestamp("2028-02-29T00:00:00Z").map(|ns| ns > 0), Some(true));
        assert_eq!(parse_timestamp("2026-09-15 07:00:00Z"), None);
        let ns = parse_timestamp("2026-09-15T07:22:46.538Z").unwrap();
        assert_eq!(format_timestamp(ns), "2026-09-15T07:22:46.538Z");
    }
}
