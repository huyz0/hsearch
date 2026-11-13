#!/usr/bin/env bash
# One command: build the node, start the pinned RustFS if none is configured, run the
# end to end script over plain HTTP, and stop RustFS if this script started it.
#   scripts/e2e.sh            N=20000 scripts/e2e.sh   for a quicker run
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --locked -q -p hsearch-node
started=0
if [ -z "${HSEARCH_S3_ENDPOINT:-}" ]; then
  eval "$(scripts/rustfs.sh up)"
  started=1
fi
trap '[ "$started" = 1 ] && scripts/rustfs.sh down || true' EXIT
python3 -I scripts/e2e.py target/debug/hsearch-node
