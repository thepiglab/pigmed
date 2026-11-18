"""A tiny random EmbeddingGemma 2 snapshot for CPU unit tests (~9M params, real tokenizer and processor files).

The real snapshot is only read for its config, tokenizer and processor files: $EG2_SNAPSHOT, else the newest
google/embeddinggemma-2 snapshot in the HF cache. Tests that need it skip when neither exists.
"""
from __future__ import annotations

import glob
import os
import shutil
from pathlib import Path

import torch


def real_snapshot() -> Path | None:
    p = os.environ.get("EG2_SNAPSHOT")
    if p and Path(p).exists():
        return Path(p)
    hub = os.environ.get("HF_HUB_CACHE") or os.path.join(os.environ.get("HF_HOME", os.path.expanduser("~/.cache/huggingface")), "hub")
    snaps = sorted(glob.glob(os.path.join(hub, "models--google--embeddinggemma-2", "snapshots", "*")), key=os.path.getmtime)
    return Path(snaps[-1]) if snaps else None


def make_tiny(out: Path, seed: int = 0, vision: bool = True) -> Path:
    """Write a tiny random EG2 snapshot to `out` (idempotent) and return it."""
    from transformers import AutoConfig, AutoModel
    out = Path(out)
    if (out / "model.safetensors").exists():
        return out
    src = real_snapshot()
    if src is None:
        raise FileNotFoundError("no EmbeddingGemma 2 snapshot (set EG2_SNAPSHOT)")
    c = AutoConfig.from_pretrained(src)
    t = c.text_config
    t.hidden_size, t.hidden_size_per_layer_input, t.intermediate_size = 32, 8, 64
    t.num_hidden_layers, t.num_attention_heads, t.num_key_value_heads, t.head_dim = 4, 2, 1, 16
    t.layer_types = ["sliding_attention", "full_attention", "sliding_attention", "full_attention"]
    t.per_layer_config = {"01": {"head_dim": 32, "num_key_value_heads": 1}, "03": {"head_dim": 32, "num_key_value_heads": 1}}
    t.embedding_dim, t.sliding_window = 24, 8
    v = c.vision_config
    v.hidden_size, v.intermediate_size, v.num_hidden_layers = 32, 64, 1
    v.num_attention_heads, v.num_key_value_heads, v.head_dim, v.global_head_dim = 2, 2, 16, 16
    c.audio_config = None
    if not vision:
        c.vision_config = None
    torch.manual_seed(seed)
    m = AutoModel.from_config(c)
    for p in m.parameters():  # bigger random weights so outputs are not ~constant
        if p.dim() > 1:
            torch.nn.init.normal_(p, std=0.2)
    tmp = out.with_name(out.name + ".tmp")
    shutil.rmtree(tmp, ignore_errors=True)
    m.save_pretrained(tmp)
    for f in src.iterdir():
        if f.is_file() and f.name not in ("model.safetensors", "config.json", "README.md"):
            shutil.copy2(f.resolve(), tmp / f.name)
    tmp.rename(out)
    return out
