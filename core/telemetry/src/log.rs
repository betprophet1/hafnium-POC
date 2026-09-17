//! One line per event on standard output: a JSON object for a log collector, or a readable line for a terminal
//! (deploy/observability/metrics.md, logs).
//!
//! `VENUE_LOG_FORMAT=json` writes `{"ts", "level", "service", "instance", "msg", ...fields}`; anything else, or nothing,
//! writes `ts LEVEL msg key=value ...`. `instance` is `VENUE_INSTANCE`, else `HOSTNAME`. Fields are what an operator
//! needs to find the event again (a half, a peer, a feed address); never a token, a secret or a key.
//!
//! These are operational logs. The audit logs of what clients sent and were told (interfaces/audit) are separate
//! files with their own format, and nothing here writes to them.

use serde_json::{Map, Value};
use std::fmt::Display;
use std::io::Write;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    const fn name(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

struct Logger {
    service: String,
    instance: String,
    json: bool,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

/// Names the process in every line and reads the format from the environment. A process that logs without calling
/// this is `venue`, in text.
pub fn init(service: &str) {
    let instance = std::env::var("VENUE_INSTANCE")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|i| !i.is_empty())
        .unwrap_or_else(|| "local".to_string());
    let json = std::env::var("VENUE_LOG_FORMAT").is_ok_and(|f| f.eq_ignore_ascii_case("json"));
    let _ = LOGGER.set(Logger { service: service.to_string(), instance, json });
}

fn logger() -> &'static Logger {
    LOGGER.get_or_init(|| Logger { service: "venue".into(), instance: "local".into(), json: false })
}

pub fn info(msg: impl Display, fields: &[(&str, &dyn Display)]) {
    event(Level::Info, msg, fields);
}

pub fn warn(msg: impl Display, fields: &[(&str, &dyn Display)]) {
    event(Level::Warn, msg, fields);
}

pub fn error(msg: impl Display, fields: &[(&str, &dyn Display)]) {
    event(Level::Error, msg, fields);
}

pub fn event(level: Level, msg: impl Display, fields: &[(&str, &dyn Display)]) {
    let line = format_line(logger(), crate::wall_ns(), level, &msg.to_string(), fields);
    // One write of the whole line, so lines from different threads never interleave.
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(line.as_bytes());
    let _ = out.flush();
}

fn format_line(logger: &Logger, now_ns: u64, level: Level, msg: &str, fields: &[(&str, &dyn Display)]) -> String {
    let ts = timestamp(now_ns);
    if logger.json {
        let mut object = Map::new();
        object.insert("ts".into(), Value::from(ts));
        object.insert("level".into(), Value::from(level.name()));
        object.insert("service".into(), Value::from(logger.service.as_str()));
        object.insert("instance".into(), Value::from(logger.instance.as_str()));
        object.insert("msg".into(), Value::from(msg));
        for (key, value) in fields {
            object.insert((*key).to_string(), Value::from(value.to_string()));
        }
        let mut line = Value::Object(object).to_string();
        line.push('\n');
        line
    } else {
        let mut line = format!("{ts} {:5} {msg}", level.name().to_uppercase());
        for (key, value) in fields {
            line.push_str(&format!(" {key}={value}"));
        }
        line.push('\n');
        line
    }
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` for nanoseconds since the epoch.
fn timestamp(nanos: u64) -> String {
    const NANOS_PER_DAY: u64 = 86_400_000_000_000;
    let (day, rest) = (nanos / NANOS_PER_DAY, nanos % NANOS_PER_DAY);
    let seconds = rest / 1_000_000_000;
    let (y, m, d) = civil(day);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60,
        rest % 1_000_000_000 / 1_000_000
    )
}

/// Year, month and day of a day since 1970-01-01, by Howard Hinnant's `civil_from_days`.
fn civil(day: u64) -> (u64, u64, u64) {
    let z = day + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + u64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT: u64 = 1_789_654_321_123_456_789; // 2026-09-17T14:12:01.123Z

    #[test]
    fn a_json_line_is_one_object_with_the_fields_escaped() {
        let logger = Logger { service: "venue-gateway".into(), instance: "gw-0".into(), json: true };
        let line = format_line(&logger, AT, Level::Warn, "feed \"a\": gone", &[("feed", &"core:7102"), ("n", &3)]);
        assert!(line.ends_with('\n') && !line[..line.len() - 1].contains('\n'));
        let v: Value = serde_json::from_str(&line).expect("JSON");
        assert_eq!(v["ts"], "2026-09-17T14:12:01.123Z");
        assert_eq!(v["level"], "warn");
        assert_eq!(v["service"], "venue-gateway");
        assert_eq!(v["instance"], "gw-0");
        assert_eq!(v["msg"], "feed \"a\": gone");
        assert_eq!(v["feed"], "core:7102");
        assert_eq!(v["n"], "3");
    }

    #[test]
    fn a_text_line_reads_on_a_terminal() {
        let logger = Logger { service: "venue-node".into(), instance: "local".into(), json: false };
        let line = format_line(&logger, AT, Level::Info, "recovered", &[("inputs", &12)]);
        assert_eq!(line, "2026-09-17T14:12:01.123Z INFO  recovered inputs=12\n");
    }
}
