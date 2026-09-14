//! Every input and every expected result in the compiled vectors survives JSON -> SBE -> JSON.
//!
//! This tests the generated codec, not the engine, so it passes before the engine exists. Generate the
//! vectors first with `core/ci.sh`, or:
//!     python3 spec/engine/reference/run_scenarios.py --compile core/target/compiled

use serde_json::{json, Value};
use std::path::PathBuf;
use venue_harness::codec;

fn vector_dirs() -> Vec<PathBuf> {
    let target = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target");
    let dirs: Vec<PathBuf> =
        ["compiled", "differential"].iter().map(|d| target.join(d)).filter(|d| d.is_dir()).collect();
    assert!(
        !dirs.is_empty(),
        "no compiled vectors in core/target: run python3 spec/engine/reference/run_scenarios.py --compile core/target/compiled"
    );
    dirs
}

/// Every key present in `want` has an equal value in `got`, recursively; keys absent from `want` encoded
/// as defaults and are not compared.
fn contains(got: &Value, want: &Value) -> bool {
    match (got, want) {
        (Value::Object(g), Value::Object(w)) => w.iter().all(|(k, v)| g.get(k).is_some_and(|gv| contains(gv, v))),
        (Value::Array(g), Value::Array(w)) => g.len() == w.len() && g.iter().zip(w).all(|(a, b)| contains(a, b)),
        _ => got == want,
    }
}

#[test]
fn inputs_and_results_round_trip() {
    let mut buf = vec![0u8; 1 << 16];
    let mut messages = 0usize;
    for dir in vector_dirs() {
        for path in venue_harness::collect_paths(&[dir.display().to_string()]) {
            let scenario: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            for (n, step) in scenario["steps"].as_array().unwrap().iter().enumerate() {
                let input = &step["input"];
                let len =
                    codec::encode(input, &mut buf).unwrap_or_else(|e| panic!("{}: step {n}: {e}", path.display()));
                let back = codec::decode(&buf[..len]).unwrap();
                assert!(contains(&back, input), "{}: step {n}: input\n  {input}\ndecoded as\n  {back}", path.display());
                messages += 1;
                for expected in step["expect"].as_array().into_iter().flatten() {
                    let mut with_header = expected.clone();
                    with_header["result"] =
                        json!({"inputSequence": 0, "resultIndex": 0, "sequencedAt": 0, "sequencerGroup": 0});
                    let len = codec::encode(&with_header, &mut buf)
                        .unwrap_or_else(|e| panic!("{}: step {n}: {e}", path.display()));
                    let mut back = codec::decode(&buf[..len]).unwrap();
                    back.as_object_mut().unwrap().remove("result");
                    assert_eq!(&back, expected, "{}: step {n}: result did not round-trip", path.display());
                    messages += 1;
                }
            }
        }
    }
    assert!(messages > 0);
    println!("{messages} messages round-tripped");
}
