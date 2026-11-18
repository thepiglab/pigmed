#!/usr/bin/env bash
# Array task: one plan partition -> bundles, with a frozen pigmed-rs binary + config snapshot.
# Required env: PIGMED_ENV (site file), SNAP (snapshot dir with bin/pigmed-rs, lib/libpdfium.so,
# config.json), PLAN_DIR, OUT_ROOT. Submit with scripts/submit_rs_ingest.sh.
#SBATCH --nodes=1
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=8
#SBATCH --mem=16G
#SBATCH --time=04:00:00
#SBATCH --signal=TERM@120
set -euo pipefail
: "${SNAP:?}" "${PLAN_DIR:?}" "${OUT_ROOT:?}"
source "${PIGMED_ENV:?}"
part=$(printf '%s/part-%05d.jsonl' "$PLAN_DIR" "$SLURM_ARRAY_TASK_ID")
[[ -f "$part" ]] || { echo "no partition $part (plan smaller than array); nothing to do"; exit 0; }
export PIGMED_PDFIUM="$SNAP/lib/libpdfium.so"
echo "host=$(hostname) part=$part start=$(date -Is)"
/usr/bin/time -f "wall=%e s rss=%M KB cpu=%P" "$SNAP/bin/pigmed-rs" ingest --manifest "$part" --root "$OUT_ROOT" \
  --config "$SNAP/config.json" --concurrency "${CONC:-8}" --http-concurrency "${HTTP_CONC:-16}"
