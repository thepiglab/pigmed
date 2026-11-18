#!/usr/bin/env bash
# Build + unit-test pigmed-rs. Site settings (paths, caches, toolchain, Slurm account/QOS) come from
# the file named by $PIGMED_ENV, which is kept out of the repository.
#SBATCH --job-name=pigmed-rs-build
#SBATCH --nodes=1
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=16
#SBATCH --mem=32G
#SBATCH --time=00:45:00
set -euo pipefail
source "${PIGMED_ENV:?export PIGMED_ENV=/path/to/your/site/env.sh before sbatch}"
cd "$PIGMED_CODE/rust/pigmed-ingest"
rc=0; cargo build --release --message-format short > "${TMPDIR:-/tmp}/build.log" 2>&1 || rc=$?
grep -vE '^\s+(Compiling|Downloaded|Downloading|Locking|Adding|Updating)|^(cargo:|[A-Z_]+ = None)' "${TMPDIR:-/tmp}/build.log" | tail -150
[[ $rc == 0 ]] || { echo BUILD_FAILED; exit 1; }
cargo test --release 2>&1 | grep -vE '^\s+(Compiling|Running)' | tail -40
echo BUILD_OK
