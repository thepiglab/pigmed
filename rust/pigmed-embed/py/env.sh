# Derived settings for the embed runner. Your site env file must export EMBED_ROOT (persistent, group-owned
# storage for code copy, toolchains, venv, HF cache, ONNX graphs, outputs) and then source this file.
# See configs/site.env.example.
: "${EMBED_ROOT:?export EMBED_ROOT in your site env (configs/site.env.example)}"
export EMBED_CODE="${EMBED_CODE:-$EMBED_ROOT/code}"   # rsynced copy of rust/pigmed-embed
export HF_HOME="${HF_HOME_EMBED:-$EMBED_ROOT/hf}"
export UV_CACHE_DIR="$EMBED_ROOT/cache/uv"
export UV_PYTHON_INSTALL_DIR="$EMBED_ROOT/cache/uv-python"
export XDG_CACHE_HOME="$EMBED_ROOT/cache/xdg"
export CARGO_HOME="$EMBED_ROOT/cargo"
export RUSTUP_HOME="$EMBED_ROOT/rustup"
export CARGO_TARGET_DIR="$EMBED_ROOT/target"
export VENV="${EMBED_VENV:-$EMBED_ROOT/venv}"
export UV_PYTHON_PREFERENCE=only-managed   # never link the venv to some other uv cache's interpreter
export PATH="$EMBED_ROOT/bin:$CARGO_HOME/bin:$VENV/bin:$HOME/.local/bin:$PATH"
# FFmpeg shared libs for torchcodec (see examples/slurm/embed-rs-venv.sbatch); harmless if absent
[[ -d "$EMBED_ROOT/ffmpeg-shared" ]] && export LD_LIBRARY_PATH="$EMBED_ROOT/ffmpeg-shared:$VENV/lib/python3.12/site-packages/av.libs:${LD_LIBRARY_PATH:-}"
export TOKENIZERS_PARALLELISM=false
export ORT_DISABLE_TELEMETRY=1   # else ORT keeps a sqlite db in a 0700 dir under XDG_CACHE_HOME (breaks group inheritance)
umask 0002
