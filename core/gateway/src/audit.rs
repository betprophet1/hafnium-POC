//! The gateway's edge audit log (interfaces/audit/README.md): every request a client made and what it was answered,
//! refusals above all, since those never reach the journal (blueprint §12, flood protection).
//!
//! The event loops must never wait on a disk, so they only take a record — the connection it is about and what was
//! said — and put it on a bounded queue. One writer thread chains the records into lines and hands everything it took
//! at once to the kernel, then syncs the file for all of it: an order is six records, and a write each was six calls
//! into the kernel for every order. What a signal can lose is bounded by that handover, which happens as soon as the
//! queue is empty. A full queue is waited on rather than dropped from: a loop that cannot queue a record for `STALL`
//! takes the disk to have hung, and a record that cannot be written stops the process (the format document says why).

use crate::limits::monotonic;
use crate::metrics::AuditMetrics;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::cell::{Cell, RefCell};
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const FORMAT: &str = "venue-audit/2";
/// The version of the rules the gateway renders derived frames by (interfaces/audit/README.md, frames derived from
/// the journal). Raised whenever a renderer's output changes, so that a file written under older rules is still
/// reconstructed by the rules it names.
pub const RENDERER: u32 = 1;
pub const COMPONENT: &str = "gateway";
const ZEROS: &str = "0000000000000000000000000000000000000000000000000000000000000000";
/// Records the queue holds: seconds of a busy gateway's private traffic.
pub const CAPACITY: usize = 65_536;
/// How long a loop waits for room on a full queue, counted in millisecond waits, before the disk is taken to have hung.
const STALL_WAITS: u32 = 10_000;
/// Records written between syncs at most.
const BATCH: usize = 1024;
/// What the writer holds before it goes to the kernel. A drain writes what was queued and then flushes, so this only
/// decides how many writes a large drain takes: the full batch of records is about half a megabyte.
const BUFFER: usize = 256 * 1024;
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
        venue_telemetry::log::error("audit log failed, stopping", &[("reason", &reason)]);
        std::process::exit(EXIT_IO_ERROR);
    })
}

/// What a record carries: JSON the loop already had, text from the wire, or a frame the gateway derived from the
/// journal, of which only the length and hash are written. Masking and hashing both read a whole message, so they
/// belong on the writer's thread and not on an event loop.
enum Item {
    Json(&'static str, Value),
    Wire(&'static str, String),
    Digest(String),
}

/// A record's fields, in the order they were put there. Field names are static and the record is written straight
/// into the writer's buffer, so a record about a message costs one allocation: the copy of the message itself.
pub struct Fields(Vec<Item>);

impl Fields {
    /// Room for the fields a record has, taken once.
    pub fn new() -> Self {
        Self(Vec::with_capacity(8))
    }

    pub fn put(&mut self, name: &'static str, value: impl Into<Value>) {
        self.0.push(Item::Json(name, value.into()));
    }

    /// Text a client sent or was sent, kept as it arrived: the writer masks credentials out of it (`push_masked`).
    pub fn wire(&mut self, name: &'static str, text: &str) {
        self.0.push(Item::Wire(name, text.to_string()));
    }

    /// What was sent, by its length and hash rather than its bytes: for an answer whose content is elsewhere. The
    /// hash is taken by the writer, so no loop ever hashes a body it has just sent.
    pub fn digest(&mut self, text: &str) {
        self.0.push(Item::Digest(text.to_string()));
    }

    /// A frame the gateway rendered from a sequenced input: what it was, which input it came from, and the length and
    /// hash of the bytes sent, rather than the bytes themselves. The journal holds what it was rendered from, and
    /// `venue-gateway reconstruct` renders it again and checks it against the hash (interfaces/audit/README.md).
    pub fn derived(&mut self, text: &str, kind: &'static str, sequence: u64) {
        self.put("type", kind);
        self.put("sequence", sequence.to_string());
        self.0.push(Item::Digest(text.to_string()));
    }
}

impl Default for Fields {
    fn default() -> Self {
        Self::new()
    }
}

struct Record {
    micros: u64,
    kind: &'static str,
    fields: Fields,
}

thread_local! {
    /// Whether this thread gathers its records and hands them over together.
    static BATCH_HERE: Cell<bool> = const { Cell::new(false) };
    /// What it has gathered since its last `Audit::flush`.
    static PENDING: RefCell<Vec<Record>> = const { RefCell::new(Vec::new()) };
}

/// Says that this thread takes records in batches, which an event loop calls once at its start: it then hands a whole
/// order's records to the writer at once (`Audit::flush`) rather than waking it for each of the six.
pub fn batch_here() {
    BATCH_HERE.with(|batching| batching.set(true));
}

/// Where records are taken. Cheap to clone; every clone feeds the same writer.
#[derive(Clone)]
pub struct Audit {
    queue: Option<SyncSender<Vec<Record>>>,
    failure: Failure,
    metrics: AuditMetrics,
}

impl Audit {
    /// A log that records nothing, for a gateway built without one: only tests of the order path build those.
    pub fn disabled() -> Self {
        Self { queue: None, failure: exit_on_failure(), metrics: AuditMetrics::default() }
    }

    /// Opens the log in `dir` for `instance`, sealing files an earlier gateway of that instance left open, and starts
    /// its writer. An error means the log cannot be kept, and the gateway must not start.
    #[cfg(test)]
    pub fn start(dir: &Path, instance: &str, fsync: bool) -> io::Result<Self> {
        Self::start_measured(dir, instance, fsync, AuditMetrics::default())
    }

    /// As `start`, with the writer's queue, write and sync times counted in `metrics`.
    pub fn start_measured(dir: &Path, instance: &str, fsync: bool, metrics: AuditMetrics) -> io::Result<Self> {
        Self::start_with(dir, instance, fsync, CAPACITY, exit_on_failure(), metrics)
    }

    pub fn start_with(
        dir: &Path,
        instance: &str,
        fsync: bool,
        capacity: usize,
        failure: Failure,
        metrics: AuditMetrics,
    ) -> io::Result<Self> {
        let mut writer = Writer::open(dir, COMPONENT, instance, fsync, wall_micros())?;
        writer.metrics = metrics.clone();
        let (queue, records) = mpsc::sync_channel(capacity);
        let on_failure = Arc::clone(&failure);
        thread::Builder::new().name("gateway-audit".into()).spawn(move || {
            if let Err(e) = writer.run(&records) {
                on_failure(&format!("writing {}: {e}", writer.path().display()));
            }
        })?;
        Ok(Self { queue: Some(queue), failure, metrics })
    }

    /// A handle whose records wait on `capacity` places for whoever holds the receiver: for testing what a full queue
    /// does.
    #[cfg(test)]
    fn queue_only(capacity: usize, failure: Failure) -> (Self, Receiver<Vec<Record>>) {
        let (queue, records) = mpsc::sync_channel(capacity);
        (Self { queue: Some(queue), failure, metrics: AuditMetrics::default() }, records)
    }

    /// Takes a record of `kind` now. On a thread that batches (`batch_here`), it waits with what that thread has
    /// taken since its last `flush`; anywhere else it goes to the writer on its own. Neither waits on a disk.
    pub fn record(&self, kind: &'static str, fields: Fields) {
        if self.queue.is_none() {
            return;
        }
        let record = Record { micros: wall_micros(), kind, fields };
        if BATCH_HERE.with(Cell::get) {
            PENDING.with(|pending| pending.borrow_mut().push(record));
        } else {
            self.send(vec![record]);
        }
    }

    /// Hands the writer what this thread has taken. An event loop calls this before it waits for anything else, so a
    /// record is never held back by a quiet socket.
    pub fn flush(&self) {
        if self.queue.is_none() {
            return;
        }
        let batch = PENDING.with(|pending| std::mem::take(&mut *pending.borrow_mut()));
        if !batch.is_empty() {
            self.send(batch);
        }
    }

    /// Puts a batch on the queue, waiting while it is full and failing the log if it stays full.
    fn send(&self, batch: Vec<Record>) {
        let Some(queue) = &self.queue else { return };
        let taken = i64::try_from(batch.len()).unwrap_or(i64::MAX);
        let mut batch = batch;
        for wait in 0..STALL_WAITS {
            match queue.try_send(batch) {
                Ok(()) => {
                    self.metrics.queue_depth.inc_by(taken);
                    return;
                }
                Err(TrySendError::Full(back)) => {
                    if wait == 0 {
                        self.metrics.queue_full_waits.inc();
                    }
                    batch = back;
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

/// `YYYY-MM-DDTHH:MM:SS.ffffffZ`. Records go straight into the writer's buffer, so only the tests of the format
/// build one of these as a string.
#[cfg(test)]
fn timestamp(micros: u64) -> String {
    let mut out = Vec::new();
    push_timestamp(&mut out, micros);
    String::from_utf8(out).unwrap_or_default()
}

/// `YYYY-MM-DDTHH:MM:SS.ffffffZ`, appended.
fn push_timestamp(out: &mut Vec<u8>, micros: u64) {
    let (day, rest) = (micros / MICROS_PER_DAY, micros % MICROS_PER_DAY);
    let (y, m, d) = civil(day);
    let seconds = rest / 1_000_000;
    let _ = write!(
        out,
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:06}Z",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60,
        rest % 1_000_000
    );
}

/// The SHA-256 of `bytes`, in lower-case hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut out = String::new();
    push_hex(&mut out, Sha256::digest(bytes).as_slice());
    out
}

/// `bytes` as lower-case hex, replacing what `out` held: the chain's previous hash is kept in one string.
fn push_hex(out: &mut String, bytes: &[u8]) {
    out.clear();
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
}

/// `text` as a JSON string: quoted, and escaped where JSON requires it.
fn push_quoted(out: &mut Vec<u8>, text: &str) {
    out.push(b'"');
    push_escaped(out, text);
    out.push(b'"');
}

/// The body of a JSON string. Bytes above ASCII are UTF-8 and go as they are; only what JSON forbids is escaped.
fn push_escaped(out: &mut Vec<u8>, text: &str) {
    let bytes = text.as_bytes();
    let mut copied = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let escape: &[u8] = match b {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            0x08 => b"\\b",
            0x0c => b"\\f",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            0x00..=0x1f => b"",
            _ => continue,
        };
        out.extend_from_slice(&bytes[copied..i]);
        if escape.is_empty() {
            let _ = write!(out, "\\u{b:04x}");
        } else {
            out.extend_from_slice(escape);
        }
        copied = i + 1;
    }
    out.extend_from_slice(&bytes[copied..]);
}

/// Member names whose value is a credential, lower-case. Matched whatever their case, so a client that writes
/// `Token` — which the gateway refuses, and refusals are logged in full — still keeps its token out of the log.
const CREDENTIALS: [&str; 7] = ["token", "secret", "credential", "password", "passphrase", "apikey", "api_key"];

/// `text` as a JSON string, with the value of every credential member replaced by `***`. Found textually rather than
/// by parsing, so a frame that is not JSON at all is masked too.
fn push_masked(out: &mut Vec<u8>, text: &str) {
    out.push(b'"');
    let bytes = text.as_bytes();
    let (mut i, mut copied) = (0, 0);
    while let Some(found) = bytes[i..].iter().position(|&b| b == b'"') {
        let at = i + found;
        match credential_value(bytes, at) {
            Some((start, end)) => {
                push_escaped(out, &text[copied..start]);
                out.extend_from_slice(b"***");
                (i, copied) = (end, end);
            }
            None => i = at + 1,
        }
    }
    push_escaped(out, &text[copied..]);
    out.push(b'"');
}

/// Where the value of a credential member starts and ends, given the quote at `at` that may open the member's name.
fn credential_value(bytes: &[u8], at: usize) -> Option<(usize, usize)> {
    let name_end = at + 1 + bytes[at + 1..].iter().position(|&b| b == b'"')?;
    let name = std::str::from_utf8(bytes.get(at + 1..name_end)?).ok()?;
    if !CREDENTIALS.iter().any(|c| name.eq_ignore_ascii_case(c)) {
        return None;
    }
    let mut i = name_end + 1;
    while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    if bytes.get(i) != Some(&b':') {
        return None;
    }
    i += 1;
    while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    if bytes.get(i) != Some(&b'"') {
        return None;
    }
    let start = i + 1;
    let mut end = start;
    while end < bytes.len() && bytes[end] != b'"' {
        end += if bytes[end] == b'\\' { 2 } else { 1 };
    }
    Some((start, end.min(bytes.len())))
}

/// What every line carries besides its own fields: where it sits in its file, and what it chains to.
struct Head<'a> {
    micros: u64,
    n: u64,
    component: &'a str,
    instance: &'a str,
    kind: &'a str,
    prev: &'a str,
}

/// One record as its line, without the newline, appended to `out`: the writer keeps one buffer for every line it
/// ever writes, so a record of any size is written without taking memory for it.
fn push_line(out: &mut Vec<u8>, head: &Head<'_>, fields: &Fields) {
    let (n, prev) = (head.n, head.prev);
    out.extend_from_slice(b"{\"ts\":\"");
    push_timestamp(out, head.micros);
    let _ = write!(out, "\",\"n\":{n},\"component\":");
    push_quoted(out, head.component);
    out.extend_from_slice(b",\"instance\":");
    push_quoted(out, head.instance);
    out.extend_from_slice(b",\"kind\":");
    push_quoted(out, head.kind);
    for item in &fields.0 {
        out.push(b',');
        match item {
            Item::Json(name, value) => {
                push_quoted(out, name);
                out.push(b':');
                serde_json::to_writer(&mut *out, value).expect("a buffer never fails a write");
            }
            Item::Wire(name, text) => {
                push_quoted(out, name);
                out.push(b':');
                push_masked(out, text);
            }
            // The frame itself is not written: the journal holds what it was rendered from, and the hash says which
            // rendering of it was sent.
            Item::Digest(text) => {
                let _ = write!(out, "\"length\":{},\"sha256\":\"", text.len());
                for b in Sha256::digest(text.as_bytes()) {
                    let _ = write!(out, "{b:02x}");
                }
                out.push(b'"');
            }
        }
    }
    out.extend_from_slice(b",\"prev\":\"");
    out.extend_from_slice(prev.as_bytes());
    out.extend_from_slice(b"\"}");
}

/// The file being written, and what the next line chains to.
struct Writer {
    dir: PathBuf,
    component: String,
    instance: String,
    fsync: bool,
    file: BufWriter<File>,
    name: String,
    day: u64,
    next: u64,
    prev: String,
    /// The line being written, kept from record to record so no record allocates for its own bytes.
    line: Vec<u8>,
    /// The instance's lock, held for as long as this writer lives (`lock_instance`).
    _lock: File,
    metrics: AuditMetrics,
}

/// Takes the exclusive lock that says a live process is writing `instance`'s files in `dir`, and keeps it for as long
/// as the returned file is open; the operating system lets it go when the process ends, however it ends. Two live
/// gateways given the same directory and instance — two processes on one host with the default instance, or replicas
/// sharing a volume and a host name — would otherwise each seal and append to the other's open file and break both
/// chains, so the second is refused and does not start.
fn lock_instance(dir: &Path, component: &str, instance: &str) -> io::Result<File> {
    let path = dir.join(format!(".{component}.{instance}.lock"));
    let file = OpenOptions::new().create(true).truncate(false).write(true).open(&path)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(fs::TryLockError::WouldBlock) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "another live process is writing audit instance {instance} in {} ({}): give each gateway its own \
                 --audit-instance",
                dir.display(),
                path.display()
            ),
        )),
        Err(fs::TryLockError::Error(e)) => Err(e),
    }
}

impl Writer {
    fn open(dir: &Path, component: &str, instance: &str, fsync: bool, micros: u64) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let instance = sanitise(instance);
        // Before anything is sealed: a file another live process has open is not an earlier process's to seal.
        let lock = lock_instance(dir, component, &instance)?;
        let after = seal_earlier_files(dir, component, &instance, fsync, micros)?;
        let (file, name) = create_file(dir, component, &instance, micros / MICROS_PER_DAY)?;
        let file = BufWriter::with_capacity(BUFFER, file);
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
            line: Vec::new(),
            _lock: lock,
            metrics: AuditMetrics::default(),
        };
        writer.begin(micros, after)?;
        Ok(writer)
    }

    fn path(&self) -> PathBuf {
        self.dir.join(&self.name)
    }

    /// Writes what arrives until every handle has gone, then closes the file.
    fn run(&mut self, records: &Receiver<Vec<Record>>) -> io::Result<()> {
        loop {
            match records.recv_timeout(IDLE) {
                Ok(first) => {
                    let started = monotonic();
                    let mut written = 0;
                    let mut batch = first;
                    loop {
                        for record in batch.drain(..) {
                            self.append(record.micros, record.kind, &record.fields)?;
                            written += 1;
                        }
                        if written >= BATCH as i64 {
                            break;
                        }
                        match records.try_recv() {
                            Ok(next) => batch = next,
                            Err(_) => break,
                        }
                    }
                    let appended = monotonic();
                    self.metrics.queue_depth.dec_by(written);
                    self.metrics.records.inc_by(written.unsigned_abs());
                    self.metrics.write.observe_duration(appended.duration_since(started));
                    self.sync()?;
                    if self.fsync {
                        self.metrics.fsync.observe_duration(monotonic().duration_since(appended));
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.rotate_if_past_midnight(wall_micros())?;
                }
                Err(RecvTimeoutError::Disconnected) => return self.close(wall_micros(), "shutdown"),
            }
        }
    }

    /// Appends one record, first rotating if it belongs to a later day than the file.
    fn append(&mut self, micros: u64, kind: &str, fields: &Fields) -> io::Result<()> {
        self.rotate_if_past_midnight(micros)?;
        self.write_line(micros, kind, fields)
    }

    /// One record, built in the buffer this writer keeps. It goes to the kernel with everything else the drain took
    /// (`sync`), not on its own: a record cost a write each, and there are six of them for every order.
    fn write_line(&mut self, micros: u64, kind: &str, fields: &Fields) -> io::Result<()> {
        let mut line = std::mem::take(&mut self.line);
        line.clear();
        let head =
            Head { micros, n: self.next, component: &self.component, instance: &self.instance, kind, prev: &self.prev };
        push_line(&mut line, &head, fields);
        let digest = Sha256::digest(&line);
        line.push(b'\n');
        let written = self.file.write_all(&line);
        self.line = line;
        written?;
        push_hex(&mut self.prev, digest.as_slice());
        self.next += 1;
        Ok(())
    }

    /// Hands what the drain wrote to the kernel, and to the disk when the log is synced. The flush is not conditional:
    /// after it, a record is in the kernel and survives the gateway being killed, which is what bounds what a signal
    /// can lose to the records of the drain it interrupted. Only the machine going down loses what is not synced.
    fn sync(&mut self) -> io::Result<()> {
        self.file.flush()?;
        if self.fsync {
            self.file.get_ref().sync_data()?;
        }
        Ok(())
    }

    fn rotate_if_past_midnight(&mut self, micros: u64) -> io::Result<()> {
        if micros / MICROS_PER_DAY > self.day {
            let closed = self.name.clone();
            self.metrics.rotations.inc();
            self.close(micros, "rotate")?;
            let last = self.prev.clone();
            self.open_file(micros, Some(json!({"file": closed, "last": last})))?;
        }
        Ok(())
    }

    fn open_file(&mut self, micros: u64, after: Option<Value>) -> io::Result<()> {
        let day = micros / MICROS_PER_DAY;
        let (file, name) = create_file(&self.dir, &self.component, &self.instance, day)?;
        (self.file, self.name) = (BufWriter::with_capacity(BUFFER, file), name);
        self.day = day;
        self.next = 0;
        self.prev = ZEROS.to_string();
        self.begin(micros, after)
    }

    /// Writes a new file's open record, and makes the file durable.
    fn begin(&mut self, micros: u64, after: Option<Value>) -> io::Result<()> {
        let mut open = Fields::new();
        open.put("format", FORMAT);
        open.put("file", self.name.clone());
        open.put("pid", std::process::id());
        open.put("renderer", RENDERER);
        open.put("build", env!("CARGO_PKG_VERSION"));
        if let Some(after) = after {
            open.put("after", after);
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
        let mut close = Fields::new();
        close.put("reason", reason);
        close.put("records", self.next);
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
            venue_telemetry::log::warn(
                "audit file ends in a cut-off record; left as it is",
                &[("file", &path.display())],
            );
            after = Some(json!({"file": name, "sealed": false}));
            continue;
        };
        if record.as_ref().is_some_and(|r| r["kind"] == "close") {
            after = Some(json!({"file": name, "last": sha256_hex(&last)}));
            continue;
        }
        let mut close = Fields::new();
        close.put("reason", "recovered");
        close.put("records", n + 1);
        let mut text = Vec::new();
        let head = Head { micros, n: n + 1, component, instance, kind: "close", prev: &sha256_hex(&last) };
        push_line(&mut text, &head, &close);
        let sealed = sha256_hex(&text);
        text.push(b'\n');
        let mut file = OpenOptions::new().append(true).open(&path)?;
        file.write_all(&text)?;
        if fsync {
            file.sync_data()?;
        }
        venue_telemetry::log::info("sealed an audit file an earlier process left open", &[("file", &path.display())]);
        after = Some(json!({"file": name, "last": sealed}));
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

    fn fields(text: &str) -> Fields {
        let mut fields = Fields::new();
        fields.put("dir", "in");
        fields.wire("msg", text);
        fields
    }

    /// The text of a record's wire field, for tests that read records off the queue rather than out of a file.
    fn wire_of(record: &Record) -> String {
        record
            .fields
            .0
            .iter()
            .find_map(|item| match item {
                Item::Wire(_, text) => Some(text.clone()),
                Item::Json(..) | Item::Digest(_) => None,
            })
            .unwrap_or_default()
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
        // As a drain does once its queue is empty: until then the records are the writer's, not the file's.
        writer.sync().expect("handed to the kernel");
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
        // Cut the second file part-way through a line, as a crash mid-write would, and the process with it.
        second.append(LATE - 7, "ws-frame", &fields("cut")).expect("written");
        drop(second);
        let whole = fs::read(&segment).expect("read");
        fs::write(&segment, &whole[..whole.len() - 10]).expect("cut");
        let third = Writer::open(&dir, COMPONENT, "test", false, LATE - 6).expect("opened a third time");
        assert_eq!(fs::read(&segment).expect("read").len(), whole.len() - 10, "a cut-off file was changed");
        let name = segment.file_name().and_then(|n| n.to_str()).expect("a name");
        assert_eq!(lines(&third.path())[0]["after"], json!({"file": name, "sealed": false}));
    }

    /// Only one live process writes an instance: a second writer of the same instance in the same directory is refused
    /// and touches nothing, another instance beside it is not, and the instance is free again once its writer has gone.
    #[test]
    fn an_instance_another_live_process_is_writing_is_refused() {
        let dir = dir("instance-lock");
        let mut first = Writer::open(&dir, COMPONENT, "test", false, LATE - 10).expect("opened");
        first.append(LATE - 9, "ws-frame", &fields("mine")).expect("written");
        let before = fs::read(first.path()).expect("read");
        let refused = Writer::open(&dir, COMPONENT, "test", false, LATE - 8).err().expect("a second writer refused");
        assert!(refused.to_string().contains("--audit-instance"), "{refused}");
        assert_eq!(fs::read(first.path()).expect("read"), before, "the live file was changed");
        assert_eq!(files(&dir).len(), 1, "a refused writer made a file");
        assert!(Writer::open(&dir, COMPONENT, "other", false, LATE - 8).is_ok(), "another instance was refused");
        first.append(LATE - 7, "ws-frame", &fields("still mine")).expect("written");
        assert_eq!(verify(&first.path()), Ok(false), "the live file's chain broke");
        drop(first);
        assert!(Writer::open(&dir, COMPONENT, "test", false, LATE - 6).is_ok(), "the instance stayed locked");
    }

    /// What a wire field holds in the file: masked and escaped as the writer does it, read back as JSON, so these
    /// tests check the escaping as well as the masking.
    fn masked(text: &str) -> String {
        let mut out = Vec::new();
        push_masked(&mut out, text);
        serde_json::from_slice::<String>(&out).expect("a JSON string")
    }

    #[test]
    fn secrets_are_masked() {
        assert_eq!(header_value("authorization", "Bearer dev-market-maker"), "Bearer ***");
        assert_eq!(header_value("cookie", "session=abc; x=y"), "***");
        assert_eq!(header_value("content-type", "application/json"), "application/json");
        assert_eq!(
            masked(r#"{"op":"login","token":"dev-market-maker","id":"l"}"#),
            r#"{"op":"login","token":"***","id":"l"}"#
        );
        assert_eq!(masked(r#"{"token" : "a\"b", "x": {"token":"c"}}"#), r#"{"token" : "***", "x": {"token":"***"}}"#);
        assert_eq!(
            masked(r#"{"keyId":"ak_0123456789abcdef","secret":"00ff","credential":"ak_0123456789abcdef.00ff"}"#),
            r#"{"keyId":"ak_0123456789abcdef","secret":"***","credential":"***"}"#
        );
        // Not JSON, and still masked; and a value that happens to be the word token is not a member.
        assert_eq!(masked(r#"{"op":"login","token":"secret"#), r#"{"op":"login","token":"***"#);
        assert_eq!(masked(r#"{"op":"token","x":1}"#), r#"{"op":"token","x":1}"#);
    }

    #[test]
    fn a_credential_is_masked_whatever_it_is_called() {
        // A login the gateway refuses for its field name is recorded in full, so the name it used must be masked too.
        assert_eq!(masked(r#"{"op":"login","Token":"ak_1.ff"}"#), r#"{"op":"login","Token":"***"}"#);
        assert_eq!(masked(r#"{"PASSWORD":"hunter2"}"#), r#"{"PASSWORD":"***"}"#);
        for name in ["password", "passphrase", "apiKey", "api_key"] {
            let frame = format!(r#"{{"{name}":"s3cret"}}"#);
            assert_eq!(masked(&frame), format!(r#"{{"{name}":"***"}}"#), "{name}");
        }
    }

    #[test]
    fn a_frame_is_escaped_as_json_requires() {
        // A line break in a frame must not become a line break in the file: one record is one line.
        const FRAME: &str = "a\"b\\c\nd\te\u{1}f\u{20ac}";
        let mut out = Vec::new();
        push_masked(&mut out, FRAME);
        let written = String::from_utf8(out).expect("UTF-8");
        assert_eq!(written, serde_json::to_string(FRAME).expect("JSON"), "not escaped as serde_json escapes");
        assert_eq!(masked(FRAME), FRAME, "a frame does not come back as it went in");
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
        assert_eq!(records.recv().ok().as_deref().and_then(<[Record]>::first).map(wire_of), Some("1".to_string()));
        assert!(done.recv_timeout(Duration::from_secs(5)).is_ok(), "the waiting record was never queued");
        let rest: Vec<String> = records.try_iter().flatten().map(|r| wire_of(&r)).collect();
        assert_eq!(rest, ["2", "3"]);
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
    use crate::testing::{self, eventually, MAKER_TOKEN, MARKET, RETAIL_TOKEN};
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
        exchange(address, method, path, token, body).0
    }

    /// A request's status and the body of its response, as sent.
    fn exchange(address: &str, method: &str, path: &str, token: Option<&str>, body: &str) -> (u16, String) {
        exchange_with(address, method, path, token, "", body)
    }

    /// The same, with `headers`, each line ending `\r\n`, added to the request.
    fn exchange_with(
        address: &str,
        method: &str,
        path: &str,
        token: Option<&str>,
        headers: &str,
        body: &str,
    ) -> (u16, String) {
        let mut stream = TcpStream::connect(address).expect("the gateway accepts a connection");
        stream.set_read_timeout(Some(Duration::from_secs(5))).expect("a read timeout");
        let auth = token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {address}\r\n{auth}{headers}Content-Length: {}\r\n\r\n",
            body.len()
        );
        stream.write_all(format!("{head}{body}").as_bytes()).expect("sent");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("a response");
        let status = response.split_whitespace().nth(1).and_then(|s| s.parse().ok()).expect("a status");
        let body = response.split_once("\r\n\r\n").map_or("", |(_, body)| body);
        (status, body.to_string())
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

    /// The next frame of `kind` the client is sent, as text, so a test can check what the log says about it against
    /// the bytes that were actually sent. The socket's read timeout is short, and a loaded machine can take longer
    /// than that to answer, so a timeout is waited through rather than failed on, as `eventually` does.
    fn streamed(socket: &mut WebSocket<MaybeTlsStream<TcpStream>>, kind: &str) -> String {
        // Bounded by time rather than by a count of timeouts. A read that has half a frame already returns
        // `WouldBlock` at once rather than waiting out the socket's timeout, so counting timeouts can spend a
        // budget meant for minutes in milliseconds — which is how this failed under a full `./check` and passed on
        // its own. The budget matches `testing::eventually`: 30 seconds is far longer than a frame crossing the node
        // and coming back through the feed takes, even on a machine running every other test at the same time.
        let deadline = crate::limits::monotonic() + Duration::from_secs(30);
        loop {
            match socket.read() {
                Ok(Message::Text(text)) => {
                    let m: Value = serde_json::from_str(text.as_str()).expect("JSON");
                    if m["type"] == kind {
                        return text.to_string();
                    }
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
                        && crate::limits::monotonic() < deadline =>
                {
                    // Nothing yet. A spin is not wanted either: the frame comes from another thread.
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("no {kind} frame arrived: {e}"),
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

        // Nothing is trusted by default: forwarding headers from any peer name no client.
        let forged = "X-Forwarded-For: 198.51.100.1\r\nForwarded: for=198.51.100.2\r\n";
        assert_eq!(exchange_with(&venue.address, "GET", "/v4/account", Some(MAKER_TOKEN), forged, "").0, 200);
        let asked = find(dir, "the forged request", |r| {
            is(r, "http-request", "in") && r["headers"]["x-forwarded-for"] == "198.51.100.1"
        });
        assert!(asked.get("client").is_none(), "a header from an untrusted peer was believed: {asked}");
        let answered = find(dir, "its answer", |r| is(r, "http-response", "out") && r["conn"] == asked["conn"]);
        assert!(answered.get("client").is_none(), "{answered}");
    }

    #[test]
    fn behind_a_trusted_proxy_records_name_the_client_it_forwarded_for() {
        // The test's connections come from 127.0.0.1, which plays the ingress.
        let venue = testing::venue_trusting("audit-proxy", "127.0.0.1, 10.42.0.0/16");
        let dir = &venue.audit_dir;
        let hops = "X-Forwarded-For: 203.0.113.9, 198.51.100.77\r\nX-Forwarded-For: 10.42.0.3\r\n";
        assert_eq!(exchange_with(&venue.address, "GET", "/v4/markets", None, hops, "").0, 200);
        let asked = find(dir, "the public request", |r| {
            is(r, "http-request", "in") && r["half"] == "public" && r["line"] == "GET /v4/markets HTTP/1.1"
        });
        assert_eq!(asked["client"], "198.51.100.77", "the right-most entry no trusted proxy wrote: {asked}");
        assert!(asked["peer"].as_str().is_some_and(|p| p.starts_with("127.0.0.1:")), "the peer is the proxy: {asked}");
        let answered = find(dir, "its answer", |r| is(r, "http-response", "out") && r["conn"] == asked["conn"]);
        assert_eq!(answered["client"], "198.51.100.77", "{answered}");

        let forwarded = "Forwarded: for=192.0.2.60;proto=http\r\n";
        assert_eq!(exchange_with(&venue.address, "GET", "/v4/account", Some(MAKER_TOKEN), forwarded, "").0, 200);
        find(dir, "the private request", |r| {
            is(r, "http-request", "in") && r["half"] == "private" && r["client"] == "192.0.2.60"
        });

        assert_eq!(request(&venue.address, "GET", "/v4/nowhere", None, ""), 404);
        let direct = find(dir, "a request no proxy forwarded", |r| {
            is(r, "http-request", "in") && r["line"] == "GET /v4/nowhere HTTP/1.1"
        });
        assert!(direct.get("client").is_none(), "{direct}");

        // A WebSocket: the client is taken from its upgrade request, and every record of the session carries it.
        let mut upgrade =
            tungstenite::client::IntoClientRequest::into_client_request(venue.ws_private.as_str()).expect("a request");
        upgrade.headers_mut().insert("x-forwarded-for", "198.51.100.7".parse().expect("a header value"));
        let (mut private, _) = tungstenite::connect(upgrade).expect("a WebSocket");
        if let MaybeTlsStream::Plain(stream) = private.get_ref() {
            stream.set_read_timeout(Some(Duration::from_secs(5))).expect("a read timeout");
        }
        private.send(Message::text(json!({"op": "login", "id": "l", "token": MAKER_TOKEN}).to_string())).expect("sent");
        answer(&mut private, "welcome", "l");
        let opened = find(dir, "the upgrade", |r| r["kind"] == "ws-open" && r["path"] == "/v4/ws_private");
        assert_eq!(opened["client"], "198.51.100.7", "{opened}");
        let welcomed = find(dir, "the welcome", |r| {
            is(r, "ws-frame", "out")
                && r["conn"] == opened["conn"]
                && r["msg"].as_str().is_some_and(|m| m.contains("welcome"))
        });
        assert_eq!(welcomed["client"], "198.51.100.7", "{welcomed}");
        drop(private);
        let closed = find(dir, "the session's end", |r| r["kind"] == "ws-close" && r["conn"] == opened["conn"]);
        assert_eq!(closed["client"], "198.51.100.7", "{closed}");
        let public = connect(&venue.ws_public);
        drop(public);
        let viewer =
            find(dir, "a viewer no proxy forwarded", |r| r["kind"] == "ws-open" && r["path"] == "/v4/ws_public");
        assert!(viewer.get("client").is_none(), "{viewer}");
    }

    #[test]
    fn the_public_half_records_what_it_answered_by_length_and_hash_and_a_refusal_in_full() {
        let venue = testing::venue("audit-public-rest");
        let dir = &venue.audit_dir;
        let (status, markets) = exchange(&venue.address, "GET", "/v4/markets", None, "");
        assert_eq!(status, 200);
        let (status, book) = exchange(&venue.address, "GET", &format!("/v4/markets/{MARKET}/book"), None, "");
        assert_eq!(status, 200);
        for (path, sent) in [("/v4/markets".to_string(), &markets), (format!("/v4/markets/{MARKET}/book"), &book)] {
            let asked = find(dir, &format!("the request for {path}"), |r| {
                is(r, "http-request", "in") && r["line"] == format!("GET {path} HTTP/1.1")
            });
            assert_eq!(asked["half"], "public", "{asked}");
            assert!(asked["headers"]["host"].is_string(), "the request is recorded with its headers: {asked}");
            let answered = find(dir, "its answer", |r| is(r, "http-response", "out") && r["conn"] == asked["conn"]);
            assert_eq!(answered["status"], 200, "{answered}");
            assert!(answered.get("body").is_none(), "a public answer's body was recorded: {answered}");
            assert_eq!(answered["length"], sent.len(), "{answered}");
            assert_eq!(answered["sha256"], super::sha256_hex(sent.as_bytes()), "{answered}");
        }

        // Refusals and errors on the public half keep their bodies.
        let (status, missing) = exchange(&venue.address, "GET", "/v4/markets/99999/book", None, "");
        assert_eq!(status, 404);
        let refused =
            find(dir, "the refusal", |r| is(r, "http-response", "out") && r["half"] == "public" && r["status"] == 404);
        assert_eq!(refused["body"], missing, "{refused}");
        assert!(refused.get("sha256").is_none(), "{refused}");

        // The private half keeps every body.
        let (status, account) = exchange(&venue.address, "GET", "/v4/account", Some(MAKER_TOKEN), "");
        assert_eq!(status, 200);
        let private = find(dir, "the account's answer", |r| {
            is(r, "http-response", "out") && r["half"] == "private" && r["status"] == 200
        });
        assert_eq!(private["body"], account, "{private}");

        let file = std::fs::read_dir(dir)
            .expect("listed")
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .expect("a log file");
        assert_eq!(verify(&file), Ok(false), "the chain verifies");
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
        // Cross it deliberately rather than waiting for one of the venue's bots to do it. An execution needs a
        // counterparty, and leaving that to whichever bot happened to be running made this test a race with them:
        // it failed three times under a loaded `./check` and passed alone every time, which is what a race looks
        // like from the outside. Retail sells into the resting buy, and the fill is then this test's own doing.
        let mut crossing = order("x1");
        crossing["action"] = json!("sell");
        assert_eq!(
            request(&venue.address, "POST", "/v4/orders", Some(RETAIL_TOKEN), &crossing.to_string()),
            200,
            "the crossing sell was refused, so no execution could follow"
        );

        // What the engine did is recorded by reference to the journal: what the frame was, the input it was rendered
        // from, and the hash of the bytes the client was sent (interfaces/audit/README.md).
        let execution = streamed(&mut private, "execution");
        let sent: Value = serde_json::from_str(&execution).expect("JSON");
        // The record of *this* frame, found by the connection it went out on and the sequence it carries. Matching
        // on "an out frame of type execution" was the flake: the venue's bots trade as well, so the first such
        // record in the log is not necessarily the frame this session just read, and the test then compared one
        // execution's length against another's. It held while the machine was quiet and the two coincided.
        let referenced = find(dir, "the execution streamed to the session", |r| {
            is(r, "ws-frame", "out")
                && r["type"] == "execution"
                && r["conn"] == conn
                && r["sequence"] == sent["execution"]["sequence"]
        });
        assert!(referenced.get("msg").is_none(), "a frame from the journal was written in full: {referenced}");
        assert_eq!(referenced["length"], execution.len(), "{referenced}");
        assert_eq!(referenced["sha256"], super::sha256_hex(execution.as_bytes()), "{referenced}");
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

        // The log's files, beside the lock that says a live gateway writes them.
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .expect("listed")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .collect();
        files.sort();
        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(verify(&files[0]), Ok(false), "the chain verifies, and the file is still open");
        let text = std::fs::read_to_string(&files[0]).expect("read");
        assert!(!text.contains(MAKER_TOKEN), "a token was written in clear");
    }
}
