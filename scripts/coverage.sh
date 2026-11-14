#!/usr/bin/env bash
# Per-crate line-coverage floor using standard cargo-llvm-cov. No custom report parsing.
# Usage: scripts/coverage.sh [floor]   (default 90)
# hsearch-s3's integration tests need a real store: eval "$(scripts/rustfs.sh up)" first; without one
# they skip and its coverage is below the floor, which is the intended failure.
# Needs: cargo install cargo-llvm-cov --locked   (llvm-tools-preview comes from rust-toolchain.toml)
set -euo pipefail
cd "$(dirname "$0")/.."
FLOOR="${1:-90}"
# Production crates; hsearch-testkit and xtask are test/tooling support and are excluded.
CRATES=(hsearch-types hsearch-observe hsearch-engine hsearch-object hsearch-s3 hsearch-index hsearch-format hsearch-node hsearch-module-abi hsearch-module-native)
fail=0
for crate in "${CRATES[@]}"; do
  echo "== $crate (floor ${FLOOR}% lines)"
  if ! cargo llvm-cov --locked --all-features --package "$crate" --summary-only --fail-under-lines "$FLOOR"; then
    echo "FAIL: $crate is below ${FLOOR}% line coverage" >&2
    fail=1
  fi
done
exit $fail
