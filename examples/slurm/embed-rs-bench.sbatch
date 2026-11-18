#!/usr/bin/env bash
# Python (torch, model-card runtime) vs Rust (ort) vs Python+ORT control, interleaved per config so all
# runtimes see the same node. One process per (runtime, threads, batch): peak RSS is per config.
#   MODEL=gemma|neomme MODALITY=text|image [THREADS="4 8 16"] [BATCHES="1 2 4"] [RUNTIMES="py rs" (pyort only works with non-custom-op graphs)]
#   [N=32] [REPS=3] [SAVE=1 -> vectors from rep 0] [ONNX=$EMBED_ROOT/onnx] [OUT=$EMBED_ROOT/out/bench-<job>]
#   sbatch --export=ALL,MODEL=gemma,MODALITY=text examples/slurm/embed-rs-bench.sbatch
#SBATCH --job-name=pigmed-embed-bench
#SBATCH --nodes=1
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=16
#SBATCH --mem=64G
#SBATCH --time=06:00:00
set -euo pipefail
source "${EMBED_ENV:?export EMBED_ENV=<site env that sets EMBED_ROOT and sources py/env.sh>}"
: "${MODEL:?}" "${MODALITY:?}"
THREADS=${THREADS:-"4 8 16"}; BATCHES=${BATCHES:-"1 2 4"}; RUNTIMES=${RUNTIMES:-"py rs"}
N=${N:-32}; OUT=${OUT:-$EMBED_ROOT/out/bench-$SLURM_JOB_ID}
ONNX=${ONNX:-$EMBED_ROOT/onnx-flash}; INPUTS=$EMBED_ROOT/inputs
export ORT_DYLIB_PATH=$(ls "$VENV"/lib/python3.12/site-packages/onnxruntime/capi/libonnxruntime.so.* | head -1)
SNAP=$(python -c "import json;d=json.load(open('$EMBED_ROOT/models.json'));print(d['google/embeddinggemma-2' if '$MODEL'=='gemma' else 'Hcompany/NeoMME-260M-Retriever']['path'])")
BIN=${BIN:-$EMBED_ROOT/bin/pigmed-embed}   # frozen copy of target/release/pigmed-embed
mkdir -p "$OUT"
echo "host=$(hostname) cpu=$(lscpu | sed -n 's/^Model name: *//p') flags_avx512=$(grep -c avx512f /proc/cpuinfo || true) start=$(date -Is)"
echo "model=$MODEL modality=$MODALITY threads=[$THREADS] batches=[$BATCHES] runtimes=[$RUNTIMES] n=$N out=$OUT"
lscpu > "$OUT/lscpu.txt"
SAVE_FLAG=""; [[ "${SAVE:-0}" == 1 ]] && SAVE_FLAG="--save"
cd "$EMBED_CODE/py"
REPS=${REPS:-3}
uptime | sed 's/^/load: /'
for rep in $(seq 0 $((REPS - 1))); do
  # rotate runtime order each rep so no runtime always runs first/last on the shared node
  rts=($RUNTIMES); k=$((rep % ${#rts[@]})); order=("${rts[@]:$k}" "${rts[@]:0:$k}")
  for t in $THREADS; do
    for b in $BATCHES; do
      for rt in "${order[@]}"; do
        export OMP_NUM_THREADS=$t MKL_NUM_THREADS=$t
        o=$OUT/rep$rep; sf=""; [[ $rep == 0 ]] && sf=$SAVE_FLAG
        common="--model $MODEL --modality $MODALITY --threads $t --batch $b --n $N --inputs $INPUTS --out $o $sf"
        case $rt in
          py)    python py_bench.py $common ;;
          pyort) python ort_bench.py $common --onnx "$ONNX" ;;
          rs)    "$BIN" bench $common --onnx "$ONNX" --snapshot "$SNAP" ;;
        esac 2>&1 | grep -E '^\{|Error|error|Traceback' | sed 's/"batch_latency_s": \[[^]]*\], //' | cut -c1-330 || echo "FAILED rt=$rt t=$t b=$b rep=$rep"
      done
    done
  done
  uptime | sed "s/^/load after rep $rep: /"
done
python "$EMBED_CODE/py/compare.py" --out "$OUT" --md "$OUT/table.md" || true
echo "done $(date -Is)"
