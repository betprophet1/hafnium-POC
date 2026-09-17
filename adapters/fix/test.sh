#!/usr/bin/env bash
# The FIX adapter's integration test, from nothing: fetches the pinned JDK and Gradle wrapper if they are not there,
# builds the venue binaries it runs against, then builds the adapter and runs its tests. The test starts its own node
# and gateway on 127.0.0.1:17430-17432 and the adapter on 17440, and stops them again.
#
#   adapters/fix/test.sh             everything
#   adapters/fix/test.sh --no-cargo  skip the cargo build (the release binaries are already current)
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

"$HERE/bootstrap.sh"
eval "$("$HERE/bootstrap.sh" --env)"

if [[ "${1:-}" != "--no-cargo" ]]; then
  (cd "$ROOT/core" && cargo build -q --release -p venue-node -p venue-gateway -p venue-sim)
fi

cd "$HERE"
# --rerun: the test depends on the venue binaries too, which Gradle does not see change.
./gradlew --console=plain --warning-mode=summary test --rerun
# The test checks the audit log's chains in Java; the format's own verifier checks the same files independently.
python3 "$ROOT/interfaces/tools/audit_verify.py" --closed "$HERE"/build/it/adapter/audit/*.jsonl
