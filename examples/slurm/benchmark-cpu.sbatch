#!/usr/bin/env bash
#SBATCH --job-name=pigmed-cpu-bench
#SBATCH --nodes=1
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=8
#SBATCH --mem=32G
#SBATCH --time=01:00:00
set -euo pipefail
: "${PIGMED_CODE:?}" "${PIGMED_WORK:?}" "${MODEL_ID:?}" "${MODEL_SHA:?}" "${BACKEND:?}" "${BENCH_INPUTS:?}"
export HF_HOME="${HF_HOME:-$PIGMED_WORK/cache/huggingface}"
export OMP_NUM_THREADS="$SLURM_CPUS_PER_TASK" MKL_NUM_THREADS="$SLURM_CPUS_PER_TASK" OPENBLAS_NUM_THREADS="$SLURM_CPUS_PER_TASK"
export TOKENIZERS_PARALLELISM=false HF_HUB_OFFLINE=1 TRANSFORMERS_OFFLINE=1
cd "$PIGMED_CODE"
srun uv run --no-sync pigmed benchmark --backend "$BACKEND" --model-id "$MODEL_ID" --revision "$MODEL_SHA" \
  --device cpu --inputs "$BENCH_INPUTS" --batch-size "${BATCH_SIZE:-1}" \
  --out "$PIGMED_WORK/benchmark-${BACKEND}-cpu-${SLURM_JOB_ID}.json"
