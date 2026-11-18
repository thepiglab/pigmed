#!/usr/bin/env bash
# Build the Rust runner and install it atomically as $EMBED_ROOT/bin/pigmed-embed-$TAG (running jobs keep theirs).
#   EMBED_ENV=<site env> TAG=v2 [FEATURES=candle] sbatch --export=ALL examples/slurm/embed-rs-build.sbatch
#SBATCH --job-name=pigmed-embed-build
#SBATCH --nodes=1
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=8
#SBATCH --mem=16G
#SBATCH --time=01:00:00
set -euo pipefail
source "${EMBED_ENV:?export EMBED_ENV=<site env>}"
: "${TAG:?}"
cd "$EMBED_CODE"
# link on node-local disk: lld mmaps its output, which hit SIGBUS on the shared filesystem once
export CARGO_TARGET_DIR="${TMPDIR:-/tmp}/pigmed-embed-target-$SLURM_JOB_ID"
cargo build --release -j 8 ${FEATURES:+--features $FEATURES}
mkdir -p "$EMBED_ROOT/bin"
for b in pigmed-embed flash-check linear-bench ${FEATURES:+candle-neomme gemm-bench}; do
  cp "$CARGO_TARGET_DIR/release/$b" "$EMBED_ROOT/bin/.$b-$TAG.tmp" && mv -f "$EMBED_ROOT/bin/.$b-$TAG.tmp" "$EMBED_ROOT/bin/$b-$TAG"
done
ls -la "$EMBED_ROOT/bin"
rm -rf "${TMPDIR:-/tmp}/pigmed-embed-target-$SLURM_JOB_ID"
