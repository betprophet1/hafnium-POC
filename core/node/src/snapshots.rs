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

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;

use venue_engine::snapshot::{Refused, STATE_VERSION};
use venue_engine::{Engine, PROTOCOL_VERSION};
use venue_harness::memory::OwnedMemory;
use venue_journal::message_header_codec::{MessageHeaderDecoder, ENCODED_LENGTH as HDR};
use venue_journal::snapshot_codec::{SnapshotDecoder, SnapshotEncoder, SBE_TEMPLATE_ID as SNAPSHOT};
use venue_journal::{Encoder, ReadBuf, WriteBuf};

use crate::frames::{read_frame, template_of, write_frame};
use crate::node::{engine_config, journal_profile, Error};
use venue_telemetry::log;

/// The sequence a journal frame carries, read from its record header without decoding the message. The header sits
/// at a fixed offset in every input (E-17.1), which is what lets a replay skip what a snapshot already covers.
fn sequence_of(frame: &[u8]) -> Option<u64> {
    frame.get(HDR..HDR + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap_or_default()))
}

/// Replays `journal` up to sequence `at` — or to the last input, when `at` is `None` — and returns the sequence
/// reached and the encoded `Snapshot` frame (template 53) describing the state there. Shared by `take`, which
/// writes the frame beside the journal, and `embed`, which writes it into the journal (D-32).
///
/// A `Snapshot` frame already in the journal (from an earlier `embed`) is skipped rather than applied, the same
/// way every reader of a journal skips one: it is not an input.
fn compute(journal: &Path, at: Option<u64>) -> Result<(String, u64, Vec<u8>), Error> {
    let (profile, capacities) = journal_profile(journal)?;
    let mut owned = OwnedMemory::new(&capacities);
    let group = group_of(journal)?;
    let mut engine =
        Engine::new(engine_config(group)?, owned.memory()).map_err(|e| format!("engine construction: {e:?}"))?;

    // Results are wanted by nobody here, but the engine must have somewhere to put them. This goes in a temp
    // directory rather than beside the journal: taking a snapshot is otherwise a read-only operation on a data
    // directory, and writing scratch into it is what would stop a scheduled job from mounting a live venue's
    // volume read-only. The name carries the process id because two of these may run at once.
    let scratch = std::env::temp_dir().join(format!("venue-snapshot-scratch-{}.tmp", std::process::id()));
    let mut sink = BufWriter::new(File::create(&scratch)?);

    // Where to begin. A journal that has been `cut` no longer starts at sequence 1 — its first real input is
    // whatever followed the snapshot it was cut to — so replaying it through a *fresh* engine fail-stops on
    // E-4.1 at the very first input it applies. `choose_recovery` has always known this; taking a snapshot has
    // to know it too, and the same embedded snapshot is what makes it possible. Restoring is also simply
    // cheaper on a journal that has one, cut or not: the work becomes proportional to what has happened since,
    // which is what a snapshot is for. E-20.9 is what says the two give the same state.
    let restored_to = match latest(journal)? {
        Some((sequence, frame)) if at.is_none_or(|stop| sequence <= stop) => restore_into(&mut engine, &frame)?,
        _ => 0,
    };

    let mut reader = BufReader::new(File::open(journal)?);
    let mut buf = Vec::new();
    let mut first_input = None;
    while let Some(n) = read_frame(&mut reader, &mut buf)? {
        if template_of(&buf[..n]) == Some(SNAPSHOT) {
            continue;
        }
        let sequence = sequence_of(&buf[..n]);
        first_input.get_or_insert(sequence.unwrap_or(0));
        // Everything the restored state already covers is skipped, exactly as `replay_from` skips it.
        if sequence.is_some_and(|s| s <= restored_to) {
            continue;
        }
        if let (Some(sequence), Some(stop)) = (sequence, at) {
            if sequence > stop {
                break;
            }
        }
        crate::node::apply(&mut engine, &buf[..n], &mut sink, None)?;
    }
    // A cut journal with nothing to restore from would have fail-stopped above with `SequenceOrTime at input 0`,
    // which says nothing about why. Say why instead.
    if restored_to == 0 && first_input.is_some_and(|first| first > 1) {
        return Err(format!(
            "{} starts at input {}, not 1, and has no embedded snapshot at or below the sequence asked for: it \
             was cut, and what came before it is only in the archive (venue-node restore-cut)",
            journal.display(),
            first_input.unwrap_or(0)
        )
        .into());
    }
    drop(sink);
    let _ = std::fs::remove_file(&scratch);

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
    Ok((profile, sequence, frame))
}

/// Replays `journal` and writes a snapshot of the state after sequence `at` — or after the last input, when `at`
/// is `None` — into `out`. Returns the sequence it was taken at and how many bytes the state took.
///
/// The engine is thrown away afterwards. Nothing is written to the journal being read: a snapshot belongs in the
/// stream a *writer* is appending to, and this is a reader. `embed` is the writer this is beside.
pub fn take(journal: &Path, out: &Path, at: Option<u64>) -> Result<(u64, usize), Error> {
    let (profile, sequence, frame) = compute(journal, at)?;
    let len = frame.len();
    let mut file = BufWriter::new(File::create(out)?);
    write_frame(&mut file, &frame)?;
    file.flush()?;
    log::info(
        "snapshot taken",
        &[("journal", &journal.display()), ("profile", &profile), ("sequence", &sequence), ("out", &out.display())],
    );
    Ok((sequence, len))
}

/// Replays `journal` and appends a snapshot of the state after sequence `at` — or after the last input, when `at`
/// is `None` — directly onto the end of `journal` itself: written into the journal rather than beside it (D-32),
/// which is what lets the journal before it be archived and cut once something does (interfaces/audit/README.md's
/// shipper is the reusable half of that transport; the readiness test still has to be sequence-based, not this).
///
/// For a journal no live `serve` has open. There is no lock on a node's journal today (unlike the gateway's audit
/// instance lock), so running this against one a node is still appending to races that writer; the discipline is
/// the same one interface's "one live process per instance" already asks for, just not yet enforced here.
pub fn embed(journal: &Path, at: Option<u64>) -> Result<(u64, usize), Error> {
    let (profile, sequence, frame) = compute(journal, at)?;
    let len = frame.len();
    let mut file = BufWriter::new(OpenOptions::new().append(true).open(journal)?);
    write_frame(&mut file, &frame)?;
    file.flush()?;
    log::info("snapshot embedded", &[("journal", &journal.display()), ("profile", &profile), ("sequence", &sequence)]);
    Ok((sequence, len))
}

/// The last `Snapshot` frame in `journal`, and the sequence it follows — whichever `embed` left with the greatest
/// `at`, wherever in the file it landed. Position carries no meaning here: `replay_tail` filters what follows a
/// restored engine by sequence, not by where in the file a frame sits, so the frame this returns need not be the
/// physically last one for that to be correct — only the furthest one is worth restoring from at all. `node::cut`
/// relies on that same fact the other way around: it cannot cut by this frame's byte position either, since
/// `embed` appends wherever the file currently ends, not necessarily right after the sequence it describes — a
/// higher-sequence input can physically sit *before* a snapshot embedded after it was already journaled.
///
/// `None` means the journal carries no snapshot yet.
pub fn latest(journal: &Path) -> Result<Option<(u64, Vec<u8>)>, Error> {
    let mut reader = BufReader::new(File::open(journal)?);
    let mut buf = Vec::new();
    let mut found: Option<(u64, Vec<u8>)> = None;
    while let Some(n) = read_frame(&mut reader, &mut buf)? {
        if template_of(&buf[..n]) != Some(SNAPSHOT) {
            continue;
        }
        let at = sequence_of(&buf[..n]).ok_or("a Snapshot frame with no record header")?;
        if found.as_ref().is_none_or(|(furthest, _)| at > *furthest) {
            found = Some((at, buf[..n].to_vec()));
        }
    }
    Ok(found)
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

/// Decodes a `Snapshot` frame and restores its state into `engine`. Returns the sequence it describes — the
/// input it follows, not one of its own (interfaces/sbe/venue-journal.xml, Snapshot).
pub(crate) fn restore_into(engine: &mut Engine<'_>, frame: &[u8]) -> Result<u64, Error> {
    if template_of(frame) != Some(SNAPSHOT) {
        return Err("that frame is not a Snapshot (template 53)".into());
    }
    let read = ReadBuf::new(frame);
    let header = MessageHeaderDecoder::default().wrap(read, 0);
    let mut dec = SnapshotDecoder::default().header(header, 0);
    let mut record = dec.record_decoder();
    let sequence = record.sequence();
    dec = record.parent().map_err(|e| format!("snapshot record: {e:?}"))?;
    let state_version = dec.state_version();
    let (offset, length) = dec.state_decoder();
    engine.restore(state_version, &frame[offset..offset + length]).map_err(|e| match e {
        Refused::Version(v) => format!("this build does not implement snapshot encoding {v}"),
        other => format!("the snapshot was refused: {other:?}"),
    })?;
    Ok(sequence)
}

/// Replays `journal` into `engine` — already restored to the state after sequence `at` — writing the result log
/// for what follows to `results`. Returns how many inputs were applied.
///
/// Filters by sequence, not by position: everything at or before `at` is skipped, whether it sits before or after
/// wherever a `Snapshot` frame landed in the file, and every `Snapshot` frame is skipped outright, an embedded one
/// included, since another can only describe an earlier or the same point (E-4.1 would fail-stop on the first
/// already-applied input otherwise, because the engine is already past it).
fn replay_tail(engine: &mut Engine<'_>, journal: &Path, at: u64, results: &Path) -> Result<u64, Error> {
    let mut log = BufWriter::new(File::create(results)?);
    let mut reader = BufReader::new(File::open(journal)?);
    let mut buf = Vec::new();
    let mut applied = 0u64;
    while let Some(n) = read_frame(&mut reader, &mut buf)? {
        if template_of(&buf[..n]) == Some(SNAPSHOT) {
            continue;
        }
        match sequence_of(&buf[..n]) {
            Some(sequence) if sequence <= at => continue,
            _ => {}
        }
        crate::node::apply(engine, &buf[..n], &mut log, None)?;
        applied += 1;
    }
    log.flush()?;
    Ok(applied)
}

/// A snapshot of `engine`'s current state, written to `path` — for a caller comparing the state two routes reach
/// the same input by (E-20.9). Taken here because nothing else can see the engine.
fn finish_snapshot(path: &Path, group: u16, engine: &mut Engine<'_>) -> Result<(), Error> {
    let (sequence, when) = engine.position();
    let mut state = vec![0u8; 1 << 24];
    let len = engine.snapshot(&mut state).map_err(|e| format!("snapshot: {e:?}"))?;
    state.truncate(len);
    let frame = encode(group, sequence, when, engine, &state)?;
    let mut file = BufWriter::new(File::create(path)?);
    write_frame(&mut file, &frame)?;
    file.flush()?;
    Ok(())
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
    let at = restore_into(&mut engine, &buf[..n])?;
    let applied = replay_tail(&mut engine, journal, at, results)?;
    if let Some(path) = finish {
        finish_snapshot(path, group, &mut engine)?;
    }
    log::info(
        "replayed from a snapshot",
        &[("snapshot", &snapshot.display()), ("at", &at), ("inputs", &applied), ("results", &results.display())],
    );
    Ok((at, applied))
}

/// The single-journal-file version of `replay_from`: restores from the latest `Snapshot` frame already embedded
/// in `journal` (`embed`; `latest` finds it) rather than a separate snapshot file, and replays what follows it.
/// Returns the sequence restored to and how many inputs were applied after it, or `None` when the journal carries
/// no snapshot yet — the caller's cue to fall back to a full replay from genesis (`replay`).
///
/// Not yet called by `serve`'s own recovery: doing that safely still needs the result log to survive a restart
/// rather than being rebuilt by replaying from genesis every time (`core/node/README.md`, Result log), because a
/// recovery that skips 1..`at` has nowhere to regenerate those inputs' results from. This is the tested mechanism
/// that step will use once the result log can carry its own history across a restart.
pub fn recover(journal: &Path, results: &Path, finish: Option<&Path>) -> Result<Option<(u64, u64)>, Error> {
    let Some((_, frame)) = latest(journal)? else { return Ok(None) };
    let (_, capacities) = journal_profile(journal)?;
    let mut owned = OwnedMemory::new(&capacities);
    let group = group_of(journal)?;
    let mut engine =
        Engine::new(engine_config(group)?, owned.memory()).map_err(|e| format!("engine construction: {e:?}"))?;
    let at = restore_into(&mut engine, &frame)?;
    let applied = replay_tail(&mut engine, journal, at, results)?;
    if let Some(path) = finish {
        finish_snapshot(path, group, &mut engine)?;
    }
    log::info(
        "recovered from an embedded snapshot",
        &[("journal", &journal.display()), ("at", &at), ("inputs", &applied), ("results", &results.display())],
    );
    Ok(Some((at, applied)))
}
