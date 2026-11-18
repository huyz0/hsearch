#!/usr/bin/env bash
# Build with the x86-64-v3 (AVX2, FMA, BMI) vector kernels. lucene-rust picks its SIMD
# kernels at compile time, so a default x86-64 build runs the scalar ones; this builds the
# fast ones for artifacts that will run on CPUs from 2013 (Haswell) on:
#   scripts/build-v3.sh                      # cargo build --release
#   scripts/build-v3.sh test -p hsearch-engine --release
# The binary refuses to start on a CPU without the features (`hsearch_engine::check_cpu`).
# An environment RUSTFLAGS you set is kept, with the v3 flag added. On aarch64 (NEON is in the
# baseline) it is a plain build. Measurements and trade-offs: docs/plan/evidence/m23-05-simd.md.
set -euo pipefail
cd "$(dirname "$0")/.."
flags="${RUSTFLAGS:-}"
case "$(uname -m)" in
  x86_64 | amd64) flags="$flags -C target-cpu=x86-64-v3" ;;
esac
if [ "$#" -eq 0 ]; then set -- build --release; fi
RUSTFLAGS="$flags" exec cargo "$@"
