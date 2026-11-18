#!/usr/bin/env bash
# One-time setup for the Rust embedding runner: torch-CPU venv, rustup toolchain, model snapshots, inputs.
# Submit from $EMBED_CODE with your site's account/QOS/--output (e.g. SBATCH_ACCOUNT/SBATCH_QOS in the site env):
#   EMBED_ENV=<site env> EMBED_BUNDLES="<bundle dir> ..." sbatch --export=ALL examples/slurm/embed-rs-setup.sbatch
# EMBED_BUNDLES: PigMed bundle dirs (assets.tar + items.jsonl.gz) whose page renders become image inputs.
#SBATCH --job-name=pigmed-embed-setup
#SBATCH --nodes=1
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=8
#SBATCH --mem=32G
#SBATCH --time=02:00:00
set -euo pipefail
source "${EMBED_ENV:?export EMBED_ENV=<site env that sets EMBED_ROOT and sources py/env.sh>}"
cd "$EMBED_ROOT"
echo "host=$(hostname) cpu=$(lscpu | sed -n 's/^Model name: *//p') start=$(date -Is)"
mkdir -p hf cache cargo rustup target inputs ref-src out logs
chmod g+s hf cache cargo rustup target inputs ref-src out logs

# --- Python baseline env (CPU torch from the PyTorch CPU index; never CUDA wheels) ---
if [[ ! -x "$VENV/bin/python" ]]; then
  uv venv -q --python 3.12 "$VENV"
fi
uv pip install -q --python "$VENV/bin/python" --index-url https://download.pytorch.org/whl/cpu \
  "torch==2.15.*" torchvision
uv pip install -q --python "$VENV/bin/python" \
  "transformers==5.22.2" "sentence-transformers==6.3.0" accelerate pillow numpy psutil \
  onnx onnxruntime onnxscript huggingface_hub
"$VENV/bin/python" - <<'EOF'
import torch, transformers, sentence_transformers, onnxruntime, onnx
print("torch", torch.__version__, "mkldnn", torch.backends.mkldnn.is_available(), "threads", torch.get_num_threads())
print(torch.__config__.show())
print("transformers", transformers.__version__, "st", sentence_transformers.__version__,
      "ort", onnxruntime.__version__, "onnx", onnx.__version__)
EOF

# --- Rust toolchain under $RUSTUP_HOME/$CARGO_HOME ---
if [[ ! -x "$CARGO_HOME/bin/cargo" ]]; then
  curl -sSf https://sh.rustup.rs | sh -s -- -y -q --no-modify-path --profile minimal --default-toolchain stable
fi
cargo --version; rustc --version

# --- Model snapshots (both public / ungated; no token needed) ---
"$VENV/bin/python" - <<'EOF'
import json, os
from huggingface_hub import snapshot_download, HfApi
api = HfApi(token=False)
info = {}
for mid in ["google/embeddinggemma-2", "Hcompany/NeoMME-260M-Retriever"]:
    sha = api.model_info(mid).sha
    p = snapshot_download(mid, revision=sha, token=False)
    info[mid] = {"sha": sha, "path": p}
    print(mid, sha, p, sorted(os.listdir(p)))
json.dump(info, open(os.path.join(os.environ["EMBED_ROOT"], "models.json"), "w"), indent=1)
EOF

# --- Reference source of the exact transformers modules we port/export ---
TF=$("$VENV/bin/python" -c 'import transformers,os;print(os.path.dirname(transformers.__file__))')
ST=$("$VENV/bin/python" -c 'import sentence_transformers,os;print(os.path.dirname(sentence_transformers.__file__))')
ls "$TF/models" | grep -iE 'gemma|neomme' || true
for d in embedding_gemma2 gemma4 neomme; do [[ -d "$TF/models/$d" ]] && rsync -rlt "$TF/models/$d" ref-src/; done
rsync -rlt "$ST/base/modules" ref-src/st-modules/ 2>/dev/null || true
rsync -rlt "$TF/image_processing_utils_fast.py" "$TF/image_transforms.py" ref-src/ 2>/dev/null || true

# --- Benchmark inputs ---
GEMMA=$("$VENV/bin/python" -c 'import json,os;print(json.load(open(os.environ["EMBED_ROOT"]+"/models.json"))["google/embeddinggemma-2"]["path"])')
"$VENV/bin/python" "$EMBED_CODE/py/make_inputs.py" --out inputs --gemma "$GEMMA" \
  --bundles ${EMBED_BUNDLES:?set EMBED_BUNDLES}

du -sh "$EMBED_ROOT"/* | sort -h | tail
echo "done $(date -Is)"
