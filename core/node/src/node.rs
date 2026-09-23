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
use std::io::{self, BufReader, BufWriter, ErrorKind, Read, Write};
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
pub fn replay(journal: &Path, results: &Path) -> Result<u64, Error> {
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

/// The sequence of the last input in a journal's first `length` bytes. Read by walking the frames, which is what
/// the subscriber's thread is about to do anyway; journals are opened and read here, never held open.
fn last_sequence(path: &Path, length: u64) -> Result<u64, Error> {
    let mut reader = BufReader::new(File::open(path)?.take(length));
    let (mut buf, mut last) = (Vec::new(), 0u64);
    while let Some(n) = read_frame(&mut reader, &mut buf)? {
        if n >= 16 {
            last = u64_at(&buf[..n], 8);
        }
    }
    Ok(last)
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

    // Whether the node can honour it. A reader that asks for a sequence this journal has never reached is one
    // whose node was replaced under it — a fresh data directory, or another group's — and sending it the tail
    // would leave it with a view built on a history that never happened. It is sent everything instead, and told
    // so, because a reader that kept its view has to be able to throw it away.
    let last = last_sequence(journal.0, journal.1)?;
    let resume_from = if asked_for > 0 && asked_for <= last { asked_for } else { 0 };
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
/// covers would leave a gap in `results.sbe` that a subscriber's backlog cannot fill for those sequences (the
/// journal still holds them, unshortened — cutting it is a later step, §8 item 7(b)) — and re-deriving them by
/// replaying from the snapshot defeats what the snapshot was for in the first place. So: no snapshot, or a
/// result log not yet caught up to one that exists, falls back to `Genesis`, the always-safe answer.
fn choose_recovery(journal: &Path, results: &Path) -> Result<Recovery, Error> {
    // A brand-new node, never yet run: nothing to look for a snapshot in. `latest` (like every other reader of a
    // journal here) takes its existence for granted, since every other caller's journal is already there.
    if !journal.exists() {
        return Ok(Recovery::Genesis);
    }
    let snapshot = snapshots::latest(journal)?;
    let trusted = repair_results(results)?;
    Ok(match (snapshot, trusted) {
        (Some((at, frame)), Some(trusted_through)) if trusted_through >= at => {
            Recovery::FromSnapshot { at, frame, trusted_through }
        }
        _ => Recovery::Genesis,
    })
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
            let mut results = BufWriter::new(OpenOptions::new().append(true).open(results_path)?);
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
}
