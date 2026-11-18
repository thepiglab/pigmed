#!/usr/bin/env bash
# Progressive ingest -> Hugging Face streaming on one node: the Rust producer walks plan partitions while
# a publisher loop uploads, verifies (full readback), records a receipt and deletes each committed bundle.
# A small outbox (OUTBOX_BYTES) makes the producer hit backpressure (exit 75); it then waits and resumes
# from its checkpoint, so local disk never holds more than the outbox.
# Required env: PIGMED_ENV (site file), HF_REPO (existing or --create-private), PLAN_DIR, OUT_ROOT,
#               CONFIG (ingest config). Optional: SET (JSON overrides), PARTS (glob), CONC.
#SBATCH --nodes=1
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=8
#SBATCH --mem=24G
#SBATCH --time=04:00:00
set -uo pipefail
source "${PIGMED_ENV:?}"
: "${HF_REPO:?}" "${PLAN_DIR:?}" "${OUT_ROOT:?}" "${CONFIG:?}"
SET="${SET:-{\}}"
cd "$PIGMED_CODE"
mkdir -p "$OUT_ROOT"
log() { echo "$(date +%s) $*"; }

producer() {
  for part in "$PLAN_DIR"/${PARTS:-part-*.jsonl}; do
    while true; do
      "$PIGMED_RS" ingest --manifest "$part" --root "$OUT_ROOT" --config "$CONFIG" --set "$SET" \
        --concurrency "${CONC:-8}" --http-concurrency 16 > "$OUT_ROOT/last-ingest.json" 2> "$OUT_ROOT/last-ingest.err"
      rc=$?
      if [[ $rc == 0 ]]; then log "PRODUCED $part"; break; fi
      if [[ $rc == 75 ]]; then log "BACKPRESSURE $part"; sleep 15; continue; fi
      log "PRODUCER_FAILED rc=$rc $part"; cat "$OUT_ROOT/last-ingest.err"; return $rc
    done
  done
  touch "$OUT_ROOT/producer.done"
}

rm -f "$OUT_ROOT/producer.done"
producer > "$OUT_ROOT/producer.log" 2>&1 &
ppid=$!
t0=$(date +%s)
while true; do
  done_flag=0; [[ -e "$OUT_ROOT/producer.done" ]] || ! kill -0 $ppid 2>/dev/null && done_flag=1
  out=$("$UV_PROJECT_ENVIRONMENT/bin/pigmed" publish --root "$OUT_ROOT" --repo "$HF_REPO" ${CREATE:+--create-private} \
        --delete-after-verify 2>&1) || { log "PUBLISH_FAILED $out"; sleep 30; continue; }
  log "PUBLISH $out"
  [[ $done_flag == 1 ]] && ! compgen -G "$OUT_ROOT/runs/*/ready/*/manifest.json" > /dev/null && break
  sleep 10
done
wait $ppid; prc=$?
log "END producer_rc=$prc elapsed=$(( $(date +%s) - t0 ))s"
cat "$OUT_ROOT/producer.log"
