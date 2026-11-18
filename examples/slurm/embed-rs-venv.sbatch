#!/usr/bin/env bash
# (Re)build the Python baseline venv on a uv-managed CPython stored under $EMBED_ROOT (pinned versions).
#   EMBED_ENV=<site env> EMBED_VENV=$EMBED_ROOT/venv312 sbatch --export=ALL examples/slurm/embed-rs-venv.sbatch
#SBATCH --job-name=pigmed-embed-venv
#SBATCH --nodes=1
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=4
#SBATCH --mem=16G
#SBATCH --time=01:00:00
set -euo pipefail
source "${EMBED_ENV:?export EMBED_ENV=<site env>}"
[[ -x "$VENV/bin/python" ]] || uv venv -q --python 3.12.14 "$VENV"   # UV_PYTHON_PREFERENCE=only-managed (py/env.sh)
uv pip install -q --python "$VENV/bin/python" --index-url https://download.pytorch.org/whl/cpu "torch==2.15.0" torchvision
uv pip install -q --python "$VENV/bin/python" "transformers==5.22.2" "sentence-transformers==6.3.0" accelerate pillow \
  numpy psutil onnx "onnxruntime==1.31.1" onnxscript huggingface_hub av librosa soundfile
# torchcodec (HF's default video/audio decoder): the CPU build, linked against the FFmpeg shared libraries that the
# PyAV wheel bundles (the PyPI build is CUDA-only; cluster ffmpeg modules may ship static libs only).
uv pip install -q --python "$VENV/bin/python" --index-url https://download.pytorch.org/whl/cpu torchcodec
L="$VENV/lib/python3.12/site-packages/av.libs"; D="$EMBED_ROOT/ffmpeg-shared"; mkdir -p "$D"
find "$D" -maxdepth 1 -type l -name 'lib*.so.*' -delete
for f in "$L"/lib{avutil,avcodec,avformat,avdevice,avfilter,swscale,swresample}-*.so*; do
  b=$(basename "$f"); stem=${b%%-*}; ver=${b##*.so.}; ln -sf "$f" "$D/$stem.so.${ver%%.*}"
done
export LD_LIBRARY_PATH="$D:$L:${LD_LIBRARY_PATH:-}"
"$VENV/bin/python" -c "from torchcodec.decoders import VideoDecoder; print('torchcodec ok')"
readlink -f "$VENV/bin/python"
"$VENV/bin/python" -c "import torch,transformers,sentence_transformers,onnxruntime,av;print(torch.__version__,transformers.__version__,sentence_transformers.__version__,onnxruntime.__version__,'av',av.__version__)"
