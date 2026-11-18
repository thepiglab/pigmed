#!/bin/bash
# Parity of querygen/v3.1d.toml with rust/pigmed-synth v3.1d (layout c3pf):
#   (a) persona library byte-identical;  (b) stage A/B requests and every output field identical on SAMPLE/ARTICLES,
#   both generators talking to the same deterministic mock server. pigmed-synth is built into a temp target dir
#   (its source is not touched). Usage: querygen/parity.sh SAMPLE ARTICLES [LIMIT=150]
set -euo pipefail
R=$(cd "$(dirname "$0")/.." && pwd); S=${1:?sample.jsonl}; A=${2:?articles.jsonl}; N=${3:-150}
W=$(mktemp -d); trap 'pkill -f "mock-serve --port 1818[12]" 2>/dev/null; rm -rf $W' EXIT
(cd $R/rust/pigmed-synth && CARGO_TARGET_DIR=$W/st cargo build --release --locked -q)
(cd $R/rust/pigmed-querygen && cargo build --release -q)
SY=$W/st/release/pigmed-synth; QG=$R/rust/pigmed-querygen/target/release/pigmed-querygen
$SY personas --out $W/p1.jsonl >/dev/null; $QG personas --config $R/querygen/v3.1d.toml --out $W/p2.jsonl >/dev/null
cmp $W/p1.jsonl $W/p2.jsonl && echo "(a) personas: byte-identical ($(wc -l < $W/p1.jsonl) cards)"
run() {  # port record cmd...
  local port=$1 rec=$2; shift 2
  $QG mock-serve --port $port --record $rec >/dev/null 2>&1 & local mp=$!; sleep 1
  "$@" >/dev/null 2>$W/err.$port || { cat $W/err.$port; kill $mp; exit 1; }
  kill $mp
}
run 18181 $W/r1.jsonl $SY generate --sample $S --articles $A --layout c3pf --endpoint http://127.0.0.1:18181/v1 --model mock --arm par \
  --concurrency 1 --temperature 1.0 --top-p 0.95 --max-tokens 400 --gpus 1 --limit $N --out $W/g1
run 18182 $W/r2.jsonl $QG generate --config $R/querygen/v3.1d.toml --sample $S --articles $A --endpoint http://127.0.0.1:18182/v1 --model mock \
  --arm par --concurrency 1 --temperature 1.0 --top-p 0.95 --max-tokens 400 --gpus 1 --limit $N --text-only --out $W/g2
$QG compare --req-a $W/r1.jsonl --req-b $W/r2.jsonl --gen-a $W/g1/generations.jsonl --gen-b $W/g2/par-c3pf/generations.jsonl
echo "(b) prompts, schemas, noise, rejects, card drops: identical"
