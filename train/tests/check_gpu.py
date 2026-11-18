"""GPU checks for S1.5/S2/S3 that the CPU unit tests cannot cover (one GPU, ~15 min):

1. late-chunk parity: contextual.encode_chunks dense (bf16) == flashembed text_late on the same articles
   (cos >= 0.999) -> training pools exactly what we serve
2. image parity: EG2Dual.forward_mm dense == flashembed image modality (cos >= 0.999) -> confirms images.IMAGE_DOC_TEXT
   and that the v2 image vectors can be the S3 teacher / miner
3. S2 cost: one ctx_train_step at 8k windows (q_per_rank, articles) -> s/step, peak GB
4. S3 cost: one image train_step (vision frozen and LoRA) -> s/step, peak GB

    python -m tests.check_gpu --base SNAP --flashembed BIN --images DIR_WITH_JPEGS --articles articles.jsonl(.gz)
Prints one JSON report. Steps whose inputs are missing are skipped and say so.
"""
import argparse
import gzip
import json
import subprocess
import tempfile
import time
from pathlib import Path
from types import SimpleNamespace

import numpy as np
import torch
import torch.nn.functional as F

from eg2late.contextual import article_input, build_article, ctx_train_step, encode_chunks
from eg2late.model import EG2Dual, Tok
from eg2late.train_s1 import load_vecs


def flashembed(bin_, snap, modality, rows, tmp: Path, max_len=8192):
    inp, out = tmp / f"{modality}.jsonl", tmp / f"{modality}-out"
    inp.write_text("".join(json.dumps(r) + "\n" for r in rows))
    r = subprocess.run([bin_, "embed", "--model", "gemma", "--modality", modality, "--input", str(inp), "--out", str(out),
                        "--snapshot", str(snap), "--dtype", "bf16", "--max-len", str(max_len)], capture_output=True, text=True)
    if r.returncode:
        raise RuntimeError(r.stderr[-1500:])
    ids, v = load_vecs(out)
    return ids, np.asarray(v, dtype=np.float32)


def read_articles(p: Path, n: int):
    op = gzip.open if p.suffix == ".gz" else open
    out = []
    with op(p, "rt") as f:
        for line in f:
            out.append(json.loads(line))
            if len(out) >= n:
                break
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", required=True)
    ap.add_argument("--flashembed", default=None)
    ap.add_argument("--images", type=Path, default=None)
    ap.add_argument("--articles", type=Path, default=None)
    ap.add_argument("--n", type=int, default=8)
    ap.add_argument("--synthetic-images", type=int, default=0, help="make N random JPEGs if --images is not given")
    a = ap.parse_args()
    rep = {"gpu": torch.cuda.get_device_name()}
    tmp = Path(tempfile.mkdtemp())
    if a.images is None and a.synthetic_images:
        from PIL import Image
        a.images = tmp / "img"
        a.images.mkdir()
        g = np.random.default_rng(0)
        for i in range(a.synthetic_images):
            h, w = int(g.integers(200, 900)), int(g.integers(200, 900))
            Image.fromarray(g.integers(0, 255, (h, w, 3), dtype=np.uint8)).save(a.images / f"{i:03d}.jpg", quality=90)
    tok = Tok(a.base)
    m = EG2Dual.load(a.base, vision=True).cuda().eval()

    # 1. late-chunk parity
    try:
        if a.articles and a.flashembed:
            arts = read_articles(a.articles, a.n)
            rows = [{"id": r["article_id"], **article_input(r.get("title"), r.get("abstract"), r["chunks"])} for r in arts]
            ids, fe = flashembed(a.flashembed, a.base, "text_late", rows, tmp)
            fe_by = {x: fe[i] for i, x in enumerate(ids)}
            cos = []
            with torch.no_grad():
                for row in rows:
                    art = build_article(tok.tk, row)
                    d, _, _ = encode_chunks(m, [torch.tensor(w) for w in art["wins"]], art["place"], 16)
                    for c, cid in enumerate(art["ids"]):
                        if cid in fe_by:
                            cos.append(float(F.cosine_similarity(d[c].cpu(), torch.from_numpy(fe_by[cid]), dim=0)))
            rep["late_chunk_parity"] = {"chunks": len(cos), "cos_min": min(cos) if cos else None,
                                        "cos_mean": float(np.mean(cos)) if cos else None}
        else:
            rep["late_chunk_parity"] = "skipped (needs --articles and --flashembed)"
    except Exception as e:  # noqa: BLE001
        rep['error_1'] = f"{type(e).__name__}: {str(e)[:400]}"

    # 2. image parity
    try:
        if a.images and a.flashembed:
            from eg2late.images import ImageEncoderInputs
            from PIL import Image
            paths = sorted(p for p in a.images.iterdir() if p.suffix.lower() in (".jpg", ".jpeg", ".png"))[: a.n]
            ids, fe = flashembed(a.flashembed, a.base, "image", [{"id": p.name, "path": str(p)} for p in paths], tmp)
            fe_by = {x: fe[i] for i, x in enumerate(ids)}
            inp = ImageEncoderInputs(a.base)
            cos = []
            with torch.no_grad():
                for p in paths:
                    x = {k: v.cuda() for k, v in inp([Image.open(p).convert("RGB")]).items()}
                    with torch.autocast("cuda", dtype=torch.bfloat16):
                        d, _ = m.forward_mm(x["input_ids"], x["attention_mask"], x["late_mask"], x["pixel_values"],
                                            x["image_position_ids"])
                    cos.append(float(F.cosine_similarity(d[0].float().cpu(), torch.from_numpy(fe_by[p.name]), dim=0)))
            rep["image_parity"] = {"images": len(cos), "cos_min": min(cos), "cos_mean": float(np.mean(cos))}
        else:
            rep["image_parity"] = "skipped (needs --images and --flashembed)"
    except Exception as e:  # noqa: BLE001
        rep['error_2'] = f"{type(e).__name__}: {str(e)[:400]}"

    # 3. S2 cost at 8k windows
    try:
        if a.articles:
            from eg2late.train_s2 import CtxData
            arts = read_articles(a.articles, 64)
            root = tmp / "ctx"
            root.mkdir()
            with gzip.open(root / "articles.jsonl.gz", "wt") as f, open(root / "ctx_queries.jsonl", "w") as fq:
                for r in arts:
                    f.write(json.dumps(r) + "\n")
                    for c in r["chunks"][:8]:  # synthetic queries: first words of a chunk (cost only)
                        fq.write(json.dumps({"id": c["id"] + ":q", "text": " ".join(c["text"].split()[:12]),
                                             "article_id": r["article_id"], "pos": [c["id"]]}) + "\n")
            s = SimpleNamespace(q_per_rank=64, q_per_article=8, wmax=8192, overlap=1024, q_len=96, chunk_len=256,
                                mb_q=64, lam_seq=0.15, seq_in_batch="exclude", beta=0.3, tau_d=0.05, tau_l=0.02,
                                tau_t=0.05, w_dense=1.0, w_late=1.0, w_align=0.1, w_kd=0.5)
            probe = CtxData(root, tok, s, [])
            plan = [(probe.order, 0)]
            b = CtxData(root, tok, s, plan)[0]
            m.train()
            torch.cuda.reset_peak_memory_stats()
            t0 = time.time()
            out = ctx_train_step(m, b, s)
            torch.cuda.synchronize()
            rep["s2_step"] = {"windows": len(b["wins"]), "tokens": int(sum(len(w) for w in b["wins"])),
                              "chunks": len(b["place"]), "queries": 64, "s": round(time.time() - t0, 2),
                              "peak_gb": round(torch.cuda.max_memory_allocated() / 2**30, 1), "loss": out["total"]}
            m.zero_grad(set_to_none=True)
    except Exception as e:  # noqa: BLE001
        rep['error_3'] = f"{type(e).__name__}: {str(e)[:400]}"

    # 4. S3 cost (image docs; frozen then LoRA)
    try:
        if a.images:
            from eg2late import train_s1 as ts
            from eg2late.images import ImageEncoderInputs
            from PIL import Image
            paths = sorted(p for p in a.images.iterdir() if p.suffix.lower() in (".jpg", ".jpeg", ".png"))
            B, K = 32, 3
            imgs = [Image.open(paths[i % len(paths)]).convert("RGB") for i in range(B * (1 + K))]
            d = ImageEncoderInputs(a.base)(imgs)
            q = tok([f"task: search result | query: figure {i}" for i in range(B)], 29, 64)
            q.pop("offsets")
            hd = torch.arange(B * (1 + K))
            batch = {"q": q, "d": d, "gold": torch.cat([hd[:B, None], torch.full((B, 7), -1)], 1), "doc_hash": hd,
                     "qgroup": torch.full((B,), -1), "dgroup": torch.full((B * (1 + K),), -1), "tq": None, "td": None,
                     "sym": torch.ones(B, dtype=torch.bool)}
            s = SimpleNamespace(mb_q=64, mb_d=16, tau_d=0.05, tau_l=0.02, tau_t=0.05, w_dense=1.0, w_late=1.0, w_align=0.1,
                                w_kd_dense=0.0, w_kd_late=0.0, w_rev=1.0, qq_neg=0.0, qq_margin=0.1)
            for mode in ("frozen", "lora"):
                m.vision_mode(mode, r=16, alpha=32)
                m.train()
                torch.cuda.reset_peak_memory_stats()
                t0 = time.time()
                out, _ = ts.train_step(m, batch, s)
                torch.cuda.synchronize()
                rep[f"s3_step_{mode}"] = {"queries": B, "images": B * (1 + K), "s": round(time.time() - t0, 2),
                                          "peak_gb": round(torch.cuda.max_memory_allocated() / 2**30, 1),
                                          "loss": out["total"]}
                m.zero_grad(set_to_none=True)
    except Exception as e:  # noqa: BLE001
        rep['error_4'] = f"{type(e).__name__}: {str(e)[:400]}"

    print(json.dumps(rep, indent=1))


if __name__ == "__main__":
    main()
