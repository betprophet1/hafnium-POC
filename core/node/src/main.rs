//! `venue-node`: the single-member node for laptops and test environments. See `core/node/README.md`.

use serde_json::{json, Value};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;
use venue_harness::codec;
use venue_node::frames::{read_frame, write_frame, MAX_FRAME};
use venue_node::node::{self, Config, Error};
use venue_node::snapshots;

const USAGE: &str = "usage:
  venue-node serve [--listen ADDR] [--feed ADDR] [--data DIR] [--group N] [--node-id N] [--tick-ms N] [--fsync]
                   [--capacity small|launch|scale] [--metrics ADDR]
  venue-node send ADDR [INPUTS]            JSON inputs (an array, or one per line; stdin without a file);
                                           prints each result as a JSON line
  venue-node replay JOURNAL RESULTS        runs a journal through a fresh engine and writes its result log
  venue-node snapshot JOURNAL OUT [--at N] replays a journal and writes the state after input N, or after the
                                           last input, as a Snapshot frame (spec §20)
  venue-node embed-snapshot JOURNAL [--at N]
                                           the same, but appends the frame onto JOURNAL itself (D-32) instead of a
                                           file beside it; for a journal no live `serve` has open
  venue-node replay-from SNAPSHOT JOURNAL RESULTS [--snapshot-out FILE]
                                           restores a snapshot and replays only what follows it, writing the
                                           result log for those inputs; the results must equal the tail of a
                                           replay from genesis (E-20.9)
  venue-node recover JOURNAL RESULTS [--snapshot-out FILE]
                                           the same, but restores from the latest snapshot already embedded in
                                           JOURNAL; prints and exits 1 if it has none yet
  venue-node cut-journal JOURNAL RESULTS   removes everything before the latest embedded snapshot from both,
                                           once RESULTS already covers it; refuses otherwise. For a journal no
                                           live `serve` has open
  venue-node seed SNAPSHOT DATA_DIR [--capacity small|launch|scale]
                                           founds a fresh DATA_DIR on SNAPSHOT instead of genesis, so a `serve`
                                           pointed at it starts running from exactly that state; refuses a
                                           DATA_DIR that already has a journal of its own
  venue-node dump FILE                     prints a journal or result log as JSON lines
  venue-node import VECTORS JOURNAL        writes a compiled scenario's inputs, as stamped, as a journal
  venue-node bench JOURNAL [--runs N]      times every input of a journal through a fresh engine; build it with
                                           the perf profile (core/Cargo.toml) for numbers worth reading
  venue-node load ADDR [--connections N] [--seconds N] [--account N] [--instrument N]
                                           amends resting orders through a seeded node's ingress, one request at a
                                           time per connection, and reports messages/s and round-trip percentiles;
                                           --instrument takes an ID the seeded catalogue generated

serve reads VENUE_LISTEN, VENUE_FEED, VENUE_DATA, VENUE_GROUP, VENUE_NODE_ID, VENUE_TICK_MS, VENUE_CAPACITY and
VENUE_METRICS; flags override them. --metrics serves Prometheus metrics at http://ADDR/metrics (none by default), and
VENUE_LOG_FORMAT=json writes its log as JSON lines (deploy/observability/metrics.md). The capacity profile (default small) is recorded beside the journal; serve refuses a journal recorded
under another, and replay and bench use the one recorded.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let outcome = match (args.first().map(String::as_str), args.len()) {
        (Some("serve"), _) => {
            venue_telemetry::log::init("venue-node");
            return match serve(&args[1..]) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    venue_telemetry::log::error("stopped", &[("reason", &e)]);
                    ExitCode::FAILURE
                }
            };
        }
        (Some("send"), 2 | 3) => send(&args[1], args.get(2)),
        (Some("replay"), 3) => {
            node::replay(Path::new(&args[1]), Path::new(&args[2])).map(|n| eprintln!("replayed {n} inputs"))
        }
        (Some("snapshot"), 3 | 5) => snapshot(&args[1..]),
        (Some("embed-snapshot"), 2 | 4) => embed_snapshot(&args[1..]),
        (Some("replay-from"), 4 | 6) => replay_from(&args[1..]),
        (Some("recover"), 3 | 5) => recover(&args[1..]),
        (Some("cut-journal"), 3) => cut_journal(&args[1..]),
        (Some("seed"), 3 | 5) => seed(&args[1..]),
        (Some("dump"), 2) => dump(&args[1]),
        (Some("import"), 3) => import(&args[1], &args[2]),
        (Some("bench"), 2 | 4) => bench(&args[1..]),
        (Some("load"), n) if n >= 2 && n % 2 == 0 => load(&args[1..]),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(64);
        }
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("venue-node: {e}");
            ExitCode::FAILURE
        }
    }
}

fn serve(args: &[String]) -> Result<(), Error> {
    let env = |key: &str, default: &str| std::env::var(key).unwrap_or_else(|_| default.to_string());
    let mut listen = env("VENUE_LISTEN", "0.0.0.0:7101");
    let mut feed = env("VENUE_FEED", "0.0.0.0:7102");
    let mut data = env("VENUE_DATA", "./venue-data");
    let mut group = env("VENUE_GROUP", "1");
    let mut node_id = env("VENUE_NODE_ID", "1");
    let mut tick_ms = env("VENUE_TICK_MS", "100");
    let mut capacity = env("VENUE_CAPACITY", "small");
    let mut metrics = std::env::var("VENUE_METRICS").ok().filter(|m| !m.is_empty());
    let mut fsync = false;
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut value = || it.next().cloned().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--listen" => listen = value()?,
            "--feed" => feed = value()?,
            "--data" => data = value()?,
            "--group" => group = value()?,
            "--node-id" => node_id = value()?,
            "--tick-ms" => tick_ms = value()?,
            "--fsync" => fsync = true,
            "--capacity" => capacity = value()?,
            "--metrics" => metrics = Some(value()?),
            other => return Err(format!("unknown flag {other}\n{USAGE}").into()),
        }
    }
    let config = Config {
        listen: listen.parse()?,
        feed: feed.parse()?,
        data_dir: PathBuf::from(data),
        group: group.parse()?,
        node_id: node_id.parse()?,
        tick: Duration::from_millis(tick_ms.parse()?),
        fsync,
        capacity,
        metrics,
    };
    if config.tick.is_zero() {
        return Err("the tick must be at least 1 ms".into());
    }
    node::serve(&config)
}

fn read_inputs(file: Option<&String>) -> Result<Vec<Value>, Error> {
    let text = match file {
        Some(path) => fs::read_to_string(path)?,
        None => {
            let mut s = String::new();
            io::stdin().read_to_string(&mut s)?;
            s
        }
    };
    if text.trim_start().starts_with('[') {
        return Ok(serde_json::from_str(&text)?);
    }
    let lines = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#'));
    Ok(lines.map(serde_json::from_str).collect::<Result<_, _>>()?)
}

fn send(addr: &str, file: Option<&String>) -> Result<(), Error> {
    let inputs = read_inputs(file)?;
    let stream = TcpStream::connect(addr)?;
    stream.set_nodelay(true)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);
    let (mut buf, mut frame) = (vec![0u8; MAX_FRAME], Vec::new());
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for mut input in inputs {
        let Some(object) = input.as_object_mut() else { return Err(format!("not a JSON object: {input}").into()) };
        // The sequencer stamps the record header (E-17.2); send it zeroed.
        object.insert("record".into(), json!({"sequence": 0, "sequencedAt": 0, "sequencerGroup": 0}));
        let n = codec::encode(&input, &mut buf)?;
        write_frame(&mut writer, &buf[..n])?;
        writer.flush()?;
        loop {
            match read_frame(&mut reader, &mut frame)? {
                Some(0) => break,
                Some(n) => writeln!(out, "{}", codec::decode(&frame[..n])?)?,
                None => return Err("the node closed the connection: see its log".into()),
            }
        }
    }
    Ok(())
}

/// `snapshot JOURNAL OUT [--at N]`. Taken by this replaying engine and never by a serving one (D-32).
fn snapshot(args: &[String]) -> Result<(), node::Error> {
    let at = match args.get(2).map(String::as_str) {
        None => None,
        Some("--at") => Some(args[3].parse::<u64>().map_err(|e| format!("--at: {e}"))?),
        Some(other) => return Err(format!("unknown argument {other}").into()),
    };
    let (sequence, bytes) = snapshots::take(Path::new(&args[0]), Path::new(&args[1]), at)?;
    eprintln!("snapshot at sequence {sequence}, {bytes} bytes of state");
    Ok(())
}

/// `embed-snapshot JOURNAL [--at N]`. Taken by this replaying engine and never by a serving one (D-32), and
/// written into the journal itself rather than a file beside it.
fn embed_snapshot(args: &[String]) -> Result<(), node::Error> {
    let at = match args.get(1).map(String::as_str) {
        None => None,
        Some("--at") => Some(args[2].parse::<u64>().map_err(|e| format!("--at: {e}"))?),
        Some(other) => return Err(format!("unknown argument {other}").into()),
    };
    let (sequence, bytes) = snapshots::embed(Path::new(&args[0]), at)?;
    eprintln!("snapshot embedded at sequence {sequence}, {bytes} bytes of state");
    Ok(())
}

/// `replay-from SNAPSHOT JOURNAL RESULTS [--snapshot-out FILE]`.
fn replay_from(args: &[String]) -> Result<(), node::Error> {
    let finish = match args.get(3).map(String::as_str) {
        None => None,
        Some("--snapshot-out") => Some(PathBuf::from(&args[4])),
        Some(other) => return Err(format!("unknown argument {other}").into()),
    };
    let (at, applied) =
        snapshots::replay_from(Path::new(&args[0]), Path::new(&args[1]), Path::new(&args[2]), finish.as_deref())?;
    eprintln!("restored at sequence {at}, replayed {applied} inputs after it");
    Ok(())
}

/// `recover JOURNAL RESULTS [--snapshot-out FILE]`.
fn recover(args: &[String]) -> Result<(), node::Error> {
    let finish = match args.get(2).map(String::as_str) {
        None => None,
        Some("--snapshot-out") => Some(PathBuf::from(&args[3])),
        Some(other) => return Err(format!("unknown argument {other}").into()),
    };
    match snapshots::recover(Path::new(&args[0]), Path::new(&args[1]), finish.as_deref())? {
        Some((at, applied)) => {
            eprintln!("restored at sequence {at}, replayed {applied} inputs after it");
            Ok(())
        }
        None => Err("that journal carries no embedded snapshot yet (venue-node embed-snapshot)".into()),
    }
}

/// `cut-journal JOURNAL RESULTS`.
fn cut_journal(args: &[String]) -> Result<(), node::Error> {
    let (at, journal_removed, results_removed) = node::cut(Path::new(&args[0]), Path::new(&args[1]))?;
    eprintln!("cut to sequence {at}: journal -{journal_removed} bytes, results -{results_removed} bytes");
    Ok(())
}

/// `seed SNAPSHOT DATA_DIR [--capacity small|launch|scale]`.
fn seed(args: &[String]) -> Result<(), node::Error> {
    let capacity = match args.get(2).map(String::as_str) {
        None => "small",
        Some("--capacity") => &args[3],
        Some(other) => return Err(format!("unknown argument {other}").into()),
    };
    node::seed(Path::new(&args[0]), Path::new(&args[1]), capacity)?;
    eprintln!("{} seeded from {}", args[1], args[0]);
    Ok(())
}

fn dump(path: &str) -> Result<(), Error> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut buf = Vec::new();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    while let Some(n) = read_frame(&mut reader, &mut buf)? {
        writeln!(out, "{}", codec::decode(&buf[..n])?)?;
    }
    Ok(())
}

fn bench(args: &[String]) -> Result<(), Error> {
    let runs = match args {
        [_] => 1,
        [_, flag, n] if flag == "--runs" => n.parse()?,
        _ => return Err(USAGE.into()),
    };
    venue_node::bench::bench(Path::new(&args[0]), runs)
}

fn load(args: &[String]) -> Result<(), Error> {
    let (mut connections, mut seconds) = (1usize, 10u64);
    let (mut account, mut instrument) = (1u64, 10010u64);
    let mut it = args[1..].iter();
    while let Some(flag) = it.next() {
        let value = it.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--connections" => connections = value.parse()?,
            "--seconds" => seconds = value.parse()?,
            "--account" => account = value.parse()?,
            "--instrument" => instrument = value.parse()?,
            other => return Err(format!("unknown flag {other}\n{USAGE}").into()),
        }
    }
    if connections == 0 || seconds == 0 {
        return Err("connections and seconds must be at least 1".into());
    }
    venue_node::load::load(&args[0], connections, seconds, account, instrument)
}

fn import(vectors: &str, journal: &str) -> Result<(), Error> {
    let scenario: Value = serde_json::from_str(&fs::read_to_string(vectors)?)?;
    let steps = scenario["steps"].as_array().ok_or("the file has no steps")?;
    let mut out = BufWriter::new(File::create(journal)?);
    let mut buf = vec![0u8; MAX_FRAME];
    for step in steps {
        let n = codec::encode(&step["input"], &mut buf)?;
        write_frame(&mut out, &buf[..n])?;
    }
    out.flush()?;
    eprintln!("imported {} inputs", steps.len());
    Ok(())
}
