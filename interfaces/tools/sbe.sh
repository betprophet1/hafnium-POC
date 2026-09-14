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
# The engine is no_std (blueprint §4). sbe-tool 1.40.1's Rust output uses two std paths with core equivalents;
# rewrite them, mark the crate no_std and unpublished, and fail if any std path remains.
for crate in "$OUT"/rust/*/; do
  sed -i 's/std::iter::repeat/core::iter::repeat/g; s/impl std::error::Error for SbeErr/impl core::error::Error for SbeErr/' "$crate"src/*.rs
  sed -i '1i #![no_std]' "$crate"src/lib.rs
  sed -i 's/^\[package\]$/[package]\npublish = false/' "$crate"Cargo.toml
  if grep -rn 'std::' "$crate"src; then echo "generated Rust in $crate still uses std" >&2; exit 1; fi
done
if [ -f "$ROOT/interfaces/rust-roundtrip/Cargo.toml" ]; then
  (cd "$ROOT/interfaces/rust-roundtrip" && cargo test --quiet)
fi
