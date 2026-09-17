//! The gateway's edge audit log (interfaces/audit/README.md): every request a client made and what it was answered,
//! refusals above all, since those never reach the journal (blueprint §12, flood protection).
//!
//! The event loops must never wait on a disk, so they only take a record — the connection it is about and what was
//! said — and put it on a bounded queue. One writer thread chains the records into lines, writes each with one write,
//! and syncs the file once for everything it took together. A full queue is waited on rather than dropped from: a
//! loop that cannot queue a record for `STALL` takes the disk to have hung, and a record that cannot be written stops
//! the process (the format document says why).

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const FORMAT: &str = "venue-audit/1";
pub const COMPONENT: &str = "gateway";
const ZEROS: &str = "0000000000000000000000000000000000000000000000000000000000000000";
/// Records the queue holds: seconds of a busy gateway's private traffic.
pub const CAPACITY: usize = 65_536;
/// How long a loop waits for room on a full queue, counted in millisecond waits, before the disk is taken to have hung.
const STALL_WAITS: u32 = 10_000;
/// Records written between syncs at most.
const BATCH: usize = 1024;
/// A quiet log still looks at the clock this often, to rotate soon after midnight.
const IDLE: Duration = Duration::from_secs(1);
/// The exit status of a gateway whose audit log failed: EX_IOERR.
pub const EXIT_IO_ERROR: i32 = 74;
const MICROS_PER_DAY: u64 = 86_400_000_000;

/// Wall-clock microseconds since the epoch, for audit timestamps only. The workspace bans clocks because the engine's
/// only clock is the journal; an audit timestamp says when the edge saw something, and nothing reads it back.
#[allow(clippy::disallowed_methods)]
pub fn wall_micros() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
}

/// What happens when the log cannot go on: in the gateway, a message and an exit.
pub type Failure = Arc<dyn Fn(&str) + Send + Sync>;

fn exit_on_failure() -> Failure {
    Arc::new(|reason| {
        eprintln!("venue-gateway: audit log failed, stopping: {reason}");
        std::process::exit(EXIT_IO_ERROR);
    })
}

struct Record {
    micros: u64,
    kind: &'static str,
    fields: Map<String, Value>,
}

/// Where records are taken. Cheap to clone; every clone feeds the same writer.
#[derive(Clone)]
pub struct Audit {
    queue: Option<SyncSender<Record>>,
    failure: Failure,
}

impl Audit {
    /// A log that records nothing, for a gateway built without one: only tests of the order path build those.
    pub fn disabled() -> Self {
        Self { queue: None, failure: exit_on_failure() }
    }

    /// Opens the log in `dir` for `instance`, sealing files an earlier gateway of that instance left open, and starts
    /// its writer. An error means the log cannot be kept, and the gateway must not start.
    pub fn start(dir: &Path, instance: &str, fsync: bool) -> io::Result<Self> {
        Self::start_with(dir, instance, fsync, CAPACITY, exit_on_failure())
    }

    pub fn start_with(dir: &Path, instance: &str, fsync: bool, capacity: usize, failure: Failure) -> io::Result<Self> {
        let mut writer = Writer::open(dir, COMPONENT, instance, fsync, wall_micros())?;
        let (queue, records) = mpsc::sync_channel(capacity);
        let on_failure = Arc::clone(&failure);
        thread::Builder::new().name("gateway-audit".into()).spawn(move || {
            if let Err(e) = writer.run(&records) {
                on_failure(&format!("writing {}: {e}", writer.path().display()));
            }
        })?;
        Ok(Self { queue: Some(queue), failure })
    }

    /// A handle whose records wait on `capacity` places for whoever holds the receiver: for testing what a full queue
    /// does.
    #[cfg(test)]
    fn queue_only(capacity: usize, failure: Failure) -> (Self, Receiver<Record>) {
        let (queue, records) = mpsc::sync_channel(capacity);
        (Self { queue: Some(queue), failure }, records)
    }

    /// Takes a record of `kind` now. Waits while the queue is full, and fails the log if it stays full.
    pub fn record(&self, kind: &'static str, fields: Map<String, Value>) {
        let Some(queue) = &self.queue else { return };
        let mut record = Record { micros: wall_micros(), kind, fields };
        for _ in 0..STALL_WAITS {
            match queue.try_send(record) {
                Ok(()) => return,
                Err(TrySendError::Full(back)) => {
                    record = back;
                    thread::sleep(Duration::from_millis(1));
                }
                Err(TrySendError::Disconnected(_)) => {
                    (self.failure)("the audit writer has stopped");
                    return;
                }
            }
        }
        (self.failure)("the audit queue stayed full for 10 s: the disk is not taking writes");
    }
}

/// The name a file of `component` and `instance` has for a day and segment.
fn file_name(component: &str, instance: &str, day: u64, segment: u32) -> String {
    format!("{component}.{instance}.{}.{segment:04}.jsonl", date(day))
}

/// The instance name as it appears in file names: letters, digits, `-` and `_`.
pub fn sanitise(instance: &str) -> String {
    let name: String =
        instance.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect();
    if name.is_empty() {
        "local".into()
    } else {
        name
    }
}

/// `YYYY-MM-DD` for days since the epoch (the proleptic Gregorian calendar, as `date -u` has it).
fn date(day: u64) -> String {
    let (y, m, d) = civil(day);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Year, month and day of a day since 1970-01-01, by Howard Hinnant's `civil_from_days`, for days from 1970 on.
fn civil(day: u64) -> (u64, u64, u64) {
    let z = day + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + u64::from(m <= 2);
    (y, m, d)
}

/// `YYYY-MM-DDTHH:MM:SS.ffffffZ`.
pub fn timestamp(micros: u64) -> String {
    let (day, rest) = (micros / MICROS_PER_DAY, micros % MICROS_PER_DAY);
    let seconds = rest / 1_000_000;
    format!("{}T{:02}:{:02}:{:02}.{:06}Z", date(day), seconds / 3600, seconds / 60 % 60, seconds % 60, rest % 1_000_000)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// One record as its line, without the newline.
fn line(
    micros: u64,
    n: u64,
    component: &str,
    instance: &str,
    kind: &str,
    fields: &Map<String, Value>,
    prev: &str,
) -> String {
    let mut out = format!(
        "{{\"ts\":\"{}\",\"n\":{n},\"component\":{},\"instance\":{},\"kind\":{}",
        timestamp(micros),
        Value::from(component),
        Value::from(instance),
        Value::from(kind)
    );
    for (key, value) in fields {
        out.push(',');
        out.push_str(&Value::from(key.as_str()).to_string());
        out.push(':');
        out.push_str(&value.to_string());
    }
    out.push_str(",\"prev\":\"");
    out.push_str(prev);
    out.push_str("\"}");
    out
}

/// The file being written, and what the next line chains to.
struct Writer {
    dir: PathBuf,
    component: String,
    instance: String,
    fsync: bool,
    file: File,
    name: String,
    day: u64,
    next: u64,
    prev: String,
}

impl Writer {
    fn open(dir: &Path, component: &str, instance: &str, fsync: bool, micros: u64) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let instance = sanitise(instance);
        let after = seal_earlier_files(dir, component, &instance, fsync, micros)?;
        let (file, name) = create_file(dir, component, &instance, micros / MICROS_PER_DAY)?;
        let mut writer = Self {
            dir: dir.to_path_buf(),
            component: component.to_string(),
            instance,
            fsync,
            file,
            name,
            day: micros / MICROS_PER_DAY,
            next: 0,
            prev: ZEROS.to_string(),
        };
        writer.begin(micros, after)?;
        Ok(writer)
    }

    fn path(&self) -> PathBuf {
        self.dir.join(&self.name)
    }

    /// Writes what arrives until every handle has gone, then closes the file.
    fn run(&mut self, records: &Receiver<Record>) -> io::Result<()> {
        loop {
            match records.recv_timeout(IDLE) {
                Ok(first) => {
                    self.append(first.micros, first.kind, &first.fields)?;
                    for record in records.try_iter().take(BATCH - 1) {
                        self.append(record.micros, record.kind, &record.fields)?;
                    }
                    self.sync()?;
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.rotate_if_past_midnight(wall_micros())?;
                }
                Err(RecvTimeoutError::Disconnected) => return self.close(wall_micros(), "shutdown"),
            }
        }
    }

    /// Appends one record, first rotating if it belongs to a later day than the file.
    fn append(&mut self, micros: u64, kind: &str, fields: &Map<String, Value>) -> io::Result<()> {
        self.rotate_if_past_midnight(micros)?;
        self.write_line(micros, kind, fields)
    }

    fn write_line(&mut self, micros: u64, kind: &str, fields: &Map<String, Value>) -> io::Result<()> {
        let text = line(micros, self.next, &self.component, &self.instance, kind, fields, &self.prev);
        let mut bytes = Vec::with_capacity(text.len() + 1);
        bytes.extend_from_slice(text.as_bytes());
        bytes.push(b'\n');
        self.file.write_all(&bytes)?;
        self.prev = sha256_hex(text.as_bytes());
        self.next += 1;
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        if self.fsync {
            self.file.sync_data()?;
        }
        Ok(())
    }

    fn rotate_if_past_midnight(&mut self, micros: u64) -> io::Result<()> {
        if micros / MICROS_PER_DAY > self.day {
            let closed = self.name.clone();
            self.close(micros, "rotate")?;
            let last = self.prev.clone();
            self.open_file(micros, Some(json!({"file": closed, "last": last})))?;
        }
        Ok(())
    }

    fn open_file(&mut self, micros: u64, after: Option<Value>) -> io::Result<()> {
        let day = micros / MICROS_PER_DAY;
        (self.file, self.name) = create_file(&self.dir, &self.component, &self.instance, day)?;
        self.day = day;
        self.next = 0;
        self.prev = ZEROS.to_string();
        self.begin(micros, after)
    }

    /// Writes a new file's open record, and makes the file durable.
    fn begin(&mut self, micros: u64, after: Option<Value>) -> io::Result<()> {
        let mut open = Map::new();
        open.insert("format".into(), json!(FORMAT));
        open.insert("file".into(), json!(self.name));
        open.insert("pid".into(), json!(std::process::id()));
        if let Some(after) = after {
            open.insert("after".into(), after);
        }
        self.write_line(micros, "open", &open)?;
        self.sync()?;
        if self.fsync {
            // A new file is durable only once its directory entry is.
            File::open(&self.dir)?.sync_all()?;
        }
        Ok(())
    }

    /// Ends the file with its close record.
    fn close(&mut self, micros: u64, reason: &str) -> io::Result<()> {
        let mut close = Map::new();
        close.insert("reason".into(), json!(reason));
        close.insert("records".into(), json!(self.next));
        self.write_line(micros, "close", &close)?;
        self.sync()
    }
}

/// Creates the next file of `component` and `instance` for `day`: segment 0, or one past the last segment the day has.
fn create_file(dir: &Path, component: &str, instance: &str, day: u64) -> io::Result<(File, String)> {
    let prefix = format!("{component}.{instance}.{}.", date(day));
    let segment = own_files(dir, component, instance)?
        .iter()
        .filter_map(|name| name.strip_prefix(&prefix)?.strip_suffix(".jsonl")?.parse::<u32>().ok())
        .map(|s| s + 1)
        .max()
        .unwrap_or(0);
    let name = file_name(component, instance, day, segment);
    let file = OpenOptions::new().append(true).create_new(true).open(dir.join(&name))?;
    Ok((file, name))
}

/// The names of the audit files of `component` and `instance` in `dir`, oldest first.
fn own_files(dir: &Path, component: &str, instance: &str) -> io::Result<Vec<String>> {
    let prefix = format!("{component}.{instance}.");
    let mut names: Vec<String> = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| {
            name.strip_prefix(&prefix).and_then(|rest| rest.strip_suffix(".jsonl")).is_some_and(|rest| {
                let (date, segment) = rest.split_once('.').unwrap_or(("", ""));
                date.len() == 10 && segment.len() == 4 && segment.bytes().all(|b| b.is_ascii_digit())
            })
        })
        .collect();
    names.sort();
    Ok(names)
}

/// A file's last line without its newline; None when the file is empty or its last line has no newline.
fn last_line(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    if length == 0 {
        return Ok(None);
    }
    let mut chunk = vec![0u8; 64 * 1024];
    let mut end = length;
    let mut tail: Vec<u8> = Vec::new();
    loop {
        let size = end.min(chunk.len() as u64);
        let start = end - size;
        file.seek(SeekFrom::Start(start))?;
        let piece = &mut chunk[..size as usize];
        file.read_exact(piece)?;
        let mut joined = piece.to_vec();
        joined.extend_from_slice(&tail);
        tail = joined;
        if end == length && tail.last() != Some(&b'\n') {
            return Ok(None);
        }
        // The newline that ends the last line is the tail's final byte; the line starts after the one before it.
        let body = &tail[..tail.len() - 1];
        if let Some(i) = body.iter().rposition(|&b| b == b'\n') {
            return Ok(Some(body[i + 1..].to_vec()));
        }
        if start == 0 {
            return Ok(Some(body.to_vec()));
        }
        end = start;
    }
}

/// Appends a close record to each file of this component and instance that an earlier process left without one and
/// whose last line is whole, and says what the newest file ended with, for the new file's `after`.
fn seal_earlier_files(
    dir: &Path,
    component: &str,
    instance: &str,
    fsync: bool,
    micros: u64,
) -> io::Result<Option<Value>> {
    let mut after = None;
    for name in own_files(dir, component, instance)? {
        let path = dir.join(&name);
        let last = last_line(&path)?;
        let record: Option<Value> = last.as_ref().and_then(|l| serde_json::from_slice(l).ok());
        let (Some(last), Some(n)) = (last, record.as_ref().and_then(|r| r["n"].as_u64())) else {
            eprintln!("venue-gateway: audit file {} ends in a cut-off record; left as it is", path.display());
            after = Some(json!({"file": name, "sealed": false}));
            continue;
        };
        if record.as_ref().is_some_and(|r| r["kind"] == "close") {
            after = Some(json!({"file": name, "last": sha256_hex(&last)}));
            continue;
        }
        let mut close = Map::new();
        close.insert("reason".into(), json!("recovered"));
        close.insert("records".into(), json!(n + 1));
        let text = line(micros, n + 1, component, instance, "close", &close, &sha256_hex(&last));
        let mut file = OpenOptions::new().append(true).open(&path)?;
        file.write_all(format!("{text}\n").as_bytes())?;
        if fsync {
            file.sync_data()?;
        }
        eprintln!("venue-gateway: sealed audit file {}, left open by an earlier process", path.display());
        after = Some(json!({"file": name, "last": sha256_hex(text.as_bytes())}));
    }
    Ok(after)
}

/// Checks one file (interfaces/audit/README.md, verifying): `Ok(closed)`, or the first broken line, counted from 1.
pub fn verify(path: &Path) -> Result<bool, String> {
    let data = fs::read(path).map_err(|e| e.to_string())?;
    if data.last() != Some(&b'\n') {
        return Err("the file is empty or its last line is cut off".into());
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let (mut prev, mut owner, mut closed) = (ZEROS.to_string(), None, false);
    for (n, raw) in data[..data.len() - 1].split(|&b| b == b'\n').enumerate() {
        let broken = |why: &str| Err(format!("line {}: {why}", n + 1));
        if closed {
            return broken("a record after the close record");
        }
        let Ok(record) = serde_json::from_slice::<Value>(raw) else { return broken("not JSON") };
        if record["prev"].as_str() != Some(prev.as_str()) {
            return broken("prev is not the hash of the line before");
        }
        if record["n"].as_u64() != Some(n as u64) {
            return broken("n is out of step");
        }
        let this = (record["component"].clone(), record["instance"].clone());
        if n == 0 {
            if record["kind"] != "open" || record["format"] != FORMAT || record["file"] != name {
                return broken("not an open record naming this file");
            }
            owner = Some(this);
        } else if owner.as_ref() != Some(&this) {
            return broken("component or instance differs from the open record");
        }
        if record["kind"] == "close" {
            if record["records"].as_u64() != Some(n as u64) {
                return broken("the close record miscounts");
            }
            closed = true;
        }
        prev = sha256_hex(raw);
    }
    Ok(closed)
}

// ---------------------------------------------------------------------------------------------------------------------
// Masking (interfaces/audit/README.md, masking)

/// A header's value as it is recorded: credentials become their scheme and `***`.
pub fn header_value(name: &str, value: &str) -> String {
    match name {
        "authorization" | "proxy-authorization" | "cookie" => match value.split_once(' ') {
            Some((scheme, _)) if name != "cookie" => format!("{scheme} ***"),
            _ => "***".into(),
        },
        _ => value.to_string(),
    }
}

/// JSON text as it is recorded: the string value of every member named `token` becomes `***`. Found textually, so a
/// frame that is not valid JSON is masked too.
pub fn mask_tokens(text: &str) -> String {
    const KEY: &str = "\"token\"";
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut from = 0;
    while let Some(found) = text[from..].find(KEY) {
        let key_end = from + found + KEY.len();
        from = key_end;
        let mut i = key_end;
        while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        if bytes.get(i) != Some(&b':') {
            continue;
        }
        i += 1;
        while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        if bytes.get(i) != Some(&b'"') {
            continue;
        }
        let start = i + 1;
        let mut end = start;
        while end < bytes.len() && bytes[end] != b'"' {
            end += if bytes[end] == b'\\' { 2 } else { 1 };
        }
        let end = end.min(bytes.len());
        out.push_str(&text[copied..start]);
        out.push_str("***");
        copied = end;
        from = end;
    }
    out.push_str(&text[copied..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const DAY: u64 = MICROS_PER_DAY;
    /// 2026-09-17T23:59:59.999999Z.
    const LATE: u64 = 20_714 * DAY - 1;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("venue-gateway-audit-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn fields(text: &str) -> Map<String, Value> {
        let mut fields = Map::new();
        fields.insert("dir".into(), json!("in"));
        fields.insert("msg".into(), json!(text));
        fields
    }

    fn files(dir: &Path) -> Vec<PathBuf> {
        own_files(dir, COMPONENT, "test").expect("listed").into_iter().map(|n| dir.join(n)).collect()
    }

    fn lines(path: &Path) -> Vec<Value> {
        fs::read_to_string(path).expect("read").lines().map(|l| serde_json::from_str(l).expect("JSON")).collect()
    }

    #[test]
    fn timestamps_and_dates_are_utc() {
        assert_eq!(timestamp(0), "1970-01-01T00:00:00.000000Z");
        assert_eq!(timestamp(LATE), "2026-09-17T23:59:59.999999Z");
        assert_eq!(timestamp(LATE + 1), "2026-09-18T00:00:00.000000Z");
        // A leap day, and the turn of a century that is not one.
        assert_eq!(date(11_016), "2000-02-29");
        assert_eq!(date(47_541), "2100-03-01");
    }

    #[test]
    fn records_chain_and_a_new_day_closes_the_file_and_opens_the_next() {
        let dir = dir("rotate");
        let mut writer = Writer::open(&dir, COMPONENT, "test", false, LATE - 5).expect("opened");
        writer.append(LATE - 1, "ws-frame", &fields("before midnight")).expect("written");
        writer.append(LATE + 1, "ws-frame", &fields("after midnight")).expect("written");
        writer.close(LATE + 2, "shutdown").expect("closed");
        let files = files(&dir);
        let names: Vec<_> = files.iter().filter_map(|p| p.file_name()?.to_str()).collect();
        assert_eq!(names, ["gateway.test.2026-09-17.0000.jsonl", "gateway.test.2026-09-18.0000.jsonl"]);
        assert_eq!(files.iter().map(|f| verify(f)).collect::<Vec<_>>(), [Ok(true), Ok(true)]);
        let (first, second) = (lines(&files[0]), lines(&files[1]));
        assert_eq!(
            first.iter().map(|r| r["kind"].as_str().unwrap_or("")).collect::<Vec<_>>(),
            ["open", "ws-frame", "close"]
        );
        assert_eq!((first[2]["reason"].as_str(), first[2]["records"].as_u64()), (Some("rotate"), Some(2)));
        let close_line =
            fs::read_to_string(&files[0]).expect("read").lines().last().map(str::to_string).expect("a line");
        assert_eq!(second[0]["after"], json!({"file": names[0], "last": sha256_hex(close_line.as_bytes())}));
        assert_eq!(second[1]["msg"], "after midnight");
    }

    #[test]
    fn an_altered_or_removed_line_breaks_the_chain() {
        let dir = dir("tamper");
        let mut writer = Writer::open(&dir, COMPONENT, "test", false, LATE - 5).expect("opened");
        for text in ["one", "two", "three"] {
            writer.append(LATE - 4, "ws-frame", &fields(text)).expect("written");
        }
        let path = writer.path();
        assert_eq!(verify(&path), Ok(false), "an open file verifies, as open");
        let text = fs::read_to_string(&path).expect("read");
        fs::write(&path, text.replace("two", "tw0")).expect("altered");
        assert_eq!(verify(&path), Err("line 4: prev is not the hash of the line before".into()));
        let without: Vec<&str> = text.lines().filter(|l| !l.contains("\"two\"")).collect();
        fs::write(&path, format!("{}\n", without.join("\n"))).expect("removed");
        assert!(verify(&path).is_err_and(|e| e.starts_with("line 3:")));
    }

    #[test]
    fn a_file_left_open_is_sealed_by_the_next_start_and_a_cut_off_one_is_left_alone() {
        let dir = dir("seal");
        let mut first = Writer::open(&dir, COMPONENT, "test", false, LATE - 10).expect("opened");
        first.append(LATE - 9, "ws-frame", &fields("left open")).expect("written");
        let left_open = first.path();
        drop(first);
        let mut second = Writer::open(&dir, COMPONENT, "test", false, LATE - 8).expect("opened again");
        assert_eq!(verify(&left_open), Ok(true), "the earlier file is sealed");
        assert_eq!(lines(&left_open).last().map(|r| r["reason"].clone()), Some(json!("recovered")));
        let segment = second.path();
        assert!(segment.to_string_lossy().ends_with(".2026-09-17.0001.jsonl"), "{}", segment.display());
        // Cut the second file part-way through a line, as a crash mid-write would.
        second.append(LATE - 7, "ws-frame", &fields("cut")).expect("written");
        let whole = fs::read(&segment).expect("read");
        fs::write(&segment, &whole[..whole.len() - 10]).expect("cut");
        let third = Writer::open(&dir, COMPONENT, "test", false, LATE - 6).expect("opened a third time");
        assert_eq!(fs::read(&segment).expect("read").len(), whole.len() - 10, "a cut-off file was changed");
        let name = segment.file_name().and_then(|n| n.to_str()).expect("a name");
        assert_eq!(lines(&third.path())[0]["after"], json!({"file": name, "sealed": false}));
    }

    #[test]
    fn secrets_are_masked() {
        assert_eq!(header_value("authorization", "Bearer dev-market-maker"), "Bearer ***");
        assert_eq!(header_value("cookie", "session=abc; x=y"), "***");
        assert_eq!(header_value("content-type", "application/json"), "application/json");
        assert_eq!(
            mask_tokens(r#"{"op":"login","token":"dev-market-maker","id":"l"}"#),
            r#"{"op":"login","token":"***","id":"l"}"#
        );
        assert_eq!(
            mask_tokens(r#"{"token" : "a\"b", "x": {"token":"c"}}"#),
            r#"{"token" : "***", "x": {"token":"***"}}"#
        );
        // Not JSON, and still masked; and a value that happens to be the word token is not a member.
        assert_eq!(mask_tokens(r#"{"op":"login","token":"secret"#), r#"{"op":"login","token":"***"#);
        assert_eq!(mask_tokens(r#"{"op":"token","x":1}"#), r#"{"op":"token","x":1}"#);
    }

    #[test]
    fn a_full_queue_is_waited_on_and_never_dropped_from() {
        let failures = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&failures);
        let failure: Failure = Arc::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
        });
        let (audit, records) = Audit::queue_only(2, failure);
        audit.record("ws-frame", fields("1"));
        audit.record("ws-frame", fields("2"));
        let (sent, done) = mpsc::channel();
        let waiting = audit.clone();
        thread::spawn(move || {
            waiting.record("ws-frame", fields("3"));
            let _ = sent.send(());
        });
        assert!(done.recv_timeout(Duration::from_millis(200)).is_err(), "a record was queued past a full queue");
        assert_eq!(records.recv().map(|r| r.fields["msg"].clone()).ok(), Some(json!("1")));
        assert!(done.recv_timeout(Duration::from_secs(5)).is_ok(), "the waiting record was never queued");
        let rest: Vec<Value> = records.try_iter().map(|r| r.fields["msg"].clone()).collect();
        assert_eq!(rest, [json!("2"), json!("3")]);
        assert_eq!(failures.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_log_that_cannot_be_written_fails_rather_than_dropping() {
        let failures = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&failures);
        let failure: Failure = Arc::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
        });
        let (audit, records) = Audit::queue_only(1, failure);
        drop(records);
        audit.record("ws-frame", fields("nobody writes this"));
        assert_eq!(failures.load(Ordering::SeqCst), 1);
        // And a directory that cannot be one refuses to start.
        let file = dir("not-a-dir");
        fs::write(&file, b"a file").expect("written");
        assert!(Audit::start(&file, "test", false).is_err());
    }

    #[test]
    fn the_writer_thread_writes_what_is_recorded_and_closes_when_every_handle_has_gone() {
        let dir = dir("thread");
        let audit = Audit::start(&dir, "test", true).expect("started");
        for i in 0..100 {
            audit.record("ws-frame", fields(&i.to_string()));
        }
        drop(audit);
        let path = files(&dir).pop().expect("a file");
        crate::testing::eventually("the log is closed", || verify(&path) == Ok(true));
        assert_eq!(lines(&path).len(), 102);
    }
}

/// What a client sent and was answered, as the audit log has it, against a gateway on a real node (`testing`).
#[cfg(test)]
mod recorded {
    use super::verify;
    use crate::testing::{self, eventually, MAKER_TOKEN, MARKET};
    use serde_json::{json, Value};
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::path::Path;
    use std::time::Duration;
    use tungstenite::stream::MaybeTlsStream;
    use tungstenite::{Message, WebSocket};

    /// Every record in the directory's files, oldest file first.
    fn records(dir: &Path) -> Vec<Value> {
        let mut files: Vec<_> =
            std::fs::read_dir(dir).expect("the audit directory").flatten().map(|e| e.path()).collect();
        files.sort();
        files
            .iter()
            .flat_map(|f| {
                std::fs::read_to_string(f).unwrap_or_default().lines().map(str::to_string).collect::<Vec<_>>()
            })
            .filter_map(|l| serde_json::from_str(&l).ok())
            .collect()
    }

    /// Waits for a record `wanted` accepts, and returns it.
    fn find(dir: &Path, what: &str, wanted: impl Fn(&Value) -> bool) -> Value {
        eventually(what, || records(dir).iter().any(&wanted));
        records(dir).into_iter().find(|r| wanted(r)).expect("found")
    }

    fn request(address: &str, method: &str, path: &str, token: Option<&str>, body: &str) -> u16 {
        let mut stream = TcpStream::connect(address).expect("the gateway accepts a connection");
        stream.set_read_timeout(Some(Duration::from_secs(5))).expect("a read timeout");
        let auth = token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
        let head =
            format!("{method} {path} HTTP/1.1\r\nHost: {address}\r\n{auth}Content-Length: {}\r\n\r\n", body.len());
        stream.write_all(format!("{head}{body}").as_bytes()).expect("sent");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("a response");
        response.split_whitespace().nth(1).and_then(|s| s.parse().ok()).expect("a status")
    }

    fn connect(url: &str) -> WebSocket<MaybeTlsStream<TcpStream>> {
        let (socket, _) = tungstenite::connect(url).expect("a WebSocket");
        if let MaybeTlsStream::Plain(stream) = socket.get_ref() {
            stream.set_read_timeout(Some(Duration::from_secs(5))).expect("a read timeout");
        }
        socket
    }

    /// Reads until a message of `kind` with `id` arrives.
    fn answer(socket: &mut WebSocket<MaybeTlsStream<TcpStream>>, kind: &str, id: &str) -> Value {
        loop {
            if let Message::Text(text) = socket.read().expect("a message") {
                let m: Value = serde_json::from_str(text.as_str()).expect("JSON");
                if m["type"] == kind && m["id"] == id {
                    return m;
                }
            }
        }
    }

    fn order(id: &str) -> Value {
        json!({"clientOrderId": id, "instrumentId": MARKET.to_string(), "action": "buy", "outcome": "yes",
               "price": {"tick": 4}, "quantity": 2})
    }

    fn is(record: &Value, kind: &str, dir: &str) -> bool {
        record["kind"] == kind && record["dir"] == dir
    }

    #[test]
    fn rest_requests_and_refusals_are_recorded_with_their_answers_and_no_token() {
        let venue = testing::venue("audit-rest");
        let dir = &venue.audit_dir;
        assert_eq!(request(&venue.address, "POST", "/v4/orders", Some("not-a-token"), &order("a0").to_string()), 401);
        let refused = find(dir, "the refused request", |r| {
            is(r, "http-request", "in") && r["headers"]["authorization"] == "Bearer ***"
        });
        assert_eq!(refused["line"], "POST /v4/orders HTTP/1.1", "{refused}");
        assert!(refused.get("account").is_none(), "an unknown token names no account: {refused}");
        let answer = find(dir, "the refusal", |r| is(r, "http-response", "out") && r["conn"] == refused["conn"]);
        assert_eq!(answer["status"], 401, "{answer}");
        assert!(answer["body"].as_str().is_some_and(|b| b.contains("UNAUTHORIZED")), "{answer}");

        assert_eq!(request(&venue.address, "POST", "/v4/orders", Some(MAKER_TOKEN), &order("a1").to_string()), 200);
        let placed = find(dir, "the order", |r| {
            is(r, "http-request", "in") && r["body"].as_str().is_some_and(|b| b.contains("\"a1\""))
        });
        assert_eq!((placed["account"].as_str(), placed["half"].as_str()), (Some("1"), Some("private")), "{placed}");
        assert!(placed["peer"].as_str().is_some_and(|p| p.starts_with("127.0.0.1:")), "{placed}");
        let accepted =
            find(dir, "the order's answer", |r| is(r, "http-response", "out") && r["conn"] == placed["conn"]);
        assert_eq!(accepted["status"], 200, "{accepted}");
        assert!(accepted["body"].as_str().is_some_and(|b| b.contains("accepted")), "{accepted}");

        assert_eq!(request(&venue.address, "GET", "/v4/nowhere", None, ""), 404);
        find(dir, "a route that does not exist", |r| is(r, "http-response", "out") && r["status"] == 404);
    }

    #[test]
    fn a_websocket_session_is_recorded_frame_by_frame_with_its_token_masked() {
        let venue = testing::venue("audit-ws");
        let dir = &venue.audit_dir;
        let mut private = connect(&venue.ws_private);
        private
            .send(Message::text(json!({"op": "login", "id": "bad", "token": "not-a-token"}).to_string()))
            .expect("sent");
        answer(&mut private, "error", "bad");
        private.send(Message::text(json!({"op": "login", "id": "l", "token": MAKER_TOKEN}).to_string())).expect("sent");
        let welcome = answer(&mut private, "welcome", "l");
        let mut placing = order("w1");
        placing["op"] = json!("order");
        placing["id"] = json!("o1");
        private.send(Message::text(placing.to_string())).expect("sent");
        answer(&mut private, "reply", "o1");

        let opened = find(dir, "the upgrade", |r| r["kind"] == "ws-open" && r["path"] == "/v4/ws_private");
        let conn = opened["conn"].clone();
        let login = find(dir, "the login", |r| {
            is(r, "ws-frame", "in") && r["conn"] == conn && r["msg"].as_str().is_some_and(|m| m.contains("\"l\""))
        });
        assert!(login["msg"].as_str().is_some_and(|m| m.contains(r#""token":"***""#)), "{login}");
        find(dir, "the refused login", |r| {
            is(r, "ws-frame", "out") && r["msg"].as_str().is_some_and(|m| m.contains("UNAUTHORIZED"))
        });
        let welcomed = find(dir, "the welcome", |r| {
            is(r, "ws-frame", "out") && r["msg"].as_str().is_some_and(|m| m.contains("welcome"))
        });
        assert_eq!(welcomed["account"], "1", "{welcomed}");
        assert_eq!(welcomed["session"], welcome["sessionId"], "{welcomed}");
        find(dir, "the order", |r| is(r, "ws-frame", "in") && r["msg"].as_str().is_some_and(|m| m.contains("\"o1\"")));
        find(dir, "its reply", |r| {
            is(r, "ws-frame", "out") && r["msg"].as_str().is_some_and(|m| m.contains("\"o1\"") && m.contains("reply"))
        });
        find(dir, "the execution streamed to the session", |r| {
            is(r, "ws-frame", "out") && r["msg"].as_str().is_some_and(|m| m.contains("\"execution\""))
        });
        drop(private);
        find(dir, "the session's end", |r| r["kind"] == "ws-close" && r["conn"] == conn);

        // The public half: a subscription and a refusal, and none of the market data or heartbeats.
        let mut public = connect(&venue.ws_public);
        public.send(Message::text(json!({"op": "heartbeat", "id": "h"}).to_string())).expect("sent");
        answer(&mut public, "reply", "h");
        public
            .send(Message::text(
                json!({"op": "subscribe", "id": "s", "topics": [format!("book.{MARKET}")]}).to_string(),
            ))
            .expect("sent");
        public
            .send(Message::text(json!({"op": "subscribe", "id": "bad", "topics": ["nonsense"]}).to_string()))
            .expect("sent");
        answer(&mut public, "error", "bad");
        find(dir, "the subscription", |r| {
            is(r, "ws-frame", "in")
                && r["half"] == "public"
                && r["msg"].as_str().is_some_and(|m| m.contains("subscribe"))
        });
        find(dir, "the refused subscription", |r| {
            is(r, "ws-frame", "out") && r["half"] == "public" && r["msg"].as_str().is_some_and(|m| m.contains("error"))
        });
        let public_records: Vec<Value> = records(dir).into_iter().filter(|r| r["half"] == "public").collect();
        assert!(
            public_records
                .iter()
                .all(|r| r["msg"].as_str().is_none_or(|m| !m.contains("heartbeat") && !m.contains("\"book\""))),
            "market data or a heartbeat was recorded: {public_records:?}"
        );

        let mut files: Vec<_> = std::fs::read_dir(dir).expect("listed").flatten().map(|e| e.path()).collect();
        files.sort();
        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(verify(&files[0]), Ok(false), "the chain verifies, and the file is still open");
        let text = std::fs::read_to_string(&files[0]).expect("read");
        assert!(!text.contains(MAKER_TOKEN), "a token was written in clear");
    }
}
