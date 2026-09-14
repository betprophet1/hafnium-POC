#!/usr/bin/env bash
# The core's gates, in order: interfaces, reference model, test vectors, engine guardrails, Rust, engine results.
# ./check at the repository root runs this after the blueprint and separation gates.
#
# With JAVA and SBE_JAR set, the schema is revalidated and core/journal regenerated first. ENGINE_REQUIRED=1 makes
# the engine's scenario and differential results fatal; leave it unset until work package WP-5 has landed.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CORE="$ROOT/core"
HANDWRITTEN=(-p venue-engine -p venue-harness)
step() { printf '\n== %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

step "Interfaces: schema, committed codec, catalogue"
if [[ -n "${JAVA:-}" && -n "${SBE_JAR:-}" ]]; then
  "$ROOT/interfaces/tools/sbe.sh"
else
  echo "JAVA/SBE_JAR not set: not revalidating the schema"
fi
(cd "$ROOT" && sha256sum --quiet -c core/journal/SCHEMA.sha256) \
  || fail "core/journal was generated from a different schema: run interfaces/tools/sbe.sh"
echo "core/journal matches interfaces/sbe/venue-journal.xml"
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
grep -q '^#!\[no_std\]' "$CORE/journal/src/lib.rs" || fail "core/journal must be no_std: regenerate with interfaces/tools/sbe.sh"
if grep -rnwE 'f32|f64' "$CORE/engine/src"; then fail "floating point in the engine"; fi
if grep -rnE 'HashMap|HashSet|Instant::now|SystemTime|thread_rng|getrandom' "$CORE/engine/src"; then
  fail "a determinism leak in the engine"
fi
deps="$(cd "$CORE" && cargo tree -p venue-engine -e normal --prefix none | sed 's/ .*//' | sort -u | tr '\n' ' ')"
[[ "$deps" == "venue-engine venue_journal " ]] || fail "the engine may depend only on venue_journal, found: $deps"

step "Dependencies: bans, licenses and sources"
(cd "$CORE" && cargo deny check bans licenses sources)

step "Rust: format and lints on handwritten crates; build and test the workspace"
(cd "$CORE" && cargo fmt --check "${HANDWRITTEN[@]}" -p proto-l3book -p proto-maskbench)
(cd "$CORE" && cargo clippy "${HANDWRITTEN[@]}" --all-targets -- -D warnings)
(cd "$CORE" && cargo test --workspace)

step "Rust engine against normative scenarios and differential vectors"
if (cd "$CORE" && cargo run -q --release -p venue-harness --bin run-scenarios -- target/compiled target/differential); then
  echo "engine: PASS"
elif [[ "${ENGINE_REQUIRED:-0}" == "1" ]]; then
  fail "the engine does not pass its scenarios"
else
  echo "engine: not passing yet (expected until WP-5; set ENGINE_REQUIRED=1 to make this fatal)"
fi
