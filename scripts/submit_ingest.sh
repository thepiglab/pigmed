#!/usr/bin/env bash
# Submit only after reviewing account authorization, group quotas, and resource needs.
set -euo pipefail
: "${PIGMED_CODE:?Set absolute path to this repository}"
: "${PIGMED_WORK:?Set absolute persistent outbox path}"
: "${PLAN_DIR:?Set absolute immutable plan directory}"
: "${PIGMED_CONFIG:?Set absolute run JSON path}"
ACCOUNT="${ACCOUNT:-${SBATCH_ACCOUNT:?Set ACCOUNT or SBATCH_ACCOUNT (site env)}}"
QOS="${QOS:-${SBATCH_QOS:?Set QOS or SBATCH_QOS (site env)}}"
CONCURRENCY="${CONCURRENCY:-8}"
N=$(find "$PLAN_DIR" -maxdepth 1 -type f -name 'part-*.jsonl' | wc -l)
(( N > 0 )) || { echo 'No manifest partitions'; exit 1; }
mkdir -p "$PIGMED_WORK/logs"
export PIGMED_CODE PIGMED_WORK PLAN_DIR PIGMED_CONFIG
sbatch --account="$ACCOUNT" --qos="$QOS" --array="0-$((N-1))%${CONCURRENCY}" \
  --output="$PIGMED_WORK/logs/ingest-%A_%a.log" \
  --export=ALL "$PIGMED_CODE/examples/slurm/ingest.sbatch"
