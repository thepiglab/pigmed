#!/usr/bin/env bash
#SBATCH --job-name=pigmed-ingest
#SBATCH --nodes=1
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=8
#SBATCH --mem=32G
#SBATCH --time=04:00:00
#SBATCH --signal=TERM@120
set -euo pipefail
: "${PIGMED_CODE:?}" "${PIGMED_WORK:?}" "${PLAN_DIR:?}" "${PIGMED_CONFIG:?}"
# Node-local scratch: most sites set TMPDIR per job.
export TMPDIR="${TMPDIR:-/tmp}"
export HF_HOME="${HF_HOME:-$PIGMED_WORK/cache/huggingface}"
export UV_CACHE_DIR="${UV_CACHE_DIR:-$PIGMED_WORK/cache/uv}"
export OMP_NUM_THREADS="${SLURM_CPUS_PER_TASK:-8}"
export MKL_NUM_THREADS="$OMP_NUM_THREADS" OPENBLAS_NUM_THREADS="$OMP_NUM_THREADS"
export TOKENIZERS_PARALLELISM=false
cd "$PIGMED_CODE"
manifest=$(printf '%s/part-%05d.jsonl' "$PLAN_DIR" "$SLURM_ARRAY_TASK_ID")
[[ -f "$manifest" ]] || { echo "Missing $manifest"; exit 1; }
# Environment and model snapshots must already be installed/cached. No job-time pip installs.
# srun keeps Slurm step accounting and termination behavior explicit.
srun uv run --no-sync pigmed ingest --manifest "$manifest" --root "$PIGMED_WORK" --config "$PIGMED_CONFIG"
