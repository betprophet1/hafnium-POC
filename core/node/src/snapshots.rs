//! Taking a snapshot from a journal, and starting a replay from one (spec §20).
//!
//! Both are done by an engine replaying the journal, never by the one serving traffic (D-32): the hot path does
//! not pause to serialise itself, and replay is deterministic, so the snapshot a replaying engine takes is the one
//! the serving engine would have taken. That is what E-20.9 says, and it is why this lives beside `replay` rather
//! than inside `serve`.
//!
//! What it buys is the thing the journal could not do before. Recovery replayed from sequence 1 because there was
//! nowhere else to begin, so the journal could never be cut, so it could never be archived. A snapshot at N is a
//! place to begin: everything before it can go to object storage and be taken off the disk.

use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;

use venue_engine::snapshot::{Refused, STATE_VERSION};
use venue_engine::{Engine, PROTOCOL_VERSION};
use venue_harness::memory::OwnedMemory;
use venue_journal::message_header_codec::{MessageHeaderDecoder, ENCODED_LENGTH as HDR};
use venue_journal::snapshot_codec::{SnapshotDecoder, SnapshotEncoder, SBE_TEMPLATE_ID as SNAPSHOT};
use venue_journal::{Encoder, ReadBuf, WriteBuf};

use crate::frames::{read_frame, write_frame};
use crate::node::{engine_config, journal_profile, Error};
use venue_telemetry::log;

/// The sequence a journal frame carries, read from its record header without decoding the message. The header sits
/// at a fixed offset in every input (E-17.1), which is what lets a replay skip what a snapshot already covers.
fn sequence_of(frame: &[u8]) -> Option<u64> {
    frame.get(HDR..HDR + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap_or_default()))
}

/// The template a frame carries, so a snapshot can be told from an input.
fn template_of(frame: &[u8]) -> Option<u16> {
    frame.get(2..4).map(|b| u16::from_le_bytes(b.try_into().unwrap_or_default()))
}

/// Replays `journal` and writes a snapshot of the state after sequence `at` — or after the last input, when `at`
/// is `None` — into `out`. Returns the sequence it was taken at and how many bytes the state took.
///
/// The engine is thrown away afterwards. Nothing is written to the journal being read: a snapshot belongs in the
/// stream a *writer* is appending to, and this is a reader.
pub fn take(journal: &Path, out: &Path, at: Option<u64>) -> Result<(u64, usize), Error> {
    let (profile, capacities) = journal_profile(journal)?;
    let mut owned = OwnedMemory::new(&capacities);
    let group = group_of(journal)?;
    let mut engine =
        Engine::new(engine_config(group)?, owned.memory()).map_err(|e| format!("engine construction: {e:?}"))?;

    // Results are wanted by nobody here, but the engine must have somewhere to put them.
    let mut sink = BufWriter::new(File::create(out.with_extension("results.tmp"))?);
    let mut reader = BufReader::new(File::open(journal)?);
    let mut buf = Vec::new();
    let mut applied = 0u64;
    while let Some(n) = read_frame(&mut reader, &mut buf)? {
        if let (Some(sequence), Some(stop)) = (sequence_of(&buf[..n]), at) {
            if sequence > stop {
                break;
            }
        }
        crate::node::apply(&mut engine, &buf[..n], &mut sink, None)?;
        applied += 1;
    }
    drop(sink);
    let _ = std::fs::remove_file(out.with_extension("results.tmp"));

    let (sequence, when) = engine.position();
    if let Some(stop) = at {
        if sequence != stop {
            return Err(format!("the journal ends at {sequence}, before the requested {stop}").into());
        }
    }

    let mut state = vec![0u8; 1 << 24];
    let len = engine.snapshot(&mut state).map_err(|e| format!("snapshot: {e:?}"))?;
    state.truncate(len);

    let frame = encode(group, sequence, when, &engine, &state)?;
    let mut file = BufWriter::new(File::create(out)?);
    write_frame(&mut file, &frame)?;
    file.flush()?;
    log::info(
        "snapshot taken",
        &[
            ("journal", &journal.display()),
            ("profile", &profile),
            ("inputs", &applied),
            ("sequence", &sequence),
            ("state_bytes", &len),
            ("out", &out.display()),
        ],
    );
    Ok((sequence, len))
}

/// The `Snapshot` message (template 53) around a state blob.
fn encode(group: u16, sequence: u64, at: u64, engine: &Engine<'_>, state: &[u8]) -> Result<Vec<u8>, Error> {
    let mut buf = vec![0u8; state.len() + 256];
    let mut enc = SnapshotEncoder::default().wrap(WriteBuf::new(&mut buf), HDR);
    enc = enc.header(0).parent().map_err(|e| format!("snapshot header: {e:?}"))?;
    let mut record = enc.record_encoder();
    record.sequence(sequence);
    record.sequenced_at(at);
    record.sequencer_group(group);
    enc = record.parent().map_err(|e| format!("snapshot record: {e:?}"))?;
    enc.node_id(0);
    enc.protocol_version(PROTOCOL_VERSION);
    enc.state_version(STATE_VERSION);
    enc.binary_hash(&engine.config().binary_hash);
    enc.state(state);
    let len = enc.get_limit();
    buf.truncate(len);
    Ok(buf)
}

/// The sequencer group a journal belongs to, read from its first frame as `replay` does.
fn group_of(journal: &Path) -> Result<u16, Error> {
    let mut reader = BufReader::new(File::open(journal)?);
    let mut buf = Vec::new();
    Ok(match read_frame(&mut reader, &mut buf)? {
        Some(n) if n >= HDR + 18 => u16::from_le_bytes([buf[HDR + 16], buf[HDR + 17]]),
        Some(_) => return Err("the journal's first frame is too short for a record header".into()),
        None => 1,
    })
}

/// Restores `snapshot` into a fresh engine and replays the part of `journal` that follows it, writing the result
/// log for those inputs only. Returns the sequence restored to and how many inputs were applied after it.
///
/// This is the half of E-20.9 a journal can be checked against: the results here must equal the tail of a replay
/// from genesis, byte for byte.
pub fn replay_from(
    snapshot: &Path,
    journal: &Path,
    results: &Path,
    finish: Option<&Path>,
) -> Result<(u64, u64), Error> {
    let (_, capacities) = journal_profile(journal)?;
    let mut owned = OwnedMemory::new(&capacities);
    let group = group_of(journal)?;
    let mut engine =
        Engine::new(engine_config(group)?, owned.memory()).map_err(|e| format!("engine construction: {e:?}"))?;

    let mut reader = BufReader::new(File::open(snapshot)?);
    let mut buf = Vec::new();
    let n = read_frame(&mut reader, &mut buf)?.ok_or("the snapshot file is empty")?;
    if template_of(&buf[..n]) != Some(SNAPSHOT) {
        return Err("that file does not begin with a Snapshot frame".into());
    }
    let (at, state_version, offset, length) = {
        let read = ReadBuf::new(&buf[..n]);
        let header = MessageHeaderDecoder::default().wrap(read, 0);
        let mut dec = SnapshotDecoder::default().header(header, 0);
        let mut record = dec.record_decoder();
        let sequence = record.sequence();
        dec = record.parent().map_err(|e| format!("snapshot record: {e:?}"))?;
        let version = dec.state_version();
        let (offset, length) = dec.state_decoder();
        (sequence, version, offset, length)
    };
    engine.restore(state_version, &buf[offset..offset + length]).map_err(|e| match e {
        Refused::Version(v) => format!("this build does not implement snapshot encoding {v}"),
        other => format!("the snapshot was refused: {other:?}"),
    })?;

    let mut log = BufWriter::new(File::create(results)?);
    let mut reader = BufReader::new(File::open(journal)?);
    let mut applied = 0u64;
    while let Some(n) = read_frame(&mut reader, &mut buf)? {
        // Everything the snapshot already accounts for is skipped, not re-applied: E-4.1 would fail-stop on the
        // first of them, because the engine is already past it.
        match sequence_of(&buf[..n]) {
            Some(sequence) if sequence <= at => continue,
            _ => {}
        }
        crate::node::apply(&mut engine, &buf[..n], &mut log, None)?;
        applied += 1;
    }
    log.flush()?;
    // A snapshot of where this finished, for a caller that wants to compare the state two routes to the same
    // input reach (E-20.9). Taken here because nothing else can see the engine.
    if let Some(path) = finish {
        let (sequence, when) = engine.position();
        let mut state = vec![0u8; 1 << 24];
        let len = engine.snapshot(&mut state).map_err(|e| format!("snapshot: {e:?}"))?;
        state.truncate(len);
        let frame = encode(group, sequence, when, &engine, &state)?;
        let mut file = BufWriter::new(File::create(path)?);
        write_frame(&mut file, &frame)?;
        file.flush()?;
    }
    log::info(
        "replayed from a snapshot",
        &[("snapshot", &snapshot.display()), ("at", &at), ("inputs", &applied), ("results", &results.display())],
    );
    Ok((at, applied))
}
