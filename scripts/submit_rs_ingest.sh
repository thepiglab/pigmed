#!/usr/bin/env bash
# Usage: PIGMED_ENV=/path/site/env.sh scripts/submit_rs_ingest.sh NAME PLAN_DIR CONFIG_JSON [ARRAY_MAX] [CONCURRENT]
# Freezes the current pigmed-rs binary, libpdfium and config into $PIGMED_WORK/snapshots/NAME, then
# submits one array task per plan partition. Slurm account/QOS come from the site env
# (SBATCH_ACCOUNT / SBATCH_QOS), never from this repository.
# Retries: OUT_ROOT=<existing runs dir> ARRAY=5,6 resume those partitions from their checkpoints.
set -euo pipefail
source "${PIGMED_ENV:?export PIGMED_ENV=/path/to/your/site/env.sh}"
NAME=$1 PLAN_DIR=$2 CONFIG=$3 MAX=${4:-} CONCURRENT=${5:-8}
SNAP="$PIGMED_WORK/snapshots/$NAME"
[[ -e "$SNAP" ]] && { echo "snapshot $SNAP exists; pick a new NAME"; exit 1; }
mkdir -p "$SNAP/bin" "$SNAP/lib"
cp "$PIGMED_RS" "$SNAP/bin/pigmed-rs"
cp "${PIGMED_PDFIUM_SRC:?path to libpdfium.so}" "$SNAP/lib/"
cp "$CONFIG" "$SNAP/config.json"
sha256sum "$SNAP/bin/pigmed-rs" "$SNAP/config.json" > "$SNAP/SHA256SUMS"
if [[ -z "$MAX" ]]; then
  N=$(find "$PLAN_DIR" -maxdepth 1 -name 'part-*.jsonl' | wc -l); MAX=$((N-1))
fi
OUT_ROOT="${OUT_ROOT:-$PIGMED_WORK/runs-$NAME}"
ARRAY="${ARRAY:-0-${MAX}}"
mkdir -p "$OUT_ROOT" "$PIGMED_WORK/logs"
SB=(${SBATCH_CMD:-sbatch} --parsable)  # SBATCH_CMD lets a site wrap sbatch (e.g. an account guard)
"${SB[@]}" --job-name="pigmed-ingest-$NAME" --array="${ARRAY}%${CONCURRENT}" \
  --output="$PIGMED_WORK/logs/ingest-$NAME-%A_%a.log" \
  --export=ALL,SNAP="$SNAP",PLAN_DIR="$PLAN_DIR",OUT_ROOT="$OUT_ROOT" \
  ${DEPEND:+--dependency=$DEPEND} \
  "$PIGMED_CODE/examples/slurm/rs-ingest.sbatch"
