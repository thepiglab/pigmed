"""Diff Rust-dumped Gemma patches (PIGMED_DUMP_PREP) against the HF processor, and measure how much a given
pixel difference moves the final embedding (processor pixels vs Rust pixels through the same torch model)."""
import json, sys, numpy as np, torch
from pathlib import Path
from PIL import Image
root, snap, dump = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3])
from transformers import AutoProcessor
proc = AutoProcessor.from_pretrained(snap)
rows = [json.loads(l) for l in open(root / "images.jsonl")]
for i, r in enumerate(rows):
    f = dump / f"gemma-prep-{i:03d}.npy"
    if not f.exists():
        continue
    im = Image.open(root / r["path"]).convert("RGB")
    pv = proc.image_processor(images=[im], return_tensors="pt")["pixel_values"][0].numpy()
    rs = np.load(f)
    d = np.abs(pv[: len(rs)] - rs)
    print(f"{r['id']}: n={len(rs)} max|d|*255={d.max()*255:.2f} mean|d|*255={d.mean()*255:.4f} frac>0.5/255={(d > 0.5/255).mean():.4f}")
