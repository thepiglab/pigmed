"""GPU throughput + parity benchmark for the model-card runtimes (sentence-transformers / transformers).

Embeds the same real corpus units the CPU runner (flashembed) embedded, on a CUDA device, in float32 and bfloat16,
over a sweep of batch sizes, and saves the vectors so `compare` can report cosine vs the CPU vectors per unit.

  prepare  --bundle-items B/items.jsonl.gz --out DIR [--max-text N] [--max-images M]
             -> DIR/texts.jsonl (raw unit text), DIR/images.jsonl (+ downloaded originals in DIR/images/)
  run      --model gemma|neomme --modality text|image --inputs DIR --out OUT --device cuda --dtype float32|bfloat16
           --batches 8,32,128 [--max-len 1024]
  compare  --gpu OUT --cpu VECTORS/<model>/<bundle> -> cosine stats per (model, modality, dtype)

Document formats match the CPU run (scripts/embed_bundles.py): EmbeddingGemma 2 texts get "title: none | text: ",
NeoMME texts are raw with task="document"; images take no prefix.
"""
from __future__ import annotations

import argparse
import gzip
import json
import os
import sys
import time
from pathlib import Path

TEXT_KINDS = {"text", "table_text", "caption", "supplement_text", "table_card"}


def https(url: str) -> str:
    if url.startswith("s3://"):
        bucket, _, key = url[5:].partition("/")
        return f"https://{bucket}.s3.amazonaws.com/{key.split('?')[0]}"
    return url


def cmd_prepare(a):
    import requests
    from PIL import Image
    a.out.mkdir(parents=True, exist_ok=True)
    (a.out / "images").mkdir(exist_ok=True)
    texts, figs = [], []
    for line in gzip.open(a.bundle_items, "rt"):
        r = json.loads(line)
        if r.get("kind") in TEXT_KINDS and (r.get("text") or "").strip():
            texts.append({"id": r["unit_id"], "text": r["text"]})
        elif r.get("kind") == "figure" and r.get("source_url"):
            figs.append((r["unit_id"], r["source_url"]))
    texts = texts[: a.max_text]
    imgs = []
    s = requests.Session()
    for uid, url in figs:
        if len(imgs) >= a.max_images:
            break
        dest = a.out / "images" / (uid + (Path(url.split("?")[0]).suffix.lower() or ".bin"))
        try:
            r = s.get(https(url), timeout=60)
            r.raise_for_status()
            dest.write_bytes(r.content)
            with Image.open(dest) as im:
                im.load()
            imgs.append({"id": uid, "path": f"images/{dest.name}"})
        except Exception:
            dest.unlink(missing_ok=True)
    with open(a.out / "texts.jsonl", "w") as f:
        f.writelines(json.dumps(t) + "\n" for t in texts)
    with open(a.out / "images.jsonl", "w") as f:
        f.writelines(json.dumps(i) + "\n" for i in imgs)
    print(json.dumps({"texts": len(texts), "images": len(imgs), "bundle": str(a.bundle_items)}))


def cmd_run(a):
    import numpy as np
    import torch
    from PIL import Image

    dev = torch.device(a.device)
    dtype = {"float32": torch.float32, "bfloat16": torch.bfloat16, "float16": torch.float16}[a.dtype]
    if a.device.startswith("cuda"):
        torch.backends.cuda.matmul.allow_tf32 = False  # keep float32 runs true float32 for parity
        torch.backends.cudnn.allow_tf32 = False
    snaps = json.load(open(a.models_json))
    rows = [json.loads(l) for l in open(a.inputs / ("texts.jsonl" if a.modality == "text" else "images.jsonl"))]
    ids = [r["id"] for r in rows]
    t_load = time.perf_counter()
    if a.model == "gemma":
        from sentence_transformers import SentenceTransformer
        cfg = {"text": {"vision_config": None, "audio_config": None}, "image": {"audio_config": None}}[a.modality]
        model = SentenceTransformer(snaps["google/embeddinggemma-2"]["path"], device=str(dev), config_kwargs=cfg,
                                    model_kwargs={"torch_dtype": dtype})
        model.max_seq_length = a.max_len
        model.eval()
        items = ([f"title: none | text: {r['text']}" for r in rows] if a.modality == "text"
                 else [{"image": str(a.inputs / r["path"])} for r in rows])

        def encode(batch):
            return model.encode(items if batch is None else batch, batch_size=a._bs, convert_to_numpy=True,
                                normalize_embeddings=True, show_progress_bar=False)

        def run_all(bs):
            a._bs = bs
            return encode(items)
    else:
        from transformers import NeoMMEForRetrieval, NeoMMEProcessor
        snap = snaps["Hcompany/NeoMME-260M-Retriever"]["path"]
        proc = NeoMMEProcessor.from_pretrained(snap)
        model = NeoMMEForRetrieval.from_pretrained(snap, dtype=dtype).to(dev).eval()
        if a.modality == "text":
            items = [r["text"] for r in rows]
            order = sorted(range(len(items)), key=lambda i: -len(items[i]))
        else:
            items = [Image.open(a.inputs / r["path"]).convert("RGB") for r in rows]
            order = sorted(range(len(items)), key=lambda i: -(items[i].width * items[i].height))

        def run_all(bs):
            outs = [None] * len(items)
            for s in range(0, len(order), bs):
                idx = order[s: s + bs]
                if a.modality == "text":
                    msgs = [[{"role": "user", "content": items[i]}] for i in idx]
                    kw = {"padding": "longest", "truncation": True, "max_length": a.max_len}
                else:
                    msgs = [[{"role": "user", "content": [{"type": "image", "image": items[i]}]}] for i in idx]
                    kw = {"padding": "longest"}
                inp = proc.apply_chat_template(msgs, task="document", tokenize=True, return_dict=True,
                                               return_tensors="pt", processor_kwargs=kw)
                inp = {k: (v.to(dev) if torch.is_tensor(v) else v) for k, v in inp.items()}
                with torch.inference_mode():
                    d = model(**inp).dense_embeddings.float()
                d = torch.nn.functional.normalize(d, dim=-1).cpu().numpy()
                for j, i in enumerate(idx):
                    outs[i] = d[j]
            return np.stack(outs)
    load_s = time.perf_counter() - t_load
    gpu = torch.cuda.get_device_name(dev) if dev.type == "cuda" else "cpu"
    a.out.mkdir(parents=True, exist_ok=True)
    results = []
    for bs in [int(x) for x in a.batches.split(",")]:
        try:
            if dev.type == "cuda":
                torch.cuda.reset_peak_memory_stats(dev)
            run_all(bs) if len(items) <= 64 else None  # warm-up on small sets is the run itself
            if len(items) > 64:  # warm up kernels / autotuning on a slice
                keep = items
                items = items[: max(bs, 16)]
                if a.model == "neomme":
                    o2 = order
                    order = sorted(range(len(items)), key=lambda i: 0)
                run_all(bs)
                items = keep
                if a.model == "neomme":
                    order = o2
            if dev.type == "cuda":
                torch.cuda.synchronize(dev)
            t0 = time.perf_counter()
            v = run_all(bs)
            if dev.type == "cuda":
                torch.cuda.synchronize(dev)
            wall = time.perf_counter() - t0
            peak = torch.cuda.max_memory_allocated(dev) / 2**20 if dev.type == "cuda" else None
            row = {"model": a.model, "modality": a.modality, "dtype": a.dtype, "batch": bs, "n": len(items),
                   "wall_s": round(wall, 2), "items_per_s": round(len(items) / wall, 2), "gpu_peak_mb": peak,
                   "gpu": gpu, "load_s": round(load_s, 1), "max_len": a.max_len,
                   "torch": torch.__version__, "host": os.uname().nodename, "job": os.environ.get("SLURM_JOB_ID")}
            np.save(a.out / f"{a.model}-{a.modality}-{a.dtype}-b{bs}.dense.npy", v.astype(np.float32))
        except torch.cuda.OutOfMemoryError:
            row = {"model": a.model, "modality": a.modality, "dtype": a.dtype, "batch": bs, "oom": True, "gpu": gpu}
            torch.cuda.empty_cache()
        print(json.dumps(row), flush=True)
        results.append(row)
    json.dump(ids, open(a.out / f"{a.model}-{a.modality}.ids.json", "w"))
    with open(a.out / "results.jsonl", "a") as f:
        f.writelines(json.dumps(r) + "\n" for r in results)


def cmd_compare(a):
    import numpy as np
    out = []
    for model in ("gemma", "neomme"):
        cpu = a.cpu_root / model / a.bundle
        if not (cpu / "meta.json").exists():
            continue
        cv = np.load(cpu / "dense.npy").astype(np.float32)
        cid = {u: i for i, u in enumerate(json.load(open(cpu / "ids.json"))["unit_id"])}
        for mod in ("text", "image"):
            idf = a.gpu / f"{model}-{mod}.ids.json"
            if not idf.exists():
                continue
            gid = json.load(open(idf))
            for f in sorted(a.gpu.glob(f"{model}-{mod}-*.dense.npy")):
                g = np.load(f).astype(np.float32)
                both = [(i, cid[u]) for i, u in enumerate(gid) if u in cid]
                gi, ci = np.array([x for x, _ in both]), np.array([y for _, y in both])
                cos = (g[gi] * cv[ci]).sum(1) / (np.linalg.norm(g[gi], axis=1) * np.linalg.norm(cv[ci], axis=1))
                out.append({"file": f.name, "n": len(both), "min_cos": float(cos.min()), "p01_cos": float(np.percentile(cos, 1)),
                            "median_cos": float(np.median(cos)), "below_0.999": int((cos < 0.999).sum())})
    for r in out:
        print(json.dumps(r))
    json.dump(out, open(a.gpu / "parity.json", "w"), indent=1)


def main():
    ap = argparse.ArgumentParser()
    sp = ap.add_subparsers(dest="cmd", required=True)
    p = sp.add_parser("prepare")
    p.add_argument("--bundle-items", type=Path, required=True)
    p.add_argument("--out", type=Path, required=True)
    p.add_argument("--max-text", type=int, default=4000)
    p.add_argument("--max-images", type=int, default=400)
    r = sp.add_parser("run")
    r.add_argument("--model", choices=["gemma", "neomme"], required=True)
    r.add_argument("--modality", choices=["text", "image"], required=True)
    r.add_argument("--inputs", type=Path, required=True)
    r.add_argument("--out", type=Path, required=True)
    r.add_argument("--models-json", type=Path, required=True)
    r.add_argument("--device", default="cuda")
    r.add_argument("--dtype", default="float32")
    r.add_argument("--batches", default="8,32,128")
    r.add_argument("--max-len", type=int, default=1024)
    c = sp.add_parser("compare")
    c.add_argument("--gpu", type=Path, required=True)
    c.add_argument("--cpu-root", type=Path, required=True)
    c.add_argument("--bundle", required=True)
    a = ap.parse_args()
    {"prepare": cmd_prepare, "run": cmd_run, "compare": cmd_compare}[a.cmd](a)


if __name__ == "__main__":
    sys.exit(main())
