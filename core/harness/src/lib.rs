//! Runs compiled scenarios against the Rust engine.
//!
//! A compiled scenario is produced by `spec/engine/reference/run_scenarios.py --compile` (normative, from
//! the hand-computed scenarios) or `fuzz.py --export` (differential, from the reference model). Every input
//! is explicit and every expected result fully specified, so this harness needs no defaults of its own: it
//! encodes each input with the generated codecs, applies it, decodes the results, checks each result's
//! header (E-5.1) and compares the rest exactly.

#[rustfmt::skip]
pub mod codec;
pub mod memory;

use memory::OwnedMemory;
use serde_json::Value;
use std::fs;
use std::path::Path;
use venue_engine::state::Capacities;
use venue_engine::{Engine, EngineConfig, FailStop, ResultSink};

/// Collects encoded results.
#[derive(Default)]
pub struct Collect(pub Vec<Vec<u8>>);

impl ResultSink for Collect {
    fn emit(&mut self, encoded: &[u8]) {
        self.0.push(encoded.to_vec());
    }
}

/// Runs one compiled scenario file. `Err` describes the first difference.
pub fn run_file(path: &Path) -> Result<usize, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let scenario: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let group = scenario["group"].as_u64().unwrap_or(1) as u16;
    let mut owned = OwnedMemory::new(&Capacities::small());
    let config = EngineConfig { sequencer_group: group, binary_hash: [0; 32], table_seed: 0x5eed };
    let mut engine = Engine::new(config, owned.memory()).map_err(|e| format!("engine construction: {e:?}"))?;
    let mut buf = vec![0u8; 1 << 16];
    let steps = scenario["steps"].as_array().ok_or("no steps")?;
    for (n, step) in steps.iter().enumerate() {
        let input = &step["input"];
        let len = codec::encode(input, &mut buf).map_err(|e| format!("step {n}: encode: {e}"))?;
        let mut sink = Collect::default();
        let outcome = engine.apply(&buf[..len], &mut sink);
        let expect_fail_stop = step["expectFailStop"].as_bool().unwrap_or(false);
        match (outcome, expect_fail_stop) {
            (Err(_), true) => return Ok(n + 1),
            (Ok(()), true) => return Err(format!("step {n}: expected a fail-stop")),
            (Err(stop), false) => return Err(format!("step {n} ({}): fail-stop {stop:?}", input["msg"])),
            (Ok(()), false) => {}
        }
        let got = decode_results(n, input, &sink)?;
        if step["setup"].as_bool().unwrap_or(false) {
            for r in &got {
                if r["msg"] == "CommandApplied" && r["outcome"] != "APPLIED" {
                    return Err(format!("step {n}: setup command not applied: {r}"));
                }
            }
        }
        if let Some(expect) = step["expect"].as_array() {
            if &got != expect {
                return Err(describe_difference(n, input, &got, expect));
            }
        }
    }
    Ok(steps.len())
}

fn decode_results(n: usize, input: &Value, sink: &Collect) -> Result<Vec<Value>, String> {
    let record = &input["record"];
    let mut out = Vec::with_capacity(sink.0.len());
    for (index, bytes) in sink.0.iter().enumerate() {
        let mut result = codec::decode(bytes).map_err(|e| format!("step {n}: decode result {index}: {e}"))?;
        let header = result
            .as_object_mut()
            .and_then(|m| m.remove("result"))
            .ok_or_else(|| format!("step {n}: result {index} has no ResultHeader"))?;
        let want = serde_json::json!({
            "inputSequence": record["sequence"],
            "resultIndex": index,
            "sequencedAt": record["sequencedAt"],
            "sequencerGroup": record["sequencerGroup"],
        });
        if header != want {
            return Err(format!("step {n}: result {index} header {header} should be {want}"));
        }
        out.push(result);
    }
    Ok(out)
}

fn describe_difference(n: usize, input: &Value, got: &[Value], expect: &[Value]) -> String {
    let mut s = format!("step {n} ({}) results differ:\n", input["msg"]);
    for k in 0..got.len().max(expect.len()) {
        let g = got.get(k);
        let w = expect.get(k);
        if g == w {
            s.push_str(&format!("     [{k}] {}\n", g.map_or(Value::Null, Clone::clone)));
        } else {
            s.push_str(&format!("  !! [{k}] engine:   {}\n", g.map_or(Value::Null, Clone::clone)));
            s.push_str(&format!("  !! [{k}] expected: {}\n", w.map_or(Value::Null, Clone::clone)));
        }
    }
    s
}

/// Lists compiled scenario files under each argument, which may be a file or a directory.
pub fn collect_paths(args: &[String]) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for a in args {
        let p = Path::new(a);
        if p.is_dir() {
            let mut files: Vec<_> =
                fs::read_dir(p).map(|d| d.filter_map(Result::ok).map(|e| e.path()).collect()).unwrap_or_default();
            files.retain(|f| f.extension().is_some_and(|x| x == "json"));
            files.sort();
            out.extend(files);
        } else {
            out.push(p.to_path_buf());
        }
    }
    out
}

/// Keeps `FailStop` in the public surface so harness users can match on it.
pub type Stop = FailStop;
