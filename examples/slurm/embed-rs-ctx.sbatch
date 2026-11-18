#!/usr/bin/env bash
# Context-length sweep: Python (card runtime) vs Rust (ort) per input length, plus a mixed-length batch, an
# over-max item (truncation must stop exactly at the model max) and short queries. Vectors saved for parity.
#   EMBED_ENV=<site env> MODEL=gemma|neomme [THREADS=16] [RUNTIMES="py rs"] [PIGMED_ORT_NO_ARENA=1] [EMBED_CTX_TARS="<bundle assets.tar> ..."] \
#     sbatch --export=ALL examples/slurm/embed-rs-ctx.sbatch
# EMBED_CTX_TARS (bundle assets.tar files with */article.json) is only needed the first time, to build the inputs.
#SBATCH --job-name=pigmed-embed-ctx
#SBATCH --nodes=1
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=16
#SBATCH --mem=64G
#SBATCH --time=08:00:00
set -euo pipefail
source "${EMBED_ENV:?export EMBED_ENV=<site env that sets EMBED_ROOT and sources py/env.sh>}"
: "${MODEL:?}"
T=${THREADS:-16}; OUT=${OUT:-$EMBED_ROOT/out/ctx-$MODEL-$SLURM_JOB_ID}; ONNX=${ONNX:-$EMBED_ROOT/onnx-flash}
IN=$EMBED_ROOT/inputs; BIN=${BIN:-$EMBED_ROOT/bin/pigmed-embed}
export ORT_DYLIB_PATH=$(ls "$VENV"/lib/python3.12/site-packages/onnxruntime/capi/libonnxruntime.so.* | head -1)
export OMP_NUM_THREADS=$T MKL_NUM_THREADS=$T
GEMMA=$(python -c "import json;print(json.load(open('$EMBED_ROOT/models.json'))['google/embeddinggemma-2']['path'])")
NEO=$(python -c "import json;print(json.load(open('$EMBED_ROOT/models.json'))['Hcompany/NeoMME-260M-Retriever']['path'])")
SNAP=$GEMMA; MAXLEN=8192; [[ $MODEL == neomme ]] && { SNAP=$NEO; MAXLEN=16384; }
mkdir -p "$OUT"; lscpu > "$OUT/lscpu.txt"
echo "host=$(hostname) cpu=$(lscpu | sed -n 's/^Model name: *//p') flags=$(grep -o -w -E 'avx2|avx512f|avx512_vnni|avx_vnni|amx_tile' /proc/cpuinfo | sort -u | tr '\n' ' ') start=$(date -Is)"
cd "$EMBED_CODE/py"
if [[ ! -f $IN/ctx-$MODEL.jsonl ]]; then
  python make_long_inputs.py --tars ${EMBED_CTX_TARS:?set EMBED_CTX_TARS to build inputs} --gemma "$GEMMA" --neomme "$NEO" --out "$IN"
fi

REPS=${REPS:-3}
run() {  # run <rt> <texts-file> <batch> <n> <suffix> [extra args]; uses $REP
  local rt=$1 f=$2 b=$3 n=$4 sfx=$5; shift 5
  local o=$OUT/rep$REP sv=""; [[ $REP == 0 ]] && sv=--save
  mkdir -p "$o"
  local common="--model $MODEL --modality text --threads $T --batch $b --n $n --max-len $MAXLEN --inputs $IN --out $o $sv --texts-file $f --tag-suffix $sfx $*"
  local log=$o/$rt$sfx-b$b.log
  case $rt in
    py) /usr/bin/time -v python py_bench.py $common > "$log" 2>&1 ;;
    rs) /usr/bin/time -v "$BIN" bench $common --onnx "$ONNX" --snapshot "$SNAP" > "$log" 2>&1 ;;
  esac || echo "FAILED rt=$rt file=$f b=$b (exit $?)"
  grep -E "Maximum resident|Exit status|Command terminated" "$log" | sed "s/^/  $rt$sfx b$b: /" || true
  grep -h '"items_per_s"' "$log" | grep -o '"items_per_s":[ ]*[0-9.]*' | sed "s/^/  $rt$sfx b$b: /" || true
}

for REP in $(seq 0 $((REPS - 1))); do
rts=(${RUNTIMES:-py rs}); (( REP % 2 )) && rts=($(printf '%s\n' "${rts[@]}" | tac))  # alternate order per rep
echo "######## rep $REP order ${rts[*]} $(uptime | sed 's/.*load/load/')"
# LENGTHS="L128 L2048 over" restricts to those inputs; EXTRAS=0 skips the mixed batch and queries
files=$(ls $IN/ctx-$MODEL-L*.jsonl | sort -t L -k2 -n; echo $IN/ctx-$MODEL-over.jsonl)
if [[ -n "${LENGTHS:-}" ]]; then files=$(for l in $LENGTHS; do echo $IN/ctx-$MODEL-$l.jsonl; done); fi
[[ "${LENGTHS:-}" == none ]] && files=""
for f in $files; do
  name=$(basename "$f" .jsonl); sfx=_${name#ctx-$MODEL-}
  n=$(wc -l < "$f")
  echo "== $name ($n items) $(date +%T)"
  for rt in "${rts[@]}"; do run $rt "$(basename "$f")" 1 "$n" "$sfx"; done
done
if [[ "${EXTRAS:-1}" == 1 ]]; then
echo "== mixed-length batch $(date +%T)"
for rt in "${rts[@]}"; do for b in 1 4; do run $rt mixed-$MODEL.jsonl $b 8 _mixed; done; done
echo "== queries $(date +%T)"
side=""; [[ $MODEL == neomme ]] && side="--side query"
for rt in "${rts[@]}"; do run $rt queries-$MODEL.jsonl 1 9 _query $side; done
fi
done
python ctx_report.py "$OUT" | tee "$OUT/ctx_table.md" || true
echo "done $(date -Is)"
