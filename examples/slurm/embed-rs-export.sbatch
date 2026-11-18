#!/usr/bin/env bash
# Export ONNX graphs (FP32) with wrapper-vs-HF and ONNX-vs-wrapper checks.
#   EMBED_ENV=<site env> WHICH="gemma_lm ..." [ATTN=eager|pigmed] [OUTDIR=$EMBED_ROOT/onnx-eager-fn] sbatch --export=ALL examples/slurm/embed-rs-export.sbatch
#SBATCH --job-name=pigmed-embed-export
#SBATCH --nodes=1
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=16
#SBATCH --mem=64G
#SBATCH --time=02:00:00
set -euo pipefail
source "${EMBED_ENV:?export EMBED_ENV=<site env>}"
: "${WHICH:?}"
cd "$EMBED_CODE/py"
python export_onnx.py --out "${OUTDIR:-$EMBED_ROOT/onnx-eager-fn}" --inputs "$EMBED_ROOT/inputs" --threads 16 \
  --attn ${ATTN:-eager} --fuse-norm ${LEGACY:+--legacy-export} --which $WHICH 2>&1 | grep -v -i "warn" | grep -v "Loading weights" | cut -c1-400
echo "done $(date -Is)"
