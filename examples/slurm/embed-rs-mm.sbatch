#!/usr/bin/env bash
# Modality / file-type / size sweep, Python (card runtime) vs Rust (ort), batch 1, vectors saved for parity.
#   EMBED_ENV=<site env> MODEL=gemma|neomme [MM_GROUPS="formats res budget audio video"] sbatch --export=ALL examples/slurm/embed-rs-mm.sbatch
# Needs inputs from py/make_media_inputs.py and, for Gemma audio/video/budgets, gemma_audio.onnx + gemma_vision_dyn.onnx.
#SBATCH --job-name=pigmed-embed-mm
#SBATCH --nodes=1
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=16
#SBATCH --mem=64G
#SBATCH --time=06:00:00
set -euo pipefail
source "${EMBED_ENV:?export EMBED_ENV=<site env>}"
: "${MODEL:?}"
T=${THREADS:-16}; OUT=${OUT:-$EMBED_ROOT/out/mm-$MODEL-$SLURM_JOB_ID}; ONNX=${ONNX:-$EMBED_ROOT/onnx-flash}
IN=$EMBED_ROOT/inputs; BIN=${BIN:-$EMBED_ROOT/bin/pigmed-embed}
MM_GROUPS=${MM_GROUPS:-"formats res budget audio video"}  # not GROUPS: a bash builtin
export ORT_DYLIB_PATH=$(ls "$VENV"/lib/python3.12/site-packages/onnxruntime/capi/libonnxruntime.so.* | head -1)
export OMP_NUM_THREADS=$T MKL_NUM_THREADS=$T
SNAP=$(python -c "import json;d=json.load(open('$EMBED_ROOT/models.json'));print(d['google/embeddinggemma-2' if '$MODEL'=='gemma' else 'Hcompany/NeoMME-260M-Retriever']['path'])")
mkdir -p "$OUT/lists"; lscpu > "$OUT/lscpu.txt"
echo "host=$(hostname) cpu=$(lscpu | sed -n 's/^Model name: *//p') ffmpeg=$(ffmpeg -version 2>/dev/null | head -1 | cut -d' ' -f3) start=$(date -Is)"
cd "$EMBED_CODE/py"
# split the input manifests into one list per (kind, format) group
python - "$IN" "$OUT/lists" <<'PY'
import json, sys, collections
from pathlib import Path
inp, out = Path(sys.argv[1]), Path(sys.argv[2])
groups = collections.defaultdict(list)
for f in ("images-formats.jsonl", "images-native.jsonl", "media.jsonl"):
    if not (inp / f).exists():
        continue
    for l in open(inp / f):
        r = json.loads(l)
        key = f"{r['kind']}_{r['format'].replace('-', '')}"
        if r["kind"] != "image":
            key += f"_{int(r['seconds'])}s" if r["format"] in ("mp4", "wav-16k") else ""
        groups[key].append(r)
for k, rows in groups.items():
    (out / f"{k}.jsonl").write_text("".join(json.dumps(r) + "\n" for r in rows))
print(" ".join(sorted(groups)))
PY

run() {  # run <rt> <list relative to IN> <modality> <suffix> [extra]
  local rt=$1 f=$2 mod=$3 sfx=$4; shift 4
  local n=$(wc -l < "$IN/$f")
  local sv=""; [[ $REP == 0 ]] && sv=--save
  local common="--model $MODEL --modality $mod --threads $T --batch 1 --n $n --inputs $IN --out $OUTR $sv --images-file $f --tag-suffix $sfx $*"
  local log=$OUTR/$rt$sfx.log
  case $rt in
    py) /usr/bin/time -v python py_bench.py $common > "$log" 2>&1 ;;
    rs) /usr/bin/time -v "$BIN" bench $common --onnx "$ONNX" --snapshot "$SNAP" > "$log" 2>&1 ;;
  esac || echo "FAILED rt=$rt $sfx (see $log)"
  grep -h -o '"items_per_s":[ ]*[0-9.]*' "$log" | sed "s/^/  $rt$sfx: /" || true
  grep -h -E "Error|error:" "$log" | tail -2 | cut -c1-300 | sed "s/^/  $rt$sfx: /" || true
}
REL=$(realpath --relative-to="$IN" "$OUT/lists")
REPS=${REPS:-3}
for REP in $(seq 0 $((REPS - 1))); do
OUTR=$OUT/rep$REP; mkdir -p $OUTR
echo "######## rep $REP"
for g in $MM_GROUPS; do
  case $g in
    formats) for f in $OUT/lists/image_*.jsonl; do k=$(basename $f .jsonl); echo "== $k"; for rt in $( (( REP % 2 )) && echo rs py || echo py rs ); do run $rt $REL/$k.jsonl image _${k#image_} --decode-in-loop; done; done ;;
    res) for R in 512 1024 1536 2048; do echo "== res $R"; for rt in $( (( REP % 2 )) && echo rs py || echo py rs ); do run $rt images-res-$R.jsonl image _res$R; done; done ;;
    budget) [[ $MODEL == gemma ]] && for B in 70 140 560 1120; do echo "== budget $B"; for rt in $( (( REP % 2 )) && echo rs py || echo py rs ); do run $rt images.jsonl image _tok$B --max-soft-tokens $B; done; done ;;
    audio) [[ $MODEL == gemma ]] && for f in $OUT/lists/audio_*.jsonl; do k=$(basename $f .jsonl); echo "== $k"; for rt in $( (( REP % 2 )) && echo rs py || echo py rs ); do run $rt $REL/$k.jsonl audio _${k#audio_}; done; done ;;
    video) [[ $MODEL == gemma ]] && for f in $OUT/lists/video_*.jsonl; do k=$(basename $f .jsonl); echo "== $k"; for rt in $( (( REP % 2 )) && echo rs py || echo py rs ); do run $rt $REL/$k.jsonl video _${k#video_}; done; done ;;
  esac
done
done
python compare.py --out "$OUT" | grep -E "^(rs|py)" || true
echo "done $(date -Is)"
