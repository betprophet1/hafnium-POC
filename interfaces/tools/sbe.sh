#!/usr/bin/env bash
# Validate the SBE schemas with the real SBE tool (warnings are errors), generate Rust and Java,
# and run the Rust round-trip tests.  Needs JAVA (JDK 17+) and SBE_JAR (sbe-all 1.40.1).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
: "${JAVA:?set JAVA to a JDK 17+ java binary}"
: "${SBE_JAR:?set SBE_JAR to sbe-all-1.40.1.jar}"
OUT="$ROOT/interfaces/generated"
rm -rf "$OUT" && mkdir -p "$OUT"
for schema in "$ROOT"/interfaces/sbe/*.xml; do
  for lang in Rust Java; do
    "$JAVA" -Dsbe.validation.stop.on.error=true -Dsbe.validation.warnings.fatal=true \
            -Dsbe.target.language="$lang" -Dsbe.output.dir="$OUT/${lang,,}" \
            -jar "$SBE_JAR" "$schema"
  done
  echo "valid: ${schema#$ROOT/}"
done
if [ -f "$ROOT/interfaces/rust-roundtrip/Cargo.toml" ]; then
  (cd "$ROOT/interfaces/rust-roundtrip" && cargo test --quiet)
fi
