"""Python baseline: the runtime each model card recommends, FP32 on CPU.

  gemma  : sentence_transformers.SentenceTransformer("google/embeddinggemma-2"), text-only config for text,
           text+vision config for images (card: config_kwargs). Mean pooling + L2 normalize (modules.json).
  neomme : transformers.NeoMMEForRetrieval + NeoMMEProcessor.apply_chat_template(task="document").
           Returns dense_embeddings (1024-d) and embeddings (128-d per token/patch, masked).

One process = one (model, modality, threads, batch) config, so peak RSS (ru_maxrss) is per config.
Writes <out>/<tag>.json (timings) and, with --save, <out>/<tag>.{dense,late,mask}.npy reference vectors.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import resource
import subprocess
import sys
import time
from pathlib import Path


def lscpu_model() -> str:
    try:
        out = subprocess.run(["lscpu"], capture_output=True, text=True).stdout
        return next(l.split(":", 1)[1].strip() for l in out.splitlines() if l.startswith("Model name"))
    except Exception:
        return platform.processor()


def load_inputs(root: Path, modality: str, n: int, texts_file: str = "texts.jsonl", images_file: str = "images.jsonl",
                paths: bool = False):
    if modality == "text":
        rows = [json.loads(l) for l in open(root / texts_file)][:n]
        return [r["id"] for r in rows], [r["text"] for r in rows]
    from PIL import Image

    rows = [json.loads(l) for l in open(root / images_file)][:n]
    if paths or modality in ("audio", "video"):
        return [r["id"] for r in rows], [str(root / r["path"]) for r in rows]
    return [r["id"] for r in rows], [Image.open(root / r["path"]).convert("RGB") for r in rows]


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--model", choices=["gemma", "neomme"], required=True)
    ap.add_argument("--modality", choices=["text", "image", "audio", "video"], required=True)
    ap.add_argument("--threads", type=int, required=True)
    ap.add_argument("--batch", type=int, required=True)
    ap.add_argument("--n", type=int, default=32, help="items in the timed pass")
    ap.add_argument("--warmup", type=int, default=1, help="warmup batches (not timed)")
    ap.add_argument("--max-len", type=int, default=None,
                    help="max tokens (default 512 for text; 8192 for image/audio/video: a 30 s clip is 752 tokens)")
    ap.add_argument("--inputs", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--save", action="store_true")
    ap.add_argument("--texts-file", default="texts.jsonl", help="jsonl under --inputs (id, text)")
    ap.add_argument("--images-file", default="images.jsonl", help="jsonl under --inputs (id, path)")
    ap.add_argument("--side", choices=["document", "query"], default="document", help="NeoMME template side")
    ap.add_argument("--tag-suffix", default="", help="appended to the output tag, e.g. _L2048 (no '-')")
    ap.add_argument("--max-soft-tokens", type=int, default=280, help="Gemma image soft-token budget")
    ap.add_argument("--decode-in-loop", action="store_true", help="pass image file paths (decode inside timing)")
    args = ap.parse_args()
    if args.max_len is None:
        args.max_len = 512 if args.modality == "text" else 8192

    import numpy as np
    import torch

    torch.set_num_threads(args.threads)
    torch.set_num_interop_threads(1)
    models = json.load(open(Path(os.environ["EMBED_ROOT"]) / "models.json"))
    ids, items = load_inputs(args.inputs, args.modality, args.n, args.texts_file, args.images_file, args.decode_in_loop)
    tag = f"py-{args.model}-{args.modality}-t{args.threads}-b{args.batch}{args.tag_suffix}"
    extra = {}
    t_load = time.perf_counter()

    if args.model == "gemma":
        from sentence_transformers import SentenceTransformer

        snap = models["google/embeddinggemma-2"]["path"]
        cfg = {"text": {"vision_config": None, "audio_config": None}, "image": {"audio_config": None},
               "video": {"audio_config": None}, "audio": {"vision_config": None}}[args.modality]
        model = SentenceTransformer(snap, device="cpu", config_kwargs=cfg,
                                    model_kwargs={"torch_dtype": torch.float32})
        extra["st_default_max_seq_length"] = model.max_seq_length
        model.max_seq_length = args.max_len
        if args.modality == "image" and args.max_soft_tokens != 280:
            model[0].processor.image_processor.max_soft_tokens = args.max_soft_tokens
            extra["max_soft_tokens"] = args.max_soft_tokens
        model.eval()

        def run(batch):
            # Texts already carry the card's document format ("title: ... | text: ..."); images take no prefix.
            if args.modality != "text" and isinstance(batch[0], str):
                batch = [{args.modality: x} for x in batch]  # file paths -> modality dicts (card usage)
            v = model.encode(batch, batch_size=len(batch), convert_to_numpy=True, normalize_embeddings=True)
            return {"dense": v}
    else:
        from transformers import NeoMMEForRetrieval, NeoMMEProcessor

        snap = models["Hcompany/NeoMME-260M-Retriever"]["path"]
        proc = NeoMMEProcessor.from_pretrained(snap)
        model = NeoMMEForRetrieval.from_pretrained(snap, dtype=torch.float32).eval()

        def run(batch):
            if args.modality == "text":
                msgs = [[{"role": "user", "content": t}] for t in batch]
            else:
                msgs = [[{"role": "user", "content": [{"type": "image", "image": im}]}] for im in batch]
            kw = {"padding": "longest"}
            if args.modality == "text":
                kw.update(truncation=True, max_length=args.max_len)
            inp = proc.apply_chat_template(msgs, task=args.side, tokenize=True, return_dict=True,
                                           return_tensors="pt", processor_kwargs=kw)
            with torch.inference_mode():
                o = model(**inp)
            return {"dense": o.dense_embeddings.float().numpy(), "late": o.embeddings.float().numpy(),
                    "mask": inp["attention_mask"].numpy(), "ntok": int(inp["attention_mask"].sum())}

    load_s = time.perf_counter() - t_load
    # NeoMME: longest-first batching, the same as sentence-transformers does for Gemma (and as the Rust runner);
    # outputs are restored to input order before saving.
    perm = list(range(len(items)))
    if args.model == "neomme" and args.batch > 1:
        size = (lambda x: len(x)) if args.modality == "text" else (lambda x: x.width * x.height if hasattr(x, "width") else 0)
        perm = sorted(perm, key=lambda i: -size(items[i]))
    items_sorted = [items[i] for i in perm]
    batches = [items_sorted[i:i + args.batch] for i in range(0, len(items_sorted), args.batch)]

    # With --save, capture the exact tensors the HF model receives for every batch (tokenizer/processor
    # output), so the Rust preprocessing can be diffed against them, not just the final vectors.
    captured: list[dict] = []
    ntoks: list[int] = []
    hf_model = model[0].model if args.model == "gemma" else model
    hf_model.register_forward_pre_hook(
        lambda _m, a, kw: ntoks.append(int(kw["attention_mask"].sum())) if "attention_mask" in kw else None,
        with_kwargs=True)
    if args.save:
        hf = model[0].model if args.model == "gemma" else model

        def hook(_mod, a, kw):
            captured.append({k: v.detach().cpu().numpy() for k, v in kw.items() if torch.is_tensor(v)})

        handle = hf.register_forward_pre_hook(hook, with_kwargs=True)
    with torch.inference_mode():
        for b in batches[: args.warmup]:
            run(b)
        captured.clear()
        ntoks.clear()
        outs, lat = [], []
        t0 = time.perf_counter()
        for b in batches:
            s = time.perf_counter()
            outs.append(run(b))
            lat.append(time.perf_counter() - s)
        wall = time.perf_counter() - t0

    res = {
        "tag": tag, "runtime": "python", "model": args.model, "modality": args.modality, "threads": args.threads,
        "batch": args.batch, "n": len(items), "warmup_batches": args.warmup, "wall_s": wall,
        "items_per_s": len(items) / wall, "batch_latency_s": lat, "load_s": load_s,
        "peak_rss_mb": resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024,
        "tokens_total": sum(ntoks) or None, "side": args.side, "texts_file": args.texts_file, **extra,
        "dtype": "float32", "torch": torch.__version__, "cpu": lscpu_model(), "host": platform.node(),
        "job": os.environ.get("SLURM_JOB_ID"), "python": sys.version.split()[0],
    }
    import transformers

    res["transformers"] = transformers.__version__
    args.out.mkdir(parents=True, exist_ok=True)
    if args.save:
        inv = np.argsort(perm)  # sorted position -> input order
        dense = np.concatenate([o["dense"] for o in outs]).astype(np.float32)[inv]
        np.save(args.out / f"{tag}.dense.npy", dense)
        if "late" in outs[0]:
            # Ragged across batches (padding=longest): store per-item vectors, flattened with offsets.
            per_item = [v[m.astype(bool)] for o in outs for v, m in zip(o["late"], o["mask"])]
            late, offs = [], [0]
            for i in inv:
                keep = per_item[i]
                late.append(keep)
                offs.append(offs[-1] + len(keep))
            np.save(args.out / f"{tag}.late.npy", np.concatenate(late).astype(np.float32))
            np.save(args.out / f"{tag}.late_offsets.npy", np.array(offs, dtype=np.int64))
        json.dump(ids, open(args.out / f"{tag}.ids.json", "w"))
        handle.remove()
        np.savez_compressed(args.out / f"{tag}.inputs.npz",
                            **{f"b{i}_{k}": v for i, c in enumerate(captured) for k, v in c.items()})
    json.dump(res, open(args.out / f"{tag}.json", "w"), indent=1)
    print(json.dumps({k: v for k, v in res.items() if k != "batch_latency_s"}))


if __name__ == "__main__":
    main()
