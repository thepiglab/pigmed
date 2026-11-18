#!/usr/bin/env bash
#SBATCH --job-name=pigmed-publish
#SBATCH --nodes=1
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=2
#SBATCH --mem=4G
#SBATCH --time=04:00:00
#SBATCH --signal=TERM@120
set -euo pipefail
: "${PIGMED_CODE:?}" "${PIGMED_WORK:?}" "${HF_REPO:?Set existing private dataset repository}"
export HF_HOME="${HF_HOME:-$PIGMED_WORK/cache/huggingface}"
export UV_CACHE_DIR="${UV_CACHE_DIR:-$PIGMED_WORK/cache/uv}"
cd "$PIGMED_CODE"
# Requires prior HF authentication and storage approval. Does not create or publish a public repo.
# Full remote readback consumes bandwidth but protects the only-copy deletion step.
srun uv run --no-sync pigmed publish --root "$PIGMED_WORK" --repo "$HF_REPO" \
  --delete-after-verify --watch-seconds 13800 --poll-seconds 60
