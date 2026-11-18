#!/usr/bin/env bash
# Per-op time breakdown, torch vs ONNX Runtime (the Rust runner's engine), FP32, batch 1, T threads.
#   EMBED_ENV=<site env> [THREADS=16] sbatch --export=ALL examples/slurm/embed-rs-ops.sbatch
#SBATCH --job-name=pigmed-embed-ops
#SBATCH --nodes=1
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=16
#SBATCH --mem=64G
#SBATCH --time=04:00:00
set -euo pipefail
source "${EMBED_ENV:?export EMBED_ENV=<site env>}"
T=${THREADS:-16}; OUT=${OUT:-$EMBED_ROOT/out/ops-$SLURM_JOB_ID}; ONNX=${ONNX:-$EMBED_ROOT/onnx-flash}
export OMP_NUM_THREADS=$T MKL_NUM_THREADS=$T
mkdir -p "$OUT"; lscpu > "$OUT/lscpu.txt"
echo "host=$(hostname) cpu=$(lscpu | sed -n 's/^Model name: *//p')"
echo "isa: $(grep -o -w -E 'avx2|fma|avx512f|avx512bw|avx512_vnni|avx512_bf16|avx_vnni|amx_tile|amx_bf16|amx_int8' /proc/cpuinfo | sort -u | tr '\n' ' ')" | tee "$OUT/isa.txt"
cd "$EMBED_CODE/py"
export ORT_DYLIB_PATH=$(ls "$VENV"/lib/python3.12/site-packages/onnxruntime/capi/libonnxruntime.so.* | head -1)
BIN=${BIN:-$EMBED_ROOT/bin/pigmed-embed}
p() { python profile_ops.py --threads $T --onnx "$ONNX" --inputs "$EMBED_ROOT/inputs" --out "$OUT" --rust-bin "$BIN" "$@" 2>&1 | grep -E '^\{|Error' | cut -c1-900; }
for c in "gemma-text 128" "gemma-text 512" "neomme-text 512" "gemma-image 0" "neomme-image 0" "gemma-text 2048" "gemma-text 8192" "neomme-text 4096"; do
  set -- $c; echo "== $1 len=$2 $(date +%T)"; p --case $1 --len $2 || echo "FAILED $c"
done
echo "== neomme-text 16384 $(date +%T)"; p --case neomme-text --len 16384 --reps 1 || echo FAILED
echo "done $(date -Is)"
