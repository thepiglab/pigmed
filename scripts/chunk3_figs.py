"""Figure retrieval check on one pigmed-pmc-eg2 shard trio (chunks / vectors / media). Research only.

  prep  --hf DIR --out DIR [--n 5000]
        Picks figures that have an image vector, a caption unit with a text vector, a figure number, and an in-text
        mention in a body chunk of the same article (a sentence containing "Fig./Figure <n>").
        Writes DIR/queries.json [{qid, task, text, figure}] and DIR/root-0 (query units for EG2; prompt
        "task: search result | query: "), plus DIR/figs.npz (figure keys, caption vectors, image vectors).
        Queries are the mention sentence with the figure reference removed. The caption is never the query, so
        nothing is held out of the caption vector.
  score --out DIR --qvec VEC_DIR
        Rank all sample figures for each query by caption cos, image cos, and normalize(cap + b*img) mixes.
        Reports MRR@10 / R@1 / R@10 with paired bootstrap CIs vs caption-only.
"""
import argparse
import json
import re
import sys
from collections import defaultdict
from pathlib import Path

import numpy as np
import pyarrow.parquet as pq

sys.path.insert(0, str(Path(__file__).parent))
from chunk3_arms import sentences_of, write_bundle  # noqa: E402

NUM = re.compile(r"(\d+)")
MENTION = re.compile(r"\b(?:Fig(?:ure)?s?\.?)\s*(\d+)\b", re.I)


def fig_number(figure_id, caption):
    m = re.match(r"\s*(?:Fig(?:ure)?\.?)\s*(\d+)", caption or "", re.I)
    if m:
        return int(m.group(1))
    m = re.search(r"(?:fig|f|g)[_-]?0*(\d+)$", (figure_id or "").lower())
    return int(m.group(1)) if m else None


def cmd_prep(a):
    hf = a.hf
    med = pq.read_table(next(hf.glob("media/*.parquet")), columns=["unit_id", "article_id", "kind", "role", "modality", "caption", "figure_id"]).to_pylist()
    ch = pq.read_table(next(hf.glob("chunks/*.parquet")), columns=["unit_id", "article_id", "kind", "text", "figure_id"]).to_pylist()
    imgs = {r["unit_id"]: r for r in med if r.get("modality") == "image" and r.get("figure_id")}
    caps = {(r["article_id"], r["figure_id"]): r for r in ch if r["kind"] == "caption" and r.get("figure_id")}
    body = defaultdict(list)
    for r in ch:
        if r["kind"] == "text":
            body[r["article_id"]].append(r["text"])
    vt = pq.read_table(next(hf.glob("vectors/*.parquet")), columns=["unit_id", "modality", "embedding"])
    need = set(imgs) | {c["unit_id"] for c in caps.values()}
    uid = vt.column("unit_id").to_pylist()
    keep = [i for i, u in enumerate(uid) if u in need]
    emb = vt.column("embedding").take(keep).combine_chunks().flatten().to_numpy(zero_copy_only=False).astype(np.float32).reshape(len(keep), -1)
    vec = {uid[i]: emb[j] for j, i in enumerate(keep)}
    figs = []
    for u, m in imgs.items():
        c = caps.get((m["article_id"], m["figure_id"]))
        if not c or u not in vec or c["unit_id"] not in vec:
            continue
        n = fig_number(m["figure_id"], c.get("text") or m.get("caption"))
        if n is None:
            continue
        figs.append((m["article_id"], m["figure_id"], n, u, c["unit_id"]))
    rng = np.random.default_rng(0)
    rng.shuffle(figs)
    qs, keys, cv, iv = [], [], [], []
    by_art = defaultdict(list)
    for f in figs:
        by_art[f[0]].append(f)
    used = set()
    for f in figs:
        if len(keys) >= a.n:
            break
        aid, fid, n, iu, cu = f
        if (aid, n) in used:
            continue
        sents = []
        for t in body[aid]:
            for s0, s1 in sentences_of(t):
                s = t[s0:s1]
                if any(int(x) == n for x in MENTION.findall(s)) and 8 <= len(s.split()) <= 80:
                    sents.append(s)
        if not sents:
            continue
        used.add((aid, n))
        q = MENTION.sub("", sents[0])
        q = re.sub(r"\(\s*[,;and ]*\)|\[\s*\]", "", q)
        q = re.sub(r"\s+", " ", q).strip()
        key = f"{aid}|{fid}"
        qs.append({"qid": f"q{len(keys)}", "task": "mention", "text": q, "figure": key, "article_id": aid})
        keys.append(key)
        cv.append(vec[cu])
        iv.append(vec[iu])
    a.out.mkdir(parents=True, exist_ok=True)
    (a.out / "queries.json").write_text(json.dumps(qs))
    np.savez(a.out / "figs.npz", keys=np.array(keys), cap=np.stack(cv), img=np.stack(iv))
    write_bundle(a.out / "root-0", "q", "q-000000000",
                 [{"unit_id": f"p0|{q['qid']}", "article_id": q["qid"], "kind": "text", "text": q["text"],
                   "embed_text": "task: search result | query: " + q["text"]} for q in qs])
    print(json.dumps({"figures_with_img_cap_num": len(figs), "sample": len(keys), "articles": len({q['article_id'] for q in qs})}))


def norm(x):
    return x / np.maximum(np.linalg.norm(x, axis=1, keepdims=True), 1e-12)


def boot(d, n=2000):
    r = np.random.default_rng(0)
    m = [d[r.integers(0, len(d), len(d))].mean() for _ in range(n)]
    return [float(np.percentile(m, 2.5)), float(np.percentile(m, 97.5))]


def cmd_score(a):
    qs = json.loads((a.out / "queries.json").read_text())
    z = np.load(a.out / "figs.npz")
    keys, cap, img = list(z["keys"]), norm(z["cap"]), norm(z["img"])
    d = a.qvec / "gemma"
    qv = {}
    for m in d.glob("*/meta.json"):
        ids = json.loads((m.parent / "ids.json").read_text())["unit_id"]
        v = np.load(m.parent / "dense.npy").astype(np.float32)
        qv.update({u.split("|", 1)[1]: v[i] for i, u in enumerate(ids)})
    qs = [q for q in qs if q["qid"] in qv]
    Q = norm(np.stack([qv[q["qid"]] for q in qs]))
    tgt = np.array([keys.index(q["figure"]) for q in qs])
    variants = {"caption": cap, "image": img}
    for b in (0.25, 0.5, 1.0):
        variants[f"cap+{b}img"] = norm(cap + b * img)
    res, base = {"n": len(qs), "figures": len(keys)}, None
    for name, D in variants.items():
        S = Q @ D.T
        if name.startswith("score"):
            pass
        tv = S[np.arange(len(qs)), tgt]
        rank = (S > tv[:, None]).sum(1) + 1
        rr = np.where(rank <= 10, 1.0 / rank, 0.0)
        out = {"MRR@10": float(rr.mean()), "R@1": float((rank <= 1).mean()), "R@10": float((rank <= 10).mean())}
        if base is None:
            base = rr
        else:
            out["MRR@10_vs_caption"] = {"diff": float((rr - base).mean()), "ci95": boot(rr - base)}
        res[name] = out
    # score-level fusion (cos_cap + w * cos_img), no renormalisation of the summed vector
    for w in (0.25, 0.5):
        S = Q @ cap.T + w * (Q @ img.T)
        tv = S[np.arange(len(qs)), tgt]
        rank = (S > tv[:, None]).sum(1) + 1
        rr = np.where(rank <= 10, 1.0 / rank, 0.0)
        res[f"score cap+{w}img"] = {"MRR@10": float(rr.mean()), "R@1": float((rank <= 1).mean()), "R@10": float((rank <= 10).mean()),
                                    "MRR@10_vs_caption": {"diff": float((rr - base).mean()), "ci95": boot(rr - base)}}
    (a.out / "results.json").write_text(json.dumps(res, indent=1))
    print(json.dumps(res, indent=1))


def main():
    ap = argparse.ArgumentParser()
    sp = ap.add_subparsers(dest="cmd", required=True)
    p = sp.add_parser("prep")
    p.add_argument("--hf", type=Path, required=True)
    p.add_argument("--out", type=Path, required=True)
    p.add_argument("--n", type=int, default=5000)
    s = sp.add_parser("score")
    s.add_argument("--out", type=Path, required=True)
    s.add_argument("--qvec", type=Path, required=True)
    a = ap.parse_args()
    {"prep": cmd_prep, "score": cmd_score}[a.cmd](a)


if __name__ == "__main__":
    sys.exit(main())
