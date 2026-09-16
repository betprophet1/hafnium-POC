//! `venue-node bench`: engine latency on a recorded journal, for performance testing.
//!
//! Build it with the `perf` profile and a named target CPU (see `core/Cargo.toml`). The whole journal is read into
//! memory before anything is timed, so disk I/O is not in the numbers. Each run applies every input to a fresh engine
//! through a sink that only counts results, and times each `apply` on its own. The report gives throughput and, per
//! input message, latency percentiles: the engine's share of blueprint §9's matching-and-clearing budget. On a
//! virtual machine the tail reflects the hypervisor as much as the engine; compare builds and code, do not quote the
//! numbers as the venue's latency.

use crate::node::{engine_config, Error};
use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Instant;
use venue_engine::frame;
use venue_engine::{Engine, ResultSink};
use venue_harness::codec;
use venue_harness::memory::OwnedMemory;

/// Benchmark timing only; nothing measured here reaches a journal.
#[allow(clippy::disallowed_methods)]
fn now() -> Instant {
    Instant::now()
}

/// Counts results and keeps nothing, so the sink costs as little as possible.
#[derive(Default)]
struct Count {
    results: u64,
}

impl ResultSink for Count {
    fn emit(&mut self, _encoded: &[u8]) {
        self.results += 1;
    }
}

/// Every frame of a journal in one buffer, as `(start, length)` spans.
struct Frames {
    bytes: Vec<u8>,
    spans: Vec<(usize, usize)>,
}

fn load(path: &Path) -> Result<Frames, Error> {
    let bytes = fs::read(path)?;
    let mut spans = Vec::new();
    let mut at = 0;
    while at + 4 <= bytes.len() {
        let len = u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize;
        if at + 4 + len > bytes.len() {
            // A torn final frame, which recovery would cut off.
            break;
        }
        spans.push((at + 4, len));
        at += 4 + len;
    }
    Ok(Frames { bytes, spans })
}

/// The value at `q` hundred-thousandths of a sorted sample.
fn percentile(sorted: &[u64], q: usize) -> u64 {
    match sorted.len() {
        0 => 0,
        n => sorted[(n - 1) * q / 100_000],
    }
}

fn micros(ns: u64) -> String {
    format!("{}.{:03}", ns / 1000, ns % 1000)
}

fn row(name: &str, sorted: &[u64]) {
    let p = |q| micros(percentile(sorted, q));
    println!(
        "  {name:<18} {:>9} {:>9} {:>9} {:>9} {:>9} {:>10}",
        sorted.len(),
        p(50_000),
        p(90_000),
        p(99_000),
        p(99_900),
        micros(sorted.last().copied().unwrap_or(0))
    );
}

/// The median cost of the pair of clock readings around each input.
fn timer_floor() -> u64 {
    let mut sample: Vec<u64> = (0..200_000).map(|_| now().elapsed().as_nanos() as u64).collect();
    sample.sort_unstable();
    percentile(&sample, 50_000)
}

pub fn bench(journal: &Path, runs: usize) -> Result<(), Error> {
    let frames = load(journal)?;
    let &(start, len) = frames.spans.first().ok_or("the journal is empty")?;
    let head = &frames.bytes[start..start + len];
    let group = if head.len() >= 26 { u16::from_le_bytes([head[24], head[25]]) } else { 1 };
    // Name each message once, from its first frame. Nothing here is timed.
    let mut names: BTreeMap<u16, String> = BTreeMap::new();
    let mut templates = Vec::with_capacity(frames.spans.len());
    for &(start, len) in &frames.spans {
        let input = &frames.bytes[start..start + len];
        let id = frame::check(input).map_err(|_| "the journal holds a malformed frame")?.template_id;
        if let Entry::Vacant(slot) = names.entry(id) {
            slot.insert(codec::decode(input)?["msg"].as_str().unwrap_or("?").to_string());
        }
        templates.push(id);
    }
    println!(
        "{} inputs from {}; clock readings cost {} µs per input and are included below",
        frames.spans.len(),
        journal.display(),
        micros(timer_floor())
    );
    let (_, capacities) = crate::node::journal_profile(journal)?;
    for run in 1..=runs {
        let mut owned = OwnedMemory::new(&capacities);
        let mut engine =
            Engine::new(engine_config(group)?, owned.memory()).map_err(|e| format!("engine construction: {e:?}"))?;
        let mut sink = Count::default();
        let mut samples: BTreeMap<u16, Vec<u64>> = BTreeMap::new();
        let mut applying = 0u128;
        let started = now();
        for (i, &(start, len)) in frames.spans.iter().enumerate() {
            let input = &frames.bytes[start..start + len];
            let t = now();
            let outcome = engine.apply(input, &mut sink);
            let ns = t.elapsed().as_nanos() as u64;
            outcome.map_err(|stop| format!("engine fail-stop {stop:?} at input {} (spec §18)", i + 1))?;
            applying += u128::from(ns);
            samples.entry(templates[i]).or_default().push(ns);
        }
        let wall = started.elapsed();
        let mut all: Vec<u64> = samples.values().flatten().copied().collect();
        all.sort_unstable();
        println!(
            "run {run}: {} results, {} inputs/s inside apply ({} ms applying, {} ms wall)",
            sink.results,
            frames.spans.len() as u128 * 1_000_000_000 / applying.max(1),
            applying / 1_000_000,
            wall.as_millis()
        );
        println!(
            "  {:<18} {:>9} {:>9} {:>9} {:>9} {:>9} {:>10}",
            "µs per input", "count", "p50", "p90", "p99", "p99.9", "max"
        );
        row("all inputs", &all);
        if run == runs {
            for (id, mut sample) in samples {
                sample.sort_unstable();
                row(names.get(&id).map_or("?", String::as_str), &sample);
            }
        }
    }
    Ok(())
}
