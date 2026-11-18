#!/usr/bin/env bash
# Ad-hoc experiment in a job: EMBED_ENV=<site env> CMD='...' sbatch --export=ALL [--constraint=turin] examples/slurm/embed-rs-run.sbatch
#SBATCH --job-name=pigmed-embed-run
#SBATCH --nodes=1
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=32
#SBATCH --mem=64G
#SBATCH --time=02:00:00
set -euo pipefail
source "${EMBED_ENV:?export EMBED_ENV=<site env>}"
: "${CMD:?}"
echo "host=$(hostname) cpu=$(lscpu | sed -n 's/^Model name: *//p') isa=$(grep -o -w -E 'avx2|avx512f|avx512_bf16|amx_tile' /proc/cpuinfo | sort -u | tr '\n' ' ')"
export ORT_DYLIB_PATH=$(ls "$VENV"/lib/python3.12/site-packages/onnxruntime/capi/libonnxruntime.so.* | head -1)
cd "$EMBED_CODE/py"
bash -c "$CMD"
