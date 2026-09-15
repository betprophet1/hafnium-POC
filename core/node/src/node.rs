//! The node: recovery from the journal, the ingress loop, the feed, and replay.
//!
//! Every input is stamped, appended to the journal and flushed before the engine applies it, so a journal is
//! always a prefix of what the engine was told, and a restart replays it to the same state and the same result
//! log (interface rules, `interfaces/README.md`). A fail-stop (spec §18) ends the process; restarting replays
//! the journal and stops at the same input, which is the behaviour E-18.2 requires.
//!
//! The feed is a development stand-in for interface 12: a reader receives every input with its results, from the
//! start of the journal, then live.

use crate::frames::{read_frame, write_frame};
use crate::sequencer::{self, Sequencer};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use venue_engine::frame::{self, Kind};
use venue_engine::state::Capacities;
use venue_engine::{Engine, EngineConfig, ResultSink};
use venue_harness::memory::OwnedMemory;
use venue_journal::time_tick_codec::SBE_TEMPLATE_ID as TIME_TICK;

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

fn apply(
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

/// Runs every frame of a journal through `engine`, writing results. A torn final frame, left by a crash mid-write,
/// is reported and, when `repair` is set, cut off. Returns the number of inputs applied.
fn run_journal(
    engine: &mut Engine<'_>,
    path: &Path,
    results: &mut BufWriter<File>,
    repair: bool,
) -> Result<u64, Error> {
    let file = OpenOptions::new().read(true).write(repair).create(repair).truncate(false).open(path)?;
    let length = file.metadata()?.len();
    let mut reader = BufReader::new(&file);
    let (mut buf, mut good, mut inputs) = (Vec::new(), 0u64, 0u64);
    loop {
        match read_frame(&mut reader, &mut buf) {
            Ok(Some(n)) => {
                apply(engine, &buf[..n], results, None)?;
                good += 4 + n as u64;
                inputs += 1;
            }
            Ok(None) => break,
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => {
                eprintln!("journal {}: torn final frame after byte {good} of {length}", path.display());
                break;
            }
            Err(e) => return Err(e.into()),
        }
    }
    drop(reader);
    if repair && good < length {
        file.set_len(good)?;
        eprintln!("journal {}: cut to {good} bytes", path.display());
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
    let mut owned = OwnedMemory::new(&Capacities::small());
    let mut engine =
        Engine::new(engine_config(group)?, owned.memory()).map_err(|e| format!("engine construction: {e:?}"))?;
    let mut log = BufWriter::new(File::create(results)?);
    let inputs = run_journal(&mut engine, journal, &mut log, false)?;
    log.flush()?;
    Ok(inputs)
}

/// Stamps, journals and applies inputs, in that order.
struct Pipeline {
    sequencer: Sequencer,
    journal: BufWriter<File>,
    results: BufWriter<File>,
    fsync: bool,
    /// The framed results of the input committed last.
    out: Vec<u8>,
}

impl Pipeline {
    fn commit(&mut self, engine: &mut Engine<'_>, input: &mut [u8]) -> Result<(), Error> {
        self.sequencer.stamp(input, clock_ns());
        write_frame(&mut self.journal, input)?;
        self.journal.flush()?;
        if self.fsync {
            self.journal.get_ref().sync_data()?;
        }
        self.out.clear();
        apply(engine, input, &mut self.results, Some(&mut self.out))
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

fn publish(subscribers: &mut Vec<Sender<Unit>>, input: &[u8], results: &[u8]) -> io::Result<()> {
    if subscribers.is_empty() || quiet(input, results) {
        return Ok(());
    }
    let u = Arc::new(unit(input, results)?);
    subscribers.retain(|s| s.send(Arc::clone(&u)).is_ok());
    Ok(())
}

/// Sends a subscriber the journal and result log up to the given lengths, pairing each input with its results by
/// sequence number, then every live unit. The lengths were taken, and the live channel registered, at one point
/// in the main loop, so nothing is missed or repeated.
fn serve_subscriber(
    stream: TcpStream,
    journal: (&Path, u64),
    results: (&Path, u64),
    live: Receiver<Unit>,
) -> Result<(), Error> {
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut out = BufWriter::new(stream);
    let mut inputs = BufReader::new(File::open(journal.0)?.take(journal.1));
    let mut outputs = BufReader::new(File::open(results.0)?.take(results.1));
    let (mut input, mut result, mut batch) = (Vec::new(), Vec::new(), Vec::new());
    let mut pending = read_frame(&mut outputs, &mut result)?;
    while let Some(n) = read_frame(&mut inputs, &mut input)? {
        let sequence = u64_at(&input, 8);
        batch.clear();
        while let Some(m) = pending {
            if u64_at(&result[..m], 8) != sequence {
                break;
            }
            write_frame(&mut batch, &result[..m])?;
            pending = read_frame(&mut outputs, &mut result)?;
        }
        if !quiet(&input[..n], &batch) {
            out.write_all(&unit(&input[..n], &batch)?)?;
        }
    }
    out.flush()?;
    for u in live {
        out.write_all(&u)?;
        out.flush()?;
    }
    Ok(())
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
    let mut owned = OwnedMemory::new(&Capacities::small());
    let mut engine =
        Engine::new(engine_config(config.group)?, owned.memory()).map_err(|e| format!("engine construction: {e:?}"))?;
    let mut results = BufWriter::new(File::create(&results_path)?);
    let recovered = run_journal(&mut engine, &journal_path, &mut results, true)?;
    let (last_sequence, last_at) = engine.position();
    eprintln!("recovered {recovered} inputs from {}; last sequence {last_sequence}", journal_path.display());
    let journal = BufWriter::new(OpenOptions::new().append(true).create(true).open(&journal_path)?);
    let mut pipeline = Pipeline {
        sequencer: Sequencer::resume(config.group, last_sequence, last_at),
        journal,
        results,
        fsync: config.fsync,
        out: Vec::new(),
    };
    pipeline.commit(&mut engine, &mut sequencer::engine_started(config.node_id, &hash))?;

    let listener = TcpListener::bind(config.listen)?;
    let feed = TcpListener::bind(config.feed)?;
    eprintln!(
        "group {} node {}: ingress on {}, feed on {}",
        config.group,
        config.node_id,
        listener.local_addr()?,
        feed.local_addr()?
    );
    let (tx, rx) = mpsc::channel();
    let feed_tx = tx.clone();
    thread::spawn(move || accept(listener, tx));
    thread::spawn(move || accept_feed(feed, feed_tx));
    let mut clients: BTreeMap<u64, TcpStream> = BTreeMap::new();
    let mut subscribers: Vec<Sender<Unit>> = Vec::new();
    let mut reply = Vec::new();
    let mut next_tick = monotonic() + config.tick;
    loop {
        match rx.recv_timeout(next_tick.saturating_duration_since(monotonic())) {
            Ok(Ingress::Connected(id, stream)) => {
                clients.insert(id, stream);
            }
            Ok(Ingress::Closed(id)) => {
                clients.remove(&id);
            }
            Ok(Ingress::Subscribe(stream)) => {
                pipeline.journal.flush()?;
                pipeline.results.flush()?;
                let lengths = (fs::metadata(&journal_path)?.len(), fs::metadata(&results_path)?.len());
                let (unit_tx, unit_rx) = mpsc::channel();
                subscribers.push(unit_tx);
                let (j, r) = (journal_path.clone(), results_path.clone());
                thread::spawn(move || {
                    if let Err(e) = serve_subscriber(stream, (&j, lengths.0), (&r, lengths.1), unit_rx) {
                        eprintln!("feed subscriber: {e}");
                    }
                });
            }
            Ok(Ingress::Input(id, mut input)) => {
                if let Err(why) = admissible(&input) {
                    eprintln!("connection {id}: refused: {why}");
                    if let Some(stream) = clients.remove(&id) {
                        let _ = stream.shutdown(Shutdown::Both);
                    }
                    continue;
                }
                pipeline.commit(&mut engine, &mut input)?;
                publish(&mut subscribers, &input, &pipeline.out)?;
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
            pipeline.commit(&mut engine, &mut tick)?;
            publish(&mut subscribers, &tick, &pipeline.out)?;
            pipeline.results.flush()?;
            next_tick = (next_tick + config.tick).max(now);
        }
    }
}
