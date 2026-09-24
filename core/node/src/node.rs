//! The node: recovery from the journal, the ingress loop, the feed, and replay.
//!
//! Every input is stamped, appended to the journal and flushed before the engine applies it, so a journal is
//! always a prefix of what the engine was told, and a restart replays it to the same state and the same result
//! log (interface rules, `interfaces/README.md`). A fail-stop (spec §18) ends the process; restarting replays
//! the journal and stops at the same input, which is the behaviour E-18.2 requires.
//!
//! The feed is a development stand-in for interface 12: a reader receives every input with its results, from the
//! start of the journal, then live.

use crate::frames::{read_frame, template_of, write_frame};
use crate::metrics::{NodeMetrics, Subscriber};
use crate::sequencer::{self, Sequencer};
use crate::snapshots;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, ErrorKind, Read, Seek, SeekFrom, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use venue_engine::frame::{self, Kind};
use venue_engine::state::Capacities;
use venue_engine::{Engine, EngineConfig, ResultSink};
use venue_harness::memory::OwnedMemory;
use venue_journal::snapshot_codec::SBE_TEMPLATE_ID as SNAPSHOT;
use venue_journal::time_tick_codec::SBE_TEMPLATE_ID as TIME_TICK;
use venue_telemetry::log;

/// A `Snapshot` frame (template 53) is not an input: it is written into the journal between inputs, but the
/// engine does not apply it and its `record.sequence` names the input it follows rather than one of its own
/// (interfaces/sbe/venue-journal.xml, Snapshot). Every reader that walks a journal frame by frame — recovery, a
/// full replay, a subscriber's backlog — skips it by this check instead.
pub(crate) fn is_snapshot(frame: &[u8]) -> bool {
    template_of(frame) == Some(SNAPSHOT)
}

/// Whether `journal`'s first real input is still sequence 1 — `false` once `cut` has ever removed it, and
/// `false` for a journal `seed` founded on a snapshot with nothing yet appended after it. A genesis replay of
/// either would fail-stop confusingly (E-4.1: the engine expects sequence 1 next), or silently ignore the
/// snapshot and construct a default engine instead, so this is what lets a caller refuse clearly rather than do
/// either. Skips past any `Snapshot` frame to find the first real input rather than assuming it is the very
/// first frame in the file — `cut` keeps every frame's original relative order, and a snapshot embedded after
/// later inputs already existed can physically sit *after* them (§3.7), so it is not reliably the first frame
/// even in a cut journal. Only a journal with no frames at all — a node that has truly never run — starts at
/// genesis with nothing to contradict it; one with a snapshot and nothing else does not, because it was founded
/// somewhere other than sequence 1 on purpose.
fn starts_at_genesis(journal: &Path) -> Result<bool, Error> {
    let mut reader = BufReader::new(File::open(journal)?);
    let mut buf = Vec::new();
    let mut saw_snapshot = false;
    while let Some(n) = read_frame(&mut reader, &mut buf)? {
        if is_snapshot(&buf[..n]) {
            saw_snapshot = true;
            continue;
        }
        return Ok(u64_at(&buf[..n], 8) == 1);
    }
    Ok(!saw_snapshot)
}

/// Whether `journal` carries no real input at all — only `Snapshot` frames, or nothing. This is narrower than
/// `starts_at_genesis`: an ordinary journal with a snapshot merely appended at its *tail* (`embed` with no `--at`)
/// still has every one of its real inputs before that snapshot, and `results.sbe` still owes each of them a
/// result — that case must keep falling back to a full genesis replay, same as before. This is only true of a
/// journal `seed` produced (or the degenerate case of a journal `cut` all the way to nothing but its own latest
/// snapshot): there is no real input anywhere in it, before or after, so there is nothing a lost or absent
/// `results.sbe` could owe a result for, and the snapshot is safe to trust unconditionally.
fn founded_only(journal: &Path) -> Result<bool, Error> {
    let mut reader = BufReader::new(File::open(journal)?);
    let mut buf = Vec::new();
    while let Some(n) = read_frame(&mut reader, &mut buf)? {
        if !is_snapshot(&buf[..n]) {
            return Ok(false);
        }
    }
    Ok(true)
}

pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub const JOURNAL: &str = "journal.sbe";
pub const RESULTS: &str = "results.sbe";

/// Seeds the engine's ID tables. Not observable in results.
const TABLE_SEED: u64 = 0x5eed;

pub struct Config {
    pub listen: SocketAddr,
    /// Where readers subscribe to every input and its results.
    pub feed: SocketAddr,
    pub data_dir: PathBuf,
    pub group: u16,
    pub node_id: u16,
    /// The `TimeTick` cadence (E-17.3). Expiry precision is this interval (D-5).
    pub tick: Duration,
    /// `fsync` the journal after every input, not only flush it to the operating system.
    pub fsync: bool,
    /// The capacity profile to size the engine's tables by: `small`, `launch` or `scale`.
    pub capacity: String,
    /// Where to serve metrics (deploy/observability/metrics.md), if anywhere.
    pub metrics: Option<String>,
}

/// The file beside a journal naming the capacity profile it was recorded under.
pub const CAPACITY: &str = "capacity";

/// The capacity profile a journal was recorded under: the one the file beside it names, or `small` for a journal
/// with no such file, since every journal from before profiles had names was recorded under `small`.
///
/// Replaying a journal under a different profile can fail-stop where the original did not, or not where it did,
/// so replay, benchmarks and a restarted node all take the profile from here. This works while a node's journal
/// lives in its own data directory; once replicas receive journals rather than directories, the profile has to
/// travel in the journal itself.
pub fn journal_profile(journal: &Path) -> Result<(String, Capacities), Error> {
    let path = journal.with_file_name(CAPACITY);
    let name = match fs::read_to_string(&path) {
        Ok(text) => text.trim().to_string(),
        Err(e) if e.kind() == ErrorKind::NotFound => "small".to_string(),
        Err(e) => return Err(e.into()),
    };
    let capacities = Capacities::named(&name)
        .ok_or_else(|| format!("{} names an unknown capacity profile {name:?}", path.display()))?;
    Ok((name, capacities))
}

/// SHA-256 of the running binary, for `EngineStarted` and `ActivateProtocol` (E-7.7).
pub fn binary_hash() -> Result<[u8; 32], Error> {
    let bytes = fs::read(std::env::current_exe()?)?;
    Ok(Sha256::digest(&bytes).into())
}

pub(crate) fn engine_config(group: u16) -> Result<EngineConfig, Error> {
    Ok(EngineConfig { sequencer_group: group, binary_hash: binary_hash()?, table_seed: TABLE_SEED })
}

/// The sequencer's clock (E-17.1). The workspace bans clocks because the engine's only clock is the journal; the
/// sequencer is the one component whose job is to turn a clock into journal time.
#[allow(clippy::disallowed_methods)]
fn clock_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

/// Schedules `TimeTick` injection. Never written to the journal.
#[allow(clippy::disallowed_methods)]
fn monotonic() -> Instant {
    Instant::now()
}

/// Writes results to the result log, and copies them into a buffer for the sender and the feed.
struct Sink<'a> {
    log: &'a mut BufWriter<File>,
    copy: Option<&'a mut Vec<u8>>,
    failed: Option<io::Error>,
}

impl ResultSink for Sink<'_> {
    fn emit(&mut self, encoded: &[u8]) {
        if self.failed.is_none() {
            if let Err(e) = write_frame(&mut *self.log, encoded) {
                self.failed = Some(e);
            }
        }
        if let Some(copy) = self.copy.as_deref_mut() {
            let _ = write_frame(copy, encoded);
        }
    }
}

pub(crate) fn apply(
    engine: &mut Engine<'_>,
    input: &[u8],
    results: &mut BufWriter<File>,
    copy: Option<&mut Vec<u8>>,
) -> Result<(), Error> {
    let mut sink = Sink { log: results, copy, failed: None };
    let outcome = engine.apply(input, &mut sink);
    if let Some(e) = sink.failed {
        return Err(e.into());
    }
    outcome.map_err(|stop| format!("engine fail-stop {stop:?} at input {} (spec §18)", engine.position().0).into())
}

/// Runs the frames of a journal through `engine`, writing results, in `(after, upto]` of sequence — `after: 0`
/// for no lower bound, `upto: None` for no upper one. A `Snapshot` frame is not an input and is skipped rather
/// than applied. Reads to the physical end of the file regardless of `upto`, so a torn final frame, left by a
/// crash mid-write, is reported and, when `repair` is set, cut off — meaningful only for a caller reading to the
/// true end (`upto: None`); a bounded read simply stops at its bound first. Returns the number of inputs applied.
fn run_journal(
    engine: &mut Engine<'_>,
    path: &Path,
    results: &mut BufWriter<File>,
    after: u64,
    upto: Option<u64>,
    repair: bool,
) -> Result<u64, Error> {
    let file = OpenOptions::new().read(true).write(repair).create(repair).truncate(false).open(path)?;
    let length = file.metadata()?.len();
    let mut reader = BufReader::new(&file);
    let (mut buf, mut good, mut inputs) = (Vec::new(), 0u64, 0u64);
    loop {
        match read_frame(&mut reader, &mut buf) {
            Ok(Some(n)) => {
                if is_snapshot(&buf[..n]) {
                    good += 4 + n as u64;
                    continue;
                }
                let sequence = u64_at(&buf[..n], 8);
                if sequence <= after {
                    good += 4 + n as u64;
                    continue;
                }
                if upto.is_some_and(|stop| sequence > stop) {
                    break;
                }
                apply(engine, &buf[..n], results, None)?;
                inputs += 1;
                good += 4 + n as u64;
            }
            Ok(None) => break,
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => {
                let journal = path.display();
                log::warn(
                    "torn final frame in the journal",
                    &[("journal", &journal), ("after", &good), ("of", &length)],
                );
                break;
            }
            Err(e) => return Err(e.into()),
        }
    }
    drop(reader);
    if repair && upto.is_none() && good < length {
        file.set_len(good)?;
        log::warn("journal cut to its last whole frame", &[("journal", &path.display()), ("bytes", &good)]);
    }
    Ok(inputs)
}

/// Runs a journal through a fresh engine and writes its result log. Replaying one journal always yields the
/// same result log, byte for byte, on any build with the same profile (blueprint §16).
///
/// Refuses a journal that does not start at sequence 1 — either `cut` has already shortened it, or `seed` founded
/// it on a snapshot with no genesis of its own: a fresh engine expects sequence 1 first (E-4.1), and either kind
/// of journal starts with a `Snapshot` frame instead — `recover`, not `replay`, is what such a journal is read
/// with.
pub fn replay(journal: &Path, results: &Path) -> Result<u64, Error> {
    if !starts_at_genesis(journal)? {
        return Err(format!(
            "{} does not start at sequence 1 (cut, or founded on a snapshot); use recover, not replay",
            journal.display()
        )
        .into());
    }
    let group = {
        let mut reader = BufReader::new(File::open(journal)?);
        let mut buf = Vec::new();
        match read_frame(&mut reader, &mut buf)? {
            Some(n) if n >= 26 => u16::from_le_bytes([buf[24], buf[25]]),
            Some(_) => return Err("the journal's first frame is too short for a record header".into()),
            None => 1,
        }
    };
    let (_, capacities) = journal_profile(journal)?;
    let mut owned = OwnedMemory::new(&capacities);
    let mut engine =
        Engine::new(engine_config(group)?, owned.memory()).map_err(|e| format!("engine construction: {e:?}"))?;
    let mut log = BufWriter::new(File::create(results)?);
    let inputs = run_journal(&mut engine, journal, &mut log, 0, None, false)?;
    log.flush()?;
    Ok(inputs)
}

/// The sequence a result log's results are trusted through, and how many bytes of it to keep — everything up to,
/// but not including, the last group of results found, whatever it names. `None` when nothing is trusted at all:
/// the file does not exist, is empty, or holds only the one group nothing follows.
///
/// A result carries `resultHeader.inputSequence` and `resultIndex` (`interfaces/sbe/venue-journal.xml`), never a
/// count of how many results its input produced. So a crash between two results of the same input leaves the
/// file ending on a whole, well-formed frame — `read_frame` sees nothing wrong with it — but short of what a
/// fresh replay of that input would produce, and nothing in the bytes says so. The only group this can ever rule
/// out is the last one: once a *different* sequence follows a group, that group is confirmed complete, because
/// nothing may be journaled and applied out of order (E-4.1). The last group is never trusted, complete or not,
/// the same way `run_journal`'s repair never trusts a journal's last frame without a whole one closing it.
pub fn trusted_results(path: &Path) -> Result<Option<(u64, u64)>, Error> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut reader = BufReader::new(file);
    let mut buf = Vec::new();
    let (mut offset, mut group, mut confirmed) = (0u64, None, None);
    loop {
        let start = offset;
        match read_frame(&mut reader, &mut buf) {
            Ok(Some(n)) => {
                let sequence = u64_at(&buf[..n], 8);
                if group.is_some_and(|g| g != sequence) {
                    confirmed = Some((group.expect("checked Some above"), start));
                }
                group = Some(sequence);
                offset = start + 4 + n as u64;
            }
            Ok(None) => break,
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(confirmed)
}

/// Truncates a result log to what `trusted_results` says is trusted, and returns the sequence it is trusted
/// through — `None` when nothing was, in which case the file is left exactly as it was (a caller that wants an
/// empty result log asks for that itself; this only ever removes an unconfirmed tail).
pub fn repair_results(path: &Path) -> Result<Option<u64>, Error> {
    let Some((sequence, length)) = trusted_results(path)? else { return Ok(None) };
    let file = OpenOptions::new().write(true).open(path)?;
    if file.metadata()?.len() != length {
        file.set_len(length)?;
        log::warn("result log cut to its last confirmed group", &[("results", &path.display()), ("bytes", &length)]);
    }
    Ok(Some(sequence))
}

/// Rewrites `path` to keep only the bytes from `start` onward, durably and atomically: copied to a sibling temp
/// file, `fsync`ed, then renamed over the original. A crash mid-rewrite leaves the original exactly as it was —
/// nothing is replaced until its replacement is already durable, the same principle the audit shipper's "nothing
/// deleted until it is verified in place" rests on (interfaces/audit/README.md). Returns how many bytes were
/// removed; `start: 0` is a no-op, since there is nothing to cut. Correct for a result log, whose frames are
/// always written in increasing sequence order — a single cut point is all it ever needs. Not correct for a
/// journal (see `rewrite_journal_cut`): `embed` appends wherever the file currently ends, not necessarily right
/// after the sequence it describes, so a single "keep from this byte onward" point can silently drop later,
/// higher-sequence inputs that physically sit *before* it.
fn rewrite_from(path: &Path, start: u64) -> Result<u64, Error> {
    if start == 0 {
        return Ok(0);
    }
    let temp = path.with_extension("cut-tmp");
    {
        let mut input = File::open(path)?;
        input.seek(SeekFrom::Start(start))?;
        let mut output = File::create(&temp)?;
        io::copy(&mut input, &mut output)?;
        output.sync_all()?;
    }
    fs::rename(&temp, path)?;
    Ok(start)
}

/// The byte offset in a result log where the first result for a sequence after `at` begins. Everything before it
/// — every result for a sequence at or before `at` — is safe to drop once a `Snapshot` for `at` exists and this
/// result log is already trusted at least that far, which `cut` checks before ever calling this.
fn results_cut_offset(results: &Path, at: u64) -> Result<u64, Error> {
    let mut reader = BufReader::new(File::open(results)?);
    let mut buf = Vec::new();
    let mut offset = 0u64;
    while let Some(n) = read_frame(&mut reader, &mut buf)? {
        if u64_at(&buf[..n], 8) > at {
            return Ok(offset);
        }
        offset += 4 + n as u64;
    }
    Ok(offset)
}

/// Rewrites `journal`, durably and atomically (same discipline as `rewrite_from`), to keep only the `Snapshot`
/// frame at `at` and whatever has a sequence greater than `at` — in their original relative order, wherever in
/// the file each one actually sits. A single byte offset is not enough to cut a journal correctly: `embed`
/// appends wherever the file currently ends, so a snapshot for an earlier sequence, embedded after later inputs
/// were already journaled, physically sits *after* them — filtering by sequence, frame by frame, is what
/// `replay_tail` already does for the same reason (§3.7), and cutting needs the same discipline, not a shortcut.
/// Any *other*, now-superseded `Snapshot` frame is dropped too. Returns how many bytes were removed.
fn rewrite_journal_cut(journal: &Path, at: u64) -> Result<u64, Error> {
    let before = journal.metadata()?.len();
    let temp = journal.with_extension("cut-tmp");
    let mut kept = 0u64;
    {
        let mut reader = BufReader::new(File::open(journal)?);
        let mut output = BufWriter::new(File::create(&temp)?);
        let mut buf = Vec::new();
        while let Some(n) = read_frame(&mut reader, &mut buf)? {
            let sequence = u64_at(&buf[..n], 8);
            let keep = if is_snapshot(&buf[..n]) { sequence == at } else { sequence > at };
            if keep {
                write_frame(&mut output, &buf[..n])?;
                kept += 4 + n as u64;
            }
        }
        output.flush()?;
        output.get_ref().sync_all()?;
    }
    fs::rename(&temp, journal)?;
    Ok(before.saturating_sub(kept))
}

/// What a cut would remove, written out whole rather than discarded. Everything a `Snapshot` at `at` makes
/// droppable, in a form that can be archived, verified in place, and only then cut away.
#[derive(Debug)]
pub struct Segment {
    pub at: u64,
    pub journal: PathBuf,
    pub results: PathBuf,
    pub journal_sha256: String,
    pub results_sha256: String,
    /// The span of real inputs the journal half carries. Equal when it carries exactly one; both zero when it
    /// carries none, which is a journal already cut to this snapshot and nothing to archive again.
    pub first_sequence: u64,
    pub last_sequence: u64,
}

/// Writes out exactly what `cut` would remove, and **changes nothing**: neither file is written back, so this is
/// safe against a node that is still serving, and the segment can be shipped and verified long before anything
/// is discarded. The complement of `rewrite_journal_cut`'s filter, frame for frame, and of `results_cut_offset`'s
/// prefix, byte for byte — so what this saves and what a later cut drops are the same bytes by construction
/// rather than by two rules that have to be kept in step.
///
/// Refuses on the same preconditions as `cut` (a snapshot to cut to, a result log trusted at least that far), so
/// a segment only ever exists for a cut that would actually be allowed. It checks them with `trusted_results`
/// rather than `repair_results`, because repairing truncates and this must not write.
///
/// Refuses a directory that already holds a segment: overwriting one is how an unshipped archive gets lost.
pub fn export_cut(journal: &Path, results: &Path, out_dir: &Path) -> Result<Segment, Error> {
    let Some((at, _)) = snapshots::latest(journal)? else {
        return Err(
            format!("{} has no embedded snapshot to cut to (venue-node embed-snapshot)", journal.display()).into()
        );
    };
    match trusted_results(results)? {
        Some((trusted, _)) if trusted >= at => {}
        Some((trusted, _)) => {
            return Err(format!(
                "the result log is only trusted through {trusted}, before the snapshot at {at}: run the node so \
                 it catches up, or embed a snapshot at or before {trusted}"
            )
            .into())
        }
        None => return Err(format!("the result log has nothing trusted yet, so cutting to {at} would lose it").into()),
    }

    let (journal_out, results_out) =
        (out_dir.join(format!("journal-{at}.sbe")), out_dir.join(format!("results-{at}.sbe")));
    let manifest_out = out_dir.join(SEGMENT_MANIFEST);
    for existing in [&journal_out, &results_out, &manifest_out] {
        if existing.exists() {
            return Err(
                format!("{} already exists: a segment is there, ship or move it first", existing.display()).into()
            );
        }
    }
    fs::create_dir_all(out_dir)?;

    // The journal half: every frame the cut would not keep. A superseded `Snapshot` goes too — it describes a
    // point inside the stretch being archived, so it belongs with it.
    let (mut first, mut last) = (None, 0u64);
    let mut hasher = Sha256::new();
    {
        let mut reader = BufReader::new(File::open(journal)?);
        let mut output = BufWriter::new(File::create(&journal_out)?);
        let mut buf = Vec::new();
        while let Some(n) = read_frame(&mut reader, &mut buf)? {
            let sequence = u64_at(&buf[..n], 8);
            let snapshot = is_snapshot(&buf[..n]);
            if if snapshot { sequence == at } else { sequence > at } {
                continue; // kept by the cut, so not ours to archive
            }
            if !snapshot {
                first.get_or_insert(sequence);
                last = last.max(sequence);
            }
            hasher.update((n as u32).to_le_bytes());
            hasher.update(&buf[..n]);
            write_frame(&mut output, &buf[..n])?;
        }
        output.flush()?;
        output.get_ref().sync_all()?;
    }
    let journal_sha256 = hex(hasher.finalize().as_slice());

    // The result half is a byte prefix, so it is copied as one rather than re-framed.
    let results_sha256 = copy_prefix(results, results_cut_offset(results, at)?, &results_out)?;

    let segment = Segment {
        at,
        journal: journal_out,
        results: results_out,
        journal_sha256,
        results_sha256,
        first_sequence: first.unwrap_or(0),
        last_sequence: if first.is_some() { last } else { 0 },
    };
    write_segment_manifest(&manifest_out, &segment)?;
    log::info(
        "cut segment exported",
        &[
            ("at", &segment.at),
            ("first_sequence", &segment.first_sequence),
            ("last_sequence", &segment.last_sequence),
            ("out", &out_dir.display()),
        ],
    );
    Ok(segment)
}

/// The manifest a segment carries, and what `--require-shipped` later reads back.
pub const SEGMENT_MANIFEST: &str = "segment.json";

/// The receipt `journal_ship.py` writes beside a segment once object storage has it and has been read back.
pub const SEGMENT_RECEIPT: &str = "shipped.json";

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Copies `length` bytes from the front of `from` into `to`, returning their digest.
fn copy_prefix(from: &Path, length: u64, to: &Path) -> Result<String, Error> {
    let mut reader = BufReader::new(File::open(from)?.take(length));
    let mut output = BufWriter::new(File::create(to)?);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        output.write_all(&buf[..n])?;
    }
    output.flush()?;
    output.get_ref().sync_all()?;
    Ok(hex(hasher.finalize().as_slice()))
}

fn write_segment_manifest(path: &Path, segment: &Segment) -> Result<(), Error> {
    let manifest = serde_json::json!({
        "at": segment.at,
        "first_sequence": segment.first_sequence,
        "last_sequence": segment.last_sequence,
        "journal": segment.journal.file_name().and_then(|n| n.to_str()).unwrap_or_default(),
        "results": segment.results.file_name().and_then(|n| n.to_str()).unwrap_or_default(),
        "journal_sha256": segment.journal_sha256,
        "results_sha256": segment.results_sha256,
    });
    let mut file = File::create(path)?;
    file.write_all(serde_json::to_string_pretty(&manifest)?.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

/// Reads a shipped receipt and says whether it really covers the segment in `dir` — the check that stands
/// between a cut and deleting history nothing has stored. The receipt names the digests object storage gave
/// back; the manifest names the digests of the files on disk; both must agree with each other and with the
/// snapshot the cut is about to cut to. A receipt for a *different* snapshot is the dangerous case, and the one
/// this exists to catch: it would otherwise look like a perfectly good archive.
fn shipped_covers(dir: &Path, at: u64) -> Result<(), Error> {
    let read = |name: &str| -> Result<serde_json::Value, Error> {
        let path = dir.join(name);
        let text = fs::read_to_string(&path)
            .map_err(|e| format!("{}: {e} (run venue-node export-cut, then journal_ship.py)", path.display()))?;
        Ok(serde_json::from_str(&text)?)
    };
    let (manifest, receipt) = (read(SEGMENT_MANIFEST)?, read(SEGMENT_RECEIPT)?);

    let manifest_at = manifest["at"].as_u64().ok_or("the segment manifest has no 'at'")?;
    if manifest_at != at {
        return Err(format!(
            "the segment in {} is for the snapshot at {manifest_at}, but this cut would cut to {at}: that \
             segment does not contain what is about to be removed",
            dir.display()
        )
        .into());
    }
    if receipt["at"].as_u64() != Some(at) {
        return Err(format!("the receipt in {} is not for the snapshot at {at}", dir.display()).into());
    }
    if receipt["verified"] != serde_json::Value::Bool(true) {
        return Err(format!("{} does not say the upload was verified", dir.join(SEGMENT_RECEIPT).display()).into());
    }
    for half in ["journal", "results"] {
        let key = format!("{half}_sha256");
        let (theirs, ours) = (&receipt[&key], &manifest[&key]);
        if theirs != ours || theirs.is_null() {
            return Err(format!(
                "the {half} half was shipped as {theirs} but the segment on disk is {ours}: the receipt is not \
                 for these bytes"
            )
            .into());
        }
    }
    Ok(())
}

/// Cuts a journal, and its result log with it, to start at the latest embedded snapshot — removing everything
/// D-32 says a snapshot is what makes removable. Refuses unless the result log is already trusted through at
/// least the snapshot's own sequence (`repair_results`, which this calls first and whose own repair happens
/// regardless of the outcome): cutting past what it actually covers would permanently lose the ability to
/// reconstruct those results, since the journal bytes a full replay would need are exactly what this removes.
///
/// Rewrites both files whole rather than truncating in place — `journal.sbe` has no way to drop bytes from its
/// *front* the way `repair_results` drops an unconfirmed *tail*. For a journal no live `serve` has open, the same
/// caveat `embed` carries: there is no lock on it today.
///
/// Returns the sequence cut to, and how many bytes were removed from the journal and from the result log.
pub fn cut(journal: &Path, results: &Path) -> Result<(u64, u64, u64), Error> {
    cut_checked(journal, results, None)
}

/// The same, but `shipped` names a segment directory that must already have been archived and verified — see
/// `shipped_covers`. This is the difference between cutting as *archiving* and cutting as deletion: without it
/// the bytes are simply gone, and nothing in the tool would have said so.
pub fn cut_checked(journal: &Path, results: &Path, shipped: Option<&Path>) -> Result<(u64, u64, u64), Error> {
    let Some((at, _)) = snapshots::latest(journal)? else {
        return Err(
            format!("{} has no embedded snapshot to cut to (venue-node embed-snapshot)", journal.display()).into()
        );
    };
    match repair_results(results)? {
        Some(trusted) if trusted >= at => {}
        Some(trusted) => {
            return Err(format!(
                "the result log is only trusted through {trusted}, before the snapshot at {at}: run the node so \
                 it catches up, or embed a snapshot at or before {trusted}"
            )
            .into())
        }
        None => return Err(format!("the result log has nothing trusted yet, so cutting to {at} would lose it").into()),
    }
    if let Some(dir) = shipped {
        shipped_covers(dir, at)?;
    }
    let journal_removed = rewrite_journal_cut(journal, at)?;
    let results_removed = rewrite_from(results, results_cut_offset(results, at)?)?;
    log::info(
        "journal cut",
        &[
            ("journal", &journal.display()),
            ("at", &at),
            ("journal_bytes_removed", &journal_removed),
            ("results_bytes_removed", &results_removed),
        ],
    );
    Ok((at, journal_removed, results_removed))
}

/// Founds a fresh data directory on `snapshot` instead of genesis: `data_dir/journal.sbe` holds only that one
/// `Snapshot` frame, `data_dir/results.sbe` starts empty, and `data_dir/capacity` records `capacity` — so a
/// `serve` pointed at `data_dir` recovers through `choose_recovery`'s `founded_only` case and starts running
/// from exactly this state, with no history of its own before it (§3.9: a test or preview environment choosing
/// which snapshot to start from, rather than genesis or whatever the live venue happens to be running).
///
/// Refuses a `data_dir` that already has a journal: seeding seeds a fresh environment, it does not replace one
/// that already has real history of its own.
pub fn seed(snapshot: &Path, data_dir: &Path, capacity: &str) -> Result<(), Error> {
    let journal = data_dir.join(JOURNAL);
    if journal.exists() {
        return Err(
            format!("{} already has a journal; seed is only for a fresh data directory", journal.display()).into()
        );
    }
    let mut reader = BufReader::new(File::open(snapshot)?);
    let mut buf = Vec::new();
    let n = read_frame(&mut reader, &mut buf)?.ok_or("the snapshot file is empty")?;
    if template_of(&buf[..n]) != Some(SNAPSHOT) {
        return Err("that file does not begin with a Snapshot frame".into());
    }
    let at = u64_at(&buf[..n], 8);
    fs::create_dir_all(data_dir)?;
    let mut out = BufWriter::new(File::create(&journal)?);
    write_frame(&mut out, &buf[..n])?;
    out.flush()?;
    File::create(data_dir.join(RESULTS))?;
    fs::write(data_dir.join(CAPACITY), capacity)?;
    log::info(
        "data directory seeded from a snapshot",
        &[("data_dir", &data_dir.display()), ("snapshot", &snapshot.display()), ("at", &at), ("capacity", &capacity)],
    );
    Ok(())
}

/// Stamps, journals and applies inputs, in that order.
struct Pipeline {
    sequencer: Sequencer,
    journal: BufWriter<File>,
    results: BufWriter<File>,
    fsync: bool,
    /// The framed results of the input committed last.
    out: Vec<u8>,
    metrics: Arc<NodeMetrics>,
}

impl Pipeline {
    fn commit(&mut self, engine: &mut Engine<'_>, input: &mut [u8]) -> Result<(), Error> {
        let (sequence, at) = self.sequencer.stamp(input, clock_ns());
        let started = monotonic();
        write_frame(&mut self.journal, input)?;
        self.journal.flush()?;
        let appended = monotonic();
        let synced = if self.fsync {
            self.journal.get_ref().sync_data()?;
            Some(monotonic().duration_since(appended))
        } else {
            None
        };
        self.metrics.journaled(input, sequence, at, appended.duration_since(started), synced);
        self.out.clear();
        apply(engine, input, &mut self.results, Some(&mut self.out))?;
        // Durable before the next input is committed, the same way the journal already is: a crash between two
        // inputs must never leave one with results only partly on disk, because nothing marks a result group done
        // (interfaces/sbe/venue-journal.xml, ResultHeader — resultIndex counts from 0, it does not say how many
        // there will be), so a torn group cannot be told from a complete one by its bytes alone. `trusted_results`
        // is what a restart checks this against; this is what makes its answer mean something.
        self.results.flush()?;
        if self.fsync {
            self.results.get_ref().sync_data()?;
        }
        Ok(())
    }
}

enum Ingress {
    Connected(u64, TcpStream),
    Input(u64, Vec<u8>),
    Closed(u64),
    Subscribe(TcpStream),
}

fn accept(listener: TcpListener, tx: Sender<Ingress>) {
    for (id, stream) in (1u64..).zip(listener.incoming()) {
        let Ok(stream) = stream else { continue };
        let Ok(writer) = stream.try_clone() else { continue };
        let _ = stream.set_nodelay(true);
        let _ = writer.set_write_timeout(Some(Duration::from_secs(2)));
        if tx.send(Ingress::Connected(id, writer)).is_err() {
            return;
        }
        let tx = tx.clone();
        thread::spawn(move || {
            let mut reader = BufReader::new(stream);
            let mut buf = Vec::new();
            while let Ok(Some(n)) = read_frame(&mut reader, &mut buf) {
                if tx.send(Ingress::Input(id, buf[..n].to_vec())).is_err() {
                    return;
                }
            }
            let _ = tx.send(Ingress::Closed(id));
        });
    }
}

fn accept_feed(listener: TcpListener, tx: Sender<Ingress>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let _ = stream.set_nodelay(true);
        if tx.send(Ingress::Subscribe(stream)).is_err() {
            return;
        }
    }
}

/// Only client messages and commands may arrive from outside; sequencer events come from the sequencer (E-17.3).
fn admissible(input: &[u8]) -> Result<(), String> {
    match frame::check(input) {
        Ok(f) if f.kind == Kind::Sequencer => Err(format!("template {} is a sequencer event", f.template_id)),
        Ok(_) => Ok(()),
        Err(e) => Err(format!("not a journal input: {e:?}")),
    }
}

// ---------------------------------------------------------------------------------------------------------- feed

type Unit = Arc<Vec<u8>>;

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    bytes.get(at..at + 8).and_then(|b| b.try_into().ok()).map_or(0, u64::from_le_bytes)
}

/// A `TimeTick` that changed nothing is not worth a reader's time.
fn quiet(input: &[u8], results: &[u8]) -> bool {
    results.is_empty() && input.get(2..4).is_some_and(|t| u16::from_le_bytes([t[0], t[1]]) == TIME_TICK)
}

/// One feed unit: the input frame, its result frames, then an empty frame.
fn unit(input: &[u8], results: &[u8]) -> io::Result<Vec<u8>> {
    let mut u = Vec::with_capacity(input.len() + results.len() + 12);
    write_frame(&mut u, input)?;
    u.extend_from_slice(results);
    write_frame(&mut u, &[])?;
    Ok(u)
}

/// A live subscriber: where its units go, and how far its thread has got with them.
type Live = (Sender<Unit>, Arc<Subscriber>);

fn publish(subscribers: &mut Vec<Live>, metrics: &NodeMetrics, input: &[u8], results: &[u8]) -> io::Result<()> {
    if quiet(input, results) {
        return Ok(());
    }
    metrics.feed_published_sequence.set(i64::try_from(u64_at(input, 8)).unwrap_or(i64::MAX));
    if subscribers.is_empty() {
        return Ok(());
    }
    let u = Arc::new(unit(input, results)?);
    subscribers.retain(|(s, progress)| {
        let sent = s.send(Arc::clone(&u)).is_ok();
        if sent {
            progress.sent.fetch_add(1, Ordering::Relaxed);
        }
        sent
    });
    metrics.feed_units_published.inc();
    Ok(())
}

/// What a journal's first `length` bytes can answer for: the highest sequence below which it carries nothing,
/// and the highest it carries. Read by walking the frames, which is what the subscriber's thread is about to do
/// anyway; journals are opened and read here, never held open.
///
/// Only real inputs decide either end. A `Snapshot` frame's own sequence names the input it follows rather than
/// one of its own, and `embed` appends wherever the file currently ends — so the frame that happens to sit last
/// can name an *earlier* sequence than inputs before it (§3.7). Taking the last frame's sequence, whatever it
/// was, made a journal with a snapshot embedded at its tail report an upper bound below its own real inputs, and
/// refuse resumes that were perfectly valid.
///
/// A journal `seed` founded, or `cut` down to nothing but its snapshot, has no real input to speak for it. Then
/// the snapshot's sequence is both ends at once: nothing below it is carried, and nothing above it has happened.
fn range(path: &Path, length: u64) -> Result<(u64, u64), Error> {
    let mut reader = BufReader::new(File::open(path)?.take(length));
    let mut buf = Vec::new();
    let (mut first, mut last, mut snapshot_at) = (None, 0u64, 0u64);
    while let Some(n) = read_frame(&mut reader, &mut buf)? {
        if n < 16 {
            continue;
        }
        let sequence = u64_at(&buf[..n], 8);
        if is_snapshot(&buf[..n]) {
            snapshot_at = snapshot_at.max(sequence);
            continue;
        }
        first.get_or_insert(sequence);
        last = last.max(sequence);
    }
    match first {
        Some(first) => Ok((first.saturating_sub(1), last)),
        None => Ok((snapshot_at, snapshot_at)),
    }
}

/// Sends a subscriber the journal and result log up to the given lengths, pairing each input with its results by
/// sequence number, then every live unit. The lengths were taken, and the live channel registered, at one point
/// in the main loop, so nothing is missed or repeated.
///
/// A subscriber opens by saying where it has got to: eight little-endian bytes, the last sequence it holds, or
/// zero for everything. A reader that reconnects after a moment's network trouble then costs what it missed
/// rather than the whole journal, which is the difference between a reconnect that is free and one that grows
/// without bound as the venue runs.
///
/// The backlog is still *read* in full — the result log is walked in step with the journal, so the pairing has to
/// see every frame — but only what follows the subscriber's position is sent.
fn serve_subscriber(
    mut stream: TcpStream,
    journal: (&Path, u64),
    results: (&Path, u64),
    live: Receiver<Unit>,
    progress: &Subscriber,
) -> Result<(), Error> {
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    // A subscriber that says nothing is one that has gone; the timeout is what stops its thread waiting for ever.
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut position = [0u8; 8];
    stream.read_exact(&mut position)?;
    let asked_for = u64::from_le_bytes(position);
    stream.set_read_timeout(None)?;

    // Whether the node can honour it, at both ends. Too high is a reader whose node was replaced under it — a
    // fresh data directory, or another group's — and sending it the tail would leave it with a view built on a
    // history that never happened. Too low is a reader asking to carry on from a point this journal no longer
    // reaches back to, because `cut` removed it or `seed` founded above it: honouring that would quietly skip
    // everything between what it holds and where the journal now starts, while the ack told it the resume had
    // worked. Either way it is sent what there is and told the position was refused, because a reader that kept
    // a view has to be able to throw it away.
    //
    // `>= floor`, not `>`: a reader holding *through* the floor is exactly right. A journal founded or cut at 62
    // carries 63 onward, so a reader that says "I have 62" is asking for precisely what is there — which is what
    // every reader founded on a snapshot pair says (`venue-view snapshot`). Refusing that would refuse the whole
    // founding path, which is how `founded_venue.rs` caught this being `>` first.
    let (floor, last) = range(journal.0, journal.1)?;
    let resume_from = if asked_for >= floor && asked_for <= last { asked_for } else { 0 };
    let peer = || stream.peer_addr().map_or_else(|_| "unknown".to_string(), |a| a.to_string());
    if asked_for > 0 && asked_for < floor {
        log::warn(
            "a subscriber asked to resume from below what this journal still carries",
            &[("peer", &peer()), ("asked_for", &asked_for), ("carried_from", &floor)],
        );
    } else if asked_for == 0 && floor > 0 {
        // Not refusable with the one byte this handshake has, and not silently wrong either: what follows is a
        // real backlog, just not the whole history the reader is asking for. A reader founded on a snapshot pair
        // (`venue-view snapshot`) asks for its own sequence instead and never lands here.
        log::warn(
            "a subscriber asked for everything, and this journal no longer starts at the beginning",
            &[("peer", &peer()), ("carried_from", &floor)],
        );
    }
    stream.write_all(&[u8::from(resume_from > 0)])?;
    let mut out = BufWriter::new(stream);
    let mut inputs = BufReader::new(File::open(journal.0)?.take(journal.1));
    let mut outputs = BufReader::new(File::open(results.0)?.take(results.1));
    let (mut input, mut result, mut batch) = (Vec::new(), Vec::new(), Vec::new());
    let mut pending = read_frame(&mut outputs, &mut result)?;
    while let Some(n) = read_frame(&mut inputs, &mut input)? {
        // Not an input, and paired with nothing: a subscriber with no use for it never sees it.
        if is_snapshot(&input[..n]) {
            continue;
        }
        let sequence = u64_at(&input, 8);
        batch.clear();
        while let Some(m) = pending {
            if u64_at(&result[..m], 8) != sequence {
                break;
            }
            write_frame(&mut batch, &result[..m])?;
            pending = read_frame(&mut outputs, &mut result)?;
        }
        // Read in full, sent from where the subscriber left off.
        if sequence > resume_from && !quiet(&input[..n], &batch) {
            out.write_all(&unit(&input[..n], &batch)?)?;
        }
    }
    out.flush()?;
    for u in live {
        out.write_all(&u)?;
        out.flush()?;
        progress.written.fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}

/// How `serve` recovers, decided once at start and never revisited during the run.
#[derive(Debug)]
enum Recovery {
    /// No usable snapshot: a fresh engine replays the whole journal, and `results.sbe` is rebuilt from nothing,
    /// exactly as before PR #14/#15.
    Genesis,
    /// The latest embedded snapshot (`at`, its frame) plus how far the preserved result log is trusted
    /// (`trusted_through`) — never earlier than `at` (below).
    FromSnapshot { at: u64, frame: Vec<u8>, trusted_through: u64 },
}

/// Decides which of the two `serve` can take, from what is already on disk. Reads `results` through
/// `repair_results`, which — as a side effect, regardless of which `Recovery` this returns — cuts it back to its
/// last confirmed-complete group; this is always correct to do, because that group is never trusted anyway.
///
/// A snapshot only helps when the result log already trustworthily covers **at least its own sequence**. One
/// found but not covered is not usable: using it without also re-deriving the results for the sequences it
/// covers would leave a gap in `results.sbe` that a subscriber's backlog cannot fill for those sequences —
/// re-deriving them by replaying from the snapshot would defeat what the snapshot was for in the first place. So:
/// a result log not yet caught up to a snapshot that exists falls back to `Genesis` — *if* genesis is still
/// possible. Once `cut` has ever run, it is not: the bytes a genesis replay would need are exactly what it
/// removed, so a journal that no longer `starts_at_genesis` and has no *usable* snapshot is not a fallback case at
/// all, but a real error — recovery is not possible until the result log catches up to the embedded snapshot.
///
/// The one exception is a journal `seed` has just founded: it carries no real input at all, before or after its
/// snapshot, so there is nothing for an uncovering result log to have lost — the check above would otherwise
/// refuse a freshly seeded environment before it ever got to run (`founded_only`). This is narrower than a
/// journal merely having nothing *after* its snapshot: an ordinary journal with one appended at the tail
/// (`embed` with no `--at`) still has real input *before* it that `results.sbe` owes results for, and must still
/// fall back to genesis, not take this shortcut.
fn choose_recovery(journal: &Path, results: &Path) -> Result<Recovery, Error> {
    // A brand-new node, never yet run: nothing to look for a snapshot in. `latest` (like every other reader of a
    // journal here) takes its existence for granted, since every other caller's journal is already there.
    if !journal.exists() {
        return Ok(Recovery::Genesis);
    }
    let snapshot = snapshots::latest(journal)?;
    let trusted = repair_results(results)?;
    if let Some((at, frame)) = &snapshot {
        if trusted.is_some_and(|through| through >= *at) {
            return Ok(Recovery::FromSnapshot { at: *at, frame: frame.clone(), trusted_through: trusted.unwrap() });
        }
        if founded_only(journal)? {
            return Ok(Recovery::FromSnapshot { at: *at, frame: frame.clone(), trusted_through: *at });
        }
    }
    if starts_at_genesis(journal)? {
        return Ok(Recovery::Genesis);
    }
    let at = snapshot.map(|(at, _)| at);
    Err(format!(
        "{} was cut to sequence {}, but its result log is only trusted through {} and genesis is no longer \
         possible: recovery is not possible until the result log catches up, or a fresh snapshot is embedded once \
         it does",
        journal.display(),
        at.map_or_else(|| "?".to_string(), |at| at.to_string()),
        trusted.map_or_else(|| "nothing".to_string(), |t| t.to_string()),
    )
    .into())
}

/// Brings `engine` up to date and returns the result log to keep appending to, following `plan`.
///
/// `FromSnapshot` restores the engine from the snapshot, then — only if the trusted result log runs ahead of the
/// snapshot's own sequence, which it will on every recovery after the first once the node has kept running past
/// the point a snapshot was taken — replays the already-trusted stretch into a throwaway sink to catch the
/// engine's *in-memory* state up to the same point without disturbing the durable results already sitting there
/// for it. Only what follows the trusted point is genuinely new: that is replayed for real, appended to the
/// preserved (not truncated) result log.
fn recover_engine(
    engine: &mut Engine<'_>,
    journal_path: &Path,
    results_path: &Path,
    plan: Recovery,
) -> Result<BufWriter<File>, Error> {
    match plan {
        Recovery::Genesis => {
            let mut results = BufWriter::new(File::create(results_path)?);
            let recovered = run_journal(engine, journal_path, &mut results, 0, None, true)?;
            log::info("recovered from the journal", &[("inputs", &recovered), ("journal", &journal_path.display())]);
            Ok(results)
        }
        Recovery::FromSnapshot { at, frame, trusted_through } => {
            snapshots::restore_into(engine, &frame)?;
            if trusted_through > at {
                let scratch = results_path.with_extension("recovery-scratch.tmp");
                let mut discard = BufWriter::new(File::create(&scratch)?);
                run_journal(engine, journal_path, &mut discard, at, Some(trusted_through), false)?;
                drop(discard);
                let _ = fs::remove_file(&scratch);
            }
            // `create(true)`: a journal `founded_only` trusts unconditionally (`choose_recovery`) can be seeded
            // with no `results.sbe` of its own yet — nothing has run to have written one.
            let mut results = BufWriter::new(OpenOptions::new().create(true).append(true).open(results_path)?);
            let recovered = run_journal(engine, journal_path, &mut results, trusted_through, None, true)?;
            log::info(
                "recovered from an embedded snapshot",
                &[
                    ("journal", &journal_path.display()),
                    ("snapshot_at", &at),
                    ("results_trusted_through", &trusted_through),
                    ("inputs", &recovered),
                ],
            );
            Ok(results)
        }
    }
}

/// Recovers from the journal in `data_dir`, writes `EngineStarted`, then serves the ingress and the feed and
/// injects `TimeTick` until a fail-stop or an I/O error.
///
/// On the ingress, a connection sends framed inputs with a zeroed record header. For each input it receives the
/// input's results as frames, then an empty frame. An input that is not a client message or command closes the
/// connection and is not journaled. On the feed, a connection receives units: an input, its results, an empty
/// frame.
pub fn serve(config: &Config) -> Result<(), Error> {
    fs::create_dir_all(&config.data_dir)?;
    let journal_path = config.data_dir.join(JOURNAL);
    let results_path = config.data_dir.join(RESULTS);
    let hash = binary_hash()?;
    let capacities = Capacities::named(&config.capacity)
        .ok_or_else(|| format!("unknown capacity profile {:?}: small, launch or scale", config.capacity))?;
    let recorded = config.data_dir.join(CAPACITY);
    let journal_started = fs::metadata(&journal_path).is_ok_and(|m| m.len() > 0);
    if recorded.exists() || journal_started {
        let (name, _) = journal_profile(&journal_path)?;
        if name != config.capacity {
            return Err(format!(
                "the journal in {} was recorded under the {name} capacity profile, not {}: replaying it under another \
                 could fail-stop where the original did not",
                config.data_dir.display(),
                config.capacity
            )
            .into());
        }
    }
    fs::write(&recorded, format!("{}\n", config.capacity))?;
    let mut registry = venue_telemetry::prometheus_client::registry::Registry::default();
    venue_telemetry::build_info(
        &mut registry,
        "venue_node",
        env!("CARGO_PKG_VERSION"),
        &[("capacity", &config.capacity)],
    );
    let metrics = NodeMetrics::register(&mut registry);
    if let Some(address) = &config.metrics {
        let bound = venue_telemetry::serve::spawn(address, registry, None)?;
        log::info("metrics served", &[("address", &bound)]);
    }
    let mut owned = OwnedMemory::new(&capacities);
    let mut engine =
        Engine::new(engine_config(config.group)?, owned.memory()).map_err(|e| format!("engine construction: {e:?}"))?;
    let plan = choose_recovery(&journal_path, &results_path)?;
    let results = recover_engine(&mut engine, &journal_path, &results_path, plan)?;
    let (last_sequence, last_at) = engine.position();
    log::info("recovered", &[("last_sequence", &last_sequence)]);
    let journal = BufWriter::new(OpenOptions::new().append(true).create(true).open(&journal_path)?);
    let mut pipeline = Pipeline {
        sequencer: Sequencer::resume(config.group, last_sequence, last_at),
        journal,
        results,
        fsync: config.fsync,
        out: Vec::new(),
        metrics: Arc::clone(&metrics),
    };
    pipeline.commit(&mut engine, &mut sequencer::engine_started(config.node_id, &hash))?;

    let listener = TcpListener::bind(config.listen)?;
    let feed = TcpListener::bind(config.feed)?;
    log::info(
        "serving",
        &[
            ("group", &config.group),
            ("node", &config.node_id),
            ("ingress", &listener.local_addr()?),
            ("feed", &feed.local_addr()?),
        ],
    );
    let (tx, rx) = mpsc::channel();
    let feed_tx = tx.clone();
    thread::spawn(move || accept(listener, tx));
    thread::spawn(move || accept_feed(feed, feed_tx));
    let mut clients: BTreeMap<u64, TcpStream> = BTreeMap::new();
    let mut subscribers: Vec<Live> = Vec::new();
    let mut reply = Vec::new();
    let mut next_tick = monotonic() + config.tick;
    loop {
        match rx.recv_timeout(next_tick.saturating_duration_since(monotonic())) {
            Ok(Ingress::Connected(id, stream)) => {
                clients.insert(id, stream);
                metrics.ingress_connections.set(i64::try_from(clients.len()).unwrap_or(i64::MAX));
            }
            Ok(Ingress::Closed(id)) => {
                clients.remove(&id);
                metrics.ingress_connections.set(i64::try_from(clients.len()).unwrap_or(i64::MAX));
            }
            Ok(Ingress::Subscribe(stream)) => {
                pipeline.journal.flush()?;
                pipeline.results.flush()?;
                let lengths = (fs::metadata(&journal_path)?.len(), fs::metadata(&results_path)?.len());
                let (unit_tx, unit_rx) = mpsc::channel();
                let progress = Arc::new(Subscriber::default());
                subscribers.push((unit_tx, Arc::clone(&progress)));
                let (j, r, metrics) = (journal_path.clone(), results_path.clone(), Arc::clone(&metrics));
                thread::spawn(move || {
                    let peer = stream.peer_addr().map_or_else(|_| "unknown".to_string(), |a| a.to_string());
                    if let Err(e) = serve_subscriber(stream, (&j, lengths.0), (&r, lengths.1), unit_rx, &progress) {
                        log::info("feed subscriber gone", &[("peer", &peer), ("reason", &e)]);
                    }
                    metrics.subscribers_disconnected.inc();
                });
            }
            Ok(Ingress::Input(id, mut input)) => {
                if let Err(why) = admissible(&input) {
                    log::warn("ingress input refused, connection closed", &[("connection", &id), ("reason", &why)]);
                    metrics.ingress_refused.inc();
                    if let Some(stream) = clients.remove(&id) {
                        let _ = stream.shutdown(Shutdown::Both);
                    }
                    continue;
                }
                pipeline.commit(&mut engine, &mut input)?;
                publish(&mut subscribers, &metrics, &input, &pipeline.out)?;
                // E-17.4: the reply leaves only after the input is journaled and its results exist.
                if let Some(stream) = clients.get_mut(&id) {
                    reply.clear();
                    reply.extend_from_slice(&pipeline.out);
                    write_frame(&mut reply, &[])?;
                    if stream.write_all(&reply).is_err() {
                        clients.remove(&id);
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err("the ingress listener stopped".into()),
        }
        let now = monotonic();
        if now >= next_tick {
            let mut tick = sequencer::time_tick();
            // `commit` already flushes (and syncs) `results` on its own now; nothing left to do here.
            pipeline.commit(&mut engine, &mut tick)?;
            publish(&mut subscribers, &metrics, &tick, &pipeline.out)?;
            metrics.subscribers(subscribers.iter().map(|(_, progress)| progress.as_ref()));
            next_tick = (next_tick + config.tick).max(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("venue-node-results-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    /// A result frame carrying nothing but a sequence, at the offset every result and every input shares.
    fn fake_result(sequence: u64) -> Vec<u8> {
        let mut frame = vec![0u8; 16];
        frame[8..16].copy_from_slice(&sequence.to_le_bytes());
        frame
    }

    fn write_results(path: &Path, groups: &[u64]) {
        let mut file = BufWriter::new(File::create(path).expect("created"));
        for &sequence in groups {
            write_frame(&mut file, &fake_result(sequence)).expect("written");
        }
        file.flush().expect("flushed");
    }

    #[test]
    fn an_empty_or_missing_result_log_trusts_nothing() {
        let dir = dir("empty");
        let path = dir.join("missing.sbe");
        assert_eq!(trusted_results(&path).expect("checked"), None);
        write_results(&path, &[]);
        assert_eq!(trusted_results(&path).expect("checked"), None);
    }

    #[test]
    fn a_single_group_with_nothing_after_it_is_never_trusted() {
        let dir = dir("single");
        let path = dir.join("results.sbe");
        // Three results, all for the one input: nothing confirms the group is done.
        write_results(&path, &[5, 5, 5]);
        assert_eq!(trusted_results(&path).expect("checked"), None);
    }

    #[test]
    fn a_group_is_trusted_once_a_different_sequence_follows_it() {
        let dir = dir("two-groups");
        let path = dir.join("results.sbe");
        write_results(&path, &[5, 5, 6, 6, 6]);
        let (sequence, length) = trusted_results(&path).expect("checked").expect("a confirmed group");
        assert_eq!(sequence, 5, "the confirmed group is the one a later sequence followed, not the later one");
        // Length is the byte offset of the first frame of the (unconfirmed) sequence-6 group: two 16-byte frames,
        // each carrying 4 bytes of length prefix.
        assert_eq!(length, 2 * (4 + 16));
    }

    #[test]
    fn only_the_last_group_is_ever_discarded() {
        let dir = dir("three-groups");
        let path = dir.join("results.sbe");
        write_results(&path, &[5, 6, 6, 7]);
        let (sequence, _) = trusted_results(&path).expect("checked").expect("a confirmed group");
        assert_eq!(sequence, 6, "a confirmed group other than the last one was not trusted");
    }

    #[test]
    fn a_torn_final_frame_is_folded_into_the_same_discarded_group() {
        let dir = dir("torn");
        let path = dir.join("results.sbe");
        write_results(&path, &[5, 5, 6]);
        // Cut off partway through what would be a second sequence-6 result: a whole frame (5,5) then a torn one.
        let mut whole = fs::read(&path).expect("read");
        whole.push(0); // a length prefix, started but not finished
        fs::write(&path, &whole).expect("written");
        let (sequence, _) = trusted_results(&path).expect("checked").expect("a confirmed group");
        assert_eq!(sequence, 5, "a torn frame after the last confirmed group changed which group that was");
    }

    #[test]
    fn repair_truncates_to_what_is_trusted_and_reports_it() {
        let dir = dir("repair");
        let path = dir.join("results.sbe");
        write_results(&path, &[5, 5, 6, 6, 6]);
        let before = fs::metadata(&path).expect("stat").len();
        let sequence = repair_results(&path).expect("repaired").expect("something was trusted");
        assert_eq!(sequence, 5);
        let after = fs::metadata(&path).expect("stat").len();
        assert_eq!(after, 2 * (4 + 16), "the untrusted sequence-6 group was not cut");
        assert!(after < before, "repair did not shrink the file");
    }

    /// Not idempotent, and deliberately so: nothing in the bytes distinguishes a group that is the last one
    /// because a crash cut it short from one that is the last one because nothing has been appended since a
    /// clean repair. A second repair call with nothing written in between no longer trusts the very group the
    /// first one just confirmed — `repair_results` never destroys what it does not trust (only what it does, and
    /// only the untrusted part), so the file is left exactly as the first repair left it, just no longer vouched
    /// for. This is why a caller repairs once, at start, and only ever appends after that; it never repairs again
    /// without new results in between.
    #[test]
    fn repairing_twice_with_nothing_appended_in_between_stops_trusting_further() {
        let dir = dir("repair-twice");
        let path = dir.join("results.sbe");
        write_results(&path, &[5, 5, 6, 6, 6]);
        assert_eq!(repair_results(&path).expect("repaired"), Some(5));
        let after_first = fs::metadata(&path).expect("stat").len();
        assert_eq!(repair_results(&path).expect("repaired"), None, "a solitary group was still trusted");
        assert_eq!(fs::metadata(&path).expect("stat").len(), after_first, "an untrusted repair destroyed data");
    }

    #[test]
    fn repair_of_a_fully_trusted_or_empty_log_touches_nothing() {
        let dir = dir("nothing-to-repair");
        let path = dir.join("results.sbe");
        assert_eq!(repair_results(&path).expect("repaired"), None, "a missing file was reported as trusted");
        write_results(&path, &[5, 5, 5]);
        let before = fs::metadata(&path).expect("stat").len();
        assert_eq!(repair_results(&path).expect("repaired"), None, "an unconfirmed single group was trusted");
        assert_eq!(fs::metadata(&path).expect("stat").len(), before, "repair touched a file it did not trust any of");
    }

    fn fixture_journal_with_snapshot(dir: &Path, at: u64) -> PathBuf {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interfaces/audit/recorded/journal.sbe");
        let journal = dir.join("journal.sbe");
        fs::copy(fixture, &journal).expect("the fixture journal copies");
        crate::snapshots::embed(&journal, Some(at)).expect("a snapshot embeds");
        journal
    }

    #[test]
    fn choose_recovery_falls_back_to_genesis_with_no_snapshot() {
        let dir = dir("choose-no-snapshot");
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interfaces/audit/recorded/journal.sbe");
        let journal = dir.join("journal.sbe");
        fs::copy(fixture, &journal).expect("the fixture journal copies");
        let results = dir.join("results.sbe");
        assert!(matches!(choose_recovery(&journal, &results).expect("decided"), Recovery::Genesis));
    }

    #[test]
    fn choose_recovery_falls_back_to_genesis_with_a_snapshot_but_no_trusted_results() {
        let dir = dir("choose-no-trust");
        let journal = fixture_journal_with_snapshot(&dir, 62);
        let results = dir.join("results.sbe"); // never created
        assert!(matches!(choose_recovery(&journal, &results).expect("decided"), Recovery::Genesis));
    }

    #[test]
    fn choose_recovery_falls_back_to_genesis_when_trust_lags_the_snapshot() {
        let dir = dir("choose-behind");
        let journal = fixture_journal_with_snapshot(&dir, 62);
        let results = dir.join("results.sbe");
        write_results(&results, &[10, 10, 20]); // trusted through 10, well before the snapshot at 62
        assert!(matches!(choose_recovery(&journal, &results).expect("decided"), Recovery::Genesis));
    }

    #[test]
    fn choose_recovery_takes_the_snapshot_when_trust_reaches_exactly_its_sequence() {
        let dir = dir("choose-exact");
        let journal = fixture_journal_with_snapshot(&dir, 62);
        let results = dir.join("results.sbe");
        write_results(&results, &[62, 62, 70]); // trusted through 62 (the group at 70 is last, so discarded)
        match choose_recovery(&journal, &results).expect("decided") {
            Recovery::FromSnapshot { at, trusted_through, .. } => {
                assert_eq!(at, 62);
                assert_eq!(trusted_through, 62);
            }
            Recovery::Genesis => panic!("a snapshot exactly covered by trust fell back to genesis"),
        }
    }

    #[test]
    fn choose_recovery_takes_the_snapshot_when_trust_runs_ahead_of_it() {
        let dir = dir("choose-ahead");
        let journal = fixture_journal_with_snapshot(&dir, 62);
        let results = dir.join("results.sbe");
        write_results(&results, &[90, 90, 100]); // trusted through 90 (the group at 100 is last, so discarded)
        match choose_recovery(&journal, &results).expect("decided") {
            Recovery::FromSnapshot { at, trusted_through, .. } => {
                assert_eq!(at, 62);
                assert_eq!(trusted_through, 90);
            }
            Recovery::Genesis => panic!("a snapshot behind trust fell back to genesis"),
        }
    }

    #[test]
    fn choose_recovery_is_genesis_for_a_node_that_has_never_run() {
        let dir = dir("choose-fresh");
        let journal = dir.join("journal.sbe"); // never created
        let results = dir.join("results.sbe"); // never created
        assert!(matches!(choose_recovery(&journal, &results).expect("decided"), Recovery::Genesis));
    }

    // ---------------------------------------------------------------------------------------------------- cutting

    #[test]
    fn rewrite_from_zero_is_a_no_op() {
        let dir = dir("rewrite-zero");
        let path = dir.join("file");
        fs::write(&path, b"hello").expect("written");
        assert_eq!(rewrite_from(&path, 0).expect("rewritten"), 0);
        assert_eq!(fs::read(&path).expect("read"), b"hello");
    }

    #[test]
    fn rewrite_from_keeps_only_the_tail() {
        let dir = dir("rewrite-tail");
        let path = dir.join("file");
        fs::write(&path, b"0123456789").expect("written");
        let removed = rewrite_from(&path, 4).expect("rewritten");
        assert_eq!(removed, 4);
        assert_eq!(fs::read(&path).expect("read"), b"456789");
    }

    #[test]
    fn results_cut_offset_finds_the_first_group_after_at() {
        let dir = dir("results-cut-offset");
        let path = dir.join("results.sbe");
        write_results(&path, &[5, 5, 6, 6, 7]);
        // Everything at or before 5 is two 16-byte frames, each with a 4-byte length prefix.
        assert_eq!(results_cut_offset(&path, 5).expect("scanned"), 2 * (4 + 16));
        // Nothing after 7: cutting to 7 keeps nothing, so the offset is the whole file.
        let total = fs::metadata(&path).expect("stat").len();
        assert_eq!(results_cut_offset(&path, 7).expect("scanned"), total);
        // Nothing at or before 1: cutting to 1 keeps everything, offset 0.
        assert_eq!(results_cut_offset(&path, 1).expect("scanned"), 0);
    }

    #[test]
    fn starts_at_genesis_is_true_for_an_embedded_but_uncut_journal() {
        let dir = dir("starts-at-genesis-uncut");
        let journal = fixture_journal_with_snapshot(&dir, 62);
        assert!(starts_at_genesis(&journal).expect("checked"), "embedding alone changed where the journal starts");
    }

    #[test]
    fn starts_at_genesis_is_false_after_a_cut() {
        let dir = dir("starts-at-genesis-cut");
        let journal = fixture_journal_with_snapshot(&dir, 62);
        let results = dir.join("results.sbe");
        replay(&journal, &results).expect("the uncut journal replays");
        cut(&journal, &results).expect("cuts");
        assert!(!starts_at_genesis(&journal).expect("checked"), "a cut journal still reported starting at genesis");
    }

    #[test]
    fn cut_refuses_without_a_snapshot() {
        let dir = dir("cut-no-snapshot");
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interfaces/audit/recorded/journal.sbe");
        let journal = dir.join("journal.sbe");
        fs::copy(fixture, &journal).expect("the fixture journal copies");
        let results = dir.join("results.sbe");
        assert!(cut(&journal, &results).is_err());
    }

    #[test]
    fn cut_refuses_when_results_do_not_cover_the_snapshot() {
        let dir = dir("cut-no-trust");
        let journal = fixture_journal_with_snapshot(&dir, 62);
        let results = dir.join("results.sbe");
        write_results(&results, &[10, 10, 20]); // trusted through 10, before the snapshot at 62
        assert!(cut(&journal, &results).is_err());
        // Refusing must not have touched the journal.
        assert!(starts_at_genesis(&journal).expect("checked"), "a refused cut still cut the journal");
    }

    #[test]
    fn cut_succeeds_and_recovery_then_uses_the_snapshot() {
        let dir = dir("cut-succeeds");
        let journal = fixture_journal_with_snapshot(&dir, 62);
        let results = dir.join("results.sbe");
        replay(&journal, &results).expect("the uncut journal replays");
        let (at, journal_removed, results_removed) = cut(&journal, &results).expect("cuts");
        assert_eq!(at, 62);
        assert!(journal_removed > 0, "nothing was removed from the journal");
        assert!(results_removed > 0, "nothing was removed from the results");
        assert!(!starts_at_genesis(&journal).expect("checked"));

        match choose_recovery(&journal, &results).expect("decided") {
            Recovery::FromSnapshot { at: got, .. } => assert_eq!(got, 62),
            Recovery::Genesis => panic!("a cut journal with a covering snapshot fell back to genesis"),
        }
    }

    #[test]
    fn choose_recovery_errors_rather_than_attempting_genesis_on_a_cut_journal_it_cannot_use() {
        let dir = dir("cut-then-stuck");
        let journal = fixture_journal_with_snapshot(&dir, 62);
        let results = dir.join("results.sbe");
        replay(&journal, &results).expect("the uncut journal replays");
        cut(&journal, &results).expect("cuts");
        // Simulate the result log having been lost after the cut: genesis is no longer an option (the journal no
        // longer has sequence 1), and the snapshot alone is not enough without a trusted result log to go with it.
        fs::remove_file(&results).expect("removed");
        let err =
            choose_recovery(&journal, &results).expect_err("a cut journal with no trusted results recovered anyway");
        assert!(err.to_string().contains("cut"), "the error did not explain what went wrong: {err}");
    }

    #[test]
    fn replay_refuses_a_cut_journal() {
        let dir = dir("replay-refuses-cut");
        let journal = fixture_journal_with_snapshot(&dir, 62);
        let results = dir.join("results.sbe");
        replay(&journal, &results).expect("the uncut journal replays");
        cut(&journal, &results).expect("cuts");
        let err = replay(&journal, &dir.join("out.sbe")).expect_err("replay accepted a cut journal");
        assert!(err.to_string().contains("recover"), "the error did not point at recover: {err}");
    }

    fn fixture_snapshot_out(dir: &Path, at: u64) -> PathBuf {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interfaces/audit/recorded/journal.sbe");
        let out = dir.join("snapshot.out");
        snapshots::take(&fixture, &out, Some(at)).expect("a snapshot is taken");
        out
    }

    /// Both ends of what a journal can answer for are decided by its real inputs. A `Snapshot` frame's sequence
    /// names the input it follows, and `embed` appends wherever the file currently ends — so one taken at an
    /// earlier sequence lands *after* later inputs (§3.7). Taking the last frame's sequence, whatever it was,
    /// put the upper bound below the journal's own inputs and refused resumes that were valid.
    ///
    /// Unreachable through a live node today, which is why it is tested here rather than in `feed_resume.rs`:
    /// `serve` appends `EngineStarted` at every start, so a snapshot an offline `embed` left at the tail always
    /// has a real input after it by the time a subscriber can connect.
    #[test]
    fn a_journals_range_is_decided_by_its_inputs_not_by_whichever_frame_sits_last() {
        let dir = dir("range-tail-snapshot");
        let journal = fixture_journal_with_snapshot(&dir, 62);
        let length = journal.metadata().expect("stat").len();
        let (floor, last) = range(&journal, length).expect("read");
        assert_eq!(floor, 0, "an uncut journal carries everything from its first input");
        assert!(last > 62, "the tail snapshot's own sequence was taken as the journal's last: {last}");
    }

    /// A journal founded on a snapshot has no real input to speak for it, so the snapshot's own sequence is both
    /// ends: nothing below it is carried, and nothing above it has happened yet.
    #[test]
    fn a_founded_journal_carries_nothing_below_its_snapshot_and_nothing_above_it() {
        let dir = dir("range-founded");
        let snapshot = fixture_snapshot_out(&dir, 62);
        let fresh = dir.join("fresh");
        seed(&snapshot, &fresh, "small").expect("seeds");
        let journal = fresh.join(JOURNAL);
        let length = journal.metadata().expect("stat").len();
        assert_eq!(range(&journal, length).expect("read"), (62, 62));
    }

    #[test]
    fn starts_at_genesis_is_false_for_a_journal_seed_founded() {
        let dir = dir("starts-at-genesis-seeded");
        let snapshot = fixture_snapshot_out(&dir, 62);
        let fresh = dir.join("fresh");
        seed(&snapshot, &fresh, "small").expect("seeds");
        assert!(
            !starts_at_genesis(&fresh.join(JOURNAL)).expect("checked"),
            "a freshly seeded journal reported starting at genesis"
        );
    }

    #[test]
    fn choose_recovery_trusts_a_freshly_seeded_journal_with_no_results_history_at_all() {
        let dir = dir("choose-seeded");
        let snapshot = fixture_snapshot_out(&dir, 62);
        let fresh = dir.join("fresh");
        seed(&snapshot, &fresh, "small").expect("seeds");
        match choose_recovery(&fresh.join(JOURNAL), &fresh.join(RESULTS)).expect("decided") {
            Recovery::FromSnapshot { at, trusted_through, .. } => {
                assert_eq!(at, 62);
                assert_eq!(trusted_through, 62, "a freshly seeded journal should be trusted exactly to its snapshot");
            }
            Recovery::Genesis => panic!("a freshly seeded journal fell back to genesis, ignoring its snapshot"),
        }
    }

    #[test]
    fn seed_refuses_a_data_dir_that_already_has_a_journal() {
        let dir = dir("seed-refuses-existing");
        let snapshot = fixture_snapshot_out(&dir, 62);
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interfaces/audit/recorded/journal.sbe");
        let existing = dir.join("existing");
        fs::create_dir_all(&existing).expect("a data directory");
        fs::copy(fixture, existing.join(JOURNAL)).expect("an existing journal");
        let err = seed(&snapshot, &existing, "small").expect_err("seed clobbered an existing environment");
        assert!(err.to_string().contains("already has a journal"), "{err}");
    }

    #[test]
    fn seed_refuses_a_file_that_is_not_a_snapshot() {
        let dir = dir("seed-refuses-non-snapshot");
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interfaces/audit/recorded/journal.sbe");
        let fresh = dir.join("fresh");
        let err = seed(&fixture, &fresh, "small").expect_err("seed accepted a plain journal as a snapshot");
        assert!(err.to_string().contains("Snapshot"), "{err}");
    }
}
