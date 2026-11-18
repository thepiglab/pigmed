"""Which inputs does the card runtime (sentence-transformers + EmbeddingGemma 2) accept, and what does it build?

For every file in images-formats.jsonl and media.jsonl: encode it alone through SentenceTransformer, record
accepted / error, decode backend, processor output shapes, token count, and wall time. Also checks the two
questions from the embeddings.py audit: an image-only dict {"image": path} and a mixed str/dict batch.
Writes <out>/probe-media.json.
"""

from __future__ import annotations

import argparse
import json
import os
import time
import traceback
from pathlib import Path

import numpy as np
import torch


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--inputs", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--threads", type=int, default=16)
    a = ap.parse_args()
    torch.set_num_threads(a.threads)
    from sentence_transformers import SentenceTransformer

    snap = json.load(open(Path(os.environ["EMBED_ROOT"]) / "models.json"))["google/embeddinggemma-2"]["path"]
    model = SentenceTransformer(snap, device="cpu", model_kwargs={"torch_dtype": torch.float32})
    hf = model[0].model
    seen = {}
    hf.register_forward_pre_hook(lambda m, ar, kw: seen.update(
        {k: (tuple(v.shape) if torch.is_tensor(v) else v) for k, v in kw.items() if v is not None}), with_kwargs=True)
    res = {"versions": {}, "items": [], "checks": {}}
    import importlib

    for mod in ("transformers", "sentence_transformers", "torchcodec", "av", "librosa", "soundfile", "PIL"):
        try:
            res["versions"][mod] = importlib.import_module(mod).__version__
        except Exception as e:
            res["versions"][mod] = f"missing ({type(e).__name__})"

    def enc(x, label):
        seen.clear()
        t = time.perf_counter()
        try:
            v = model.encode(x, convert_to_numpy=True, normalize_embeddings=True)
            return {"label": label, "ok": True, "shape": list(np.asarray(v).shape), "s": time.perf_counter() - t,
                    "model_inputs": {k: v for k, v in seen.items()}}
        except Exception as e:
            return {"label": label, "ok": False, "error": f"{type(e).__name__}: {str(e)[:400]}",
                    "where": traceback.format_exc().strip().splitlines()[-3:]}

    rows = [json.loads(l) for l in open(a.inputs / "images-formats.jsonl")]
    rows += [json.loads(l) for l in open(a.inputs / "media.jsonl")]
    for r in rows:
        p = str(a.inputs / r["path"])
        out = enc([{r["kind"]: p}], r["id"])
        out.update(kind=r["kind"], format=r["format"], seconds=r.get("seconds"))
        res["items"].append(out)
        print(json.dumps({k: out.get(k) for k in ("label", "ok", "shape", "s", "error")}), flush=True)
    img = str(a.inputs / "images/page00.png")
    res["checks"]["image_only_dict"] = enc([{"image": img}], "image-only dict")
    res["checks"]["mixed_str_dict_batch"] = enc(["title: none | text: hello world", {"image": img}], "mixed batch")
    res["checks"]["bare_path_string"] = enc([img], "bare path string (treated as text?)")
    from PIL import Image

    res["checks"]["pil_image"] = enc([Image.open(img)], "PIL image object")
    a.out.mkdir(parents=True, exist_ok=True)
    (a.out / "probe-media.json").write_text(json.dumps(res, indent=1, default=str))
    print(json.dumps(res["checks"], default=str)[:3000])
    print(json.dumps(res["versions"]))


if __name__ == "__main__":
    main()
