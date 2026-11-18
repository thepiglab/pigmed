#!/usr/bin/env bash
# Holds a 16-CPU burst allocation for build / export / parity work, driven from the login node with
#   srun --jobid=<id> --overlap -n 1 -c 16 bash -c 'source $EMBED_ENV; ...'
# Release it with scancel when done. Never used for timing (benchmarks run in embed-rs-bench.sbatch).
#SBATCH --job-name=pigmed-embed-dev
#SBATCH --nodes=1
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=16
#SBATCH --mem=64G
#SBATCH --time=08:00:00
set -euo pipefail
source "${EMBED_ENV:?export EMBED_ENV=<site env>}"
echo "host=$(hostname) cpu=$(lscpu | sed -n 's/^Model name: *//p') start=$(date -Is)"
sleep 8h
