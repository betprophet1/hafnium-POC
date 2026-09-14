//! The node: recovery from the journal, the ingress loop, and replay.
//!
//! Every input is stamped, appended to the journal and flushed before the engine applies it, so a journal is
//! always a prefix of what the engine was told, and a restart replays it to the same state and the same result
//! log (interface rules, `interfaces/README.md`). A fail-stop (spec §18) ends the process; restarting replays
//! the journal and stops at the same input, which is the behaviour E-18.2 requires.

use crate::frames::{read_frame, write_frame};
use crate::sequencer::{self, Sequencer};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, ErrorKind, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use venue_engine::frame::{self, Kind};
use venue_engine::state::Capacities;
use venue_engine::{Engine, EngineConfig, ResultSink};
use venue_harness::memory::OwnedMemory;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub const JOURNAL: &str = "journal.sbe";
pub const RESULTS: &str = "results.sbe";

/// Seeds the engine's ID tables. Not observable in results.
const TABLE_SEED: u64 = 0x5eed;

pub struct Config {
    pub listen: SocketAddr,
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

fn engine_config(group: u16) -> Result<EngineConfig, Error> {
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

/// Writes results to the result log, and copies them into a reply when an ingress connection is waiting.
struct Sink<'a> {
    log: &'a mut BufWriter<File>,
    reply: Option<&'a mut Vec<u8>>,
    failed: Option<io::Error>,
}

impl ResultSink for Sink<'_> {
    fn emit(&mut self, encoded: &[u8]) {
        if self.failed.is_none() {
            if let Err(e) = write_frame(&mut *self.log, encoded) {
                self.failed = Some(e);
            }
        }
        if let Some(reply) = self.reply.as_deref_mut() {
            let _ = write_frame(reply, encoded);
        }
    }
}

fn apply(
    engine: &mut Engine<'_>,
    input: &[u8],
    results: &mut BufWriter<File>,
    reply: Option<&mut Vec<u8>>,
) -> Result<(), Error> {
    let mut sink = Sink { log: results, reply, failed: None };
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
}

impl Pipeline {
    fn commit(&mut self, engine: &mut Engine<'_>, input: &mut [u8], reply: Option<&mut Vec<u8>>) -> Result<(), Error> {
        self.sequencer.stamp(input, clock_ns());
        write_frame(&mut self.journal, input)?;
        self.journal.flush()?;
        if self.fsync {
            self.journal.get_ref().sync_data()?;
        }
        apply(engine, input, &mut self.results, reply)
    }
}

enum Ingress {
    Connected(u64, TcpStream),
    Input(u64, Vec<u8>),
    Closed(u64),
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

/// Only client messages and commands may arrive from outside; sequencer events come from the sequencer (E-17.3).
fn admissible(input: &[u8]) -> Result<(), String> {
    match frame::check(input) {
        Ok(f) if f.kind == Kind::Sequencer => Err(format!("template {} is a sequencer event", f.template_id)),
        Ok(_) => Ok(()),
        Err(e) => Err(format!("not a journal input: {e:?}")),
    }
}

/// Recovers from the journal in `data_dir`, writes `EngineStarted`, then serves the TCP ingress and injects
/// `TimeTick` until a fail-stop or an I/O error.
///
/// On the ingress, a connection sends framed inputs with a zeroed record header. For each input it receives the
/// input's results as frames, then an empty frame. An input that is not a client message or command closes the
/// connection and is not journaled.
pub fn serve(config: &Config) -> Result<(), Error> {
    fs::create_dir_all(&config.data_dir)?;
    let journal_path = config.data_dir.join(JOURNAL);
    let hash = binary_hash()?;
    let mut owned = OwnedMemory::new(&Capacities::small());
    let mut engine =
        Engine::new(engine_config(config.group)?, owned.memory()).map_err(|e| format!("engine construction: {e:?}"))?;
    let mut results = BufWriter::new(File::create(config.data_dir.join(RESULTS))?);
    let recovered = run_journal(&mut engine, &journal_path, &mut results, true)?;
    let (last_sequence, last_at) = engine.position();
    eprintln!("recovered {recovered} inputs from {}; last sequence {last_sequence}", journal_path.display());
    let journal = BufWriter::new(OpenOptions::new().append(true).create(true).open(&journal_path)?);
    let mut pipeline = Pipeline {
        sequencer: Sequencer::resume(config.group, last_sequence, last_at),
        journal,
        results,
        fsync: config.fsync,
    };
    pipeline.commit(&mut engine, &mut sequencer::engine_started(config.node_id, &hash), None)?;

    let listener = TcpListener::bind(config.listen)?;
    eprintln!("group {} node {} listening on {}", config.group, config.node_id, listener.local_addr()?);
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || accept(listener, tx));
    let mut clients: BTreeMap<u64, TcpStream> = BTreeMap::new();
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
            Ok(Ingress::Input(id, mut input)) => {
                if let Err(why) = admissible(&input) {
                    eprintln!("connection {id}: refused: {why}");
                    if let Some(stream) = clients.remove(&id) {
                        let _ = stream.shutdown(Shutdown::Both);
                    }
                    continue;
                }
                reply.clear();
                pipeline.commit(&mut engine, &mut input, Some(&mut reply))?;
                write_frame(&mut reply, &[])?;
                // E-17.4: the reply leaves only after the input is journaled and its results exist.
                if let Some(stream) = clients.get_mut(&id) {
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
            pipeline.commit(&mut engine, &mut sequencer::time_tick(), None)?;
            pipeline.results.flush()?;
            next_tick = (next_tick + config.tick).max(now);
        }
    }
}
