#!/usr/bin/env bash
# Every gate an implementation of the core must pass, in order. Run from anywhere.
#
#   JAVA=/path/to/java SBE_JAR=/path/to/sbe-all-1.40.1.jar core/ci.sh
#
# Without JAVA and SBE_JAR the existing interfaces/generated is used. ENGINE_REQUIRED=1 makes the engine's
# scenario and differential results fatal; leave it unset until work package WP-5 has landed.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CORE="$ROOT/core"
step() { printf '\n== %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

step "Interfaces: SBE validation, generated codecs, codec round trips"
if [[ -n "${JAVA:-}" && -n "${SBE_JAR:-}" ]]; then
  "$ROOT/interfaces/tools/sbe.sh"
else
  [[ -f "$ROOT/interfaces/generated/rust/venue_journal/Cargo.toml" ]] \
    || fail "set JAVA and SBE_JAR to generate interfaces/generated (see interfaces/README.md)"
  echo "JAVA/SBE_JAR not set: using the existing interfaces/generated"
fi
python3 "$ROOT/interfaces/tools/catalogue.py" check

step "Reference model: normative scenarios and random sequences"
python3 "$ROOT/spec/engine/reference/run_scenarios.py"
python3 "$ROOT/spec/engine/reference/fuzz.py" --seeds "${FUZZ_SEEDS:-20}" --steps 300

step "Test vectors for implementations"
rm -rf "$CORE/target/compiled" "$CORE/target/differential"
python3 "$ROOT/spec/engine/reference/run_scenarios.py" --compile "$CORE/target/compiled"
python3 "$ROOT/spec/engine/reference/fuzz.py" --seeds "${DIFF_SEEDS:-10}" --steps 400 --first 90000 \
  --export "$CORE/target/differential" | head -1

step "Generated JSON codec is current"
python3 "$ROOT/interfaces/tools/gen_rust_json_codec.py" --check

step "Engine guardrails (blueprint §4, spec E-1.1)"
grep -q '^#!\[no_std\]' "$CORE/engine/src/lib.rs" || fail "the engine crate must be #![no_std]"
grep -q '^#!\[forbid(unsafe_code)\]' "$CORE/engine/src/lib.rs" || fail "the engine crate must forbid unsafe code"
if grep -rnwE 'f32|f64' "$CORE/engine/src"; then fail "floating point in the engine"; fi
if grep -rnE 'HashMap|HashSet|Instant::now|SystemTime|thread_rng|getrandom' "$CORE/engine/src"; then
  fail "a determinism leak in the engine"
fi
deps="$(cd "$CORE" && cargo tree -p venue-engine -e normal --prefix none | sed 's/ .*//' | sort -u | tr '\n' ' ')"
[[ "$deps" == "venue-engine venue_journal " ]] || fail "the engine may depend only on venue_journal, found: $deps"

step "Rust: format, lints, tests"
(cd "$CORE" && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test)

step "Rust engine against normative scenarios and differential vectors"
if (cd "$CORE" && cargo run -q --release -p venue-harness --bin run-scenarios -- target/compiled target/differential); then
  echo "engine: PASS"
elif [[ "${ENGINE_REQUIRED:-0}" == "1" ]]; then
  fail "the engine does not pass its scenarios"
else
  echo "engine: not passing yet (expected until WP-5; set ENGINE_REQUIRED=1 to make this fatal)"
fi
