"""Retrieval fidelity of embedding models on an embedded PigMed corpus (outputs of embed_bundles.py).

Self-supervised tasks built from the corpus itself (no external labels):
  caption->figure   query = a figure's caption; target = that figure's image, among every embedded figure image.
  title->article    query = an article title; target = any text chunk of that article, among every embedded text
                    chunk (documents were embedded without titles, so the title is not copied into the chunk).
  figure->caption   query = a figure image's vector; target = its caption chunk, among every caption chunk.
Every model is scored on the same queries and the same corpus units (items both models embedded).
For NeoMME, bundles embedded with late vectors also get dense top-100 -> late MaxSim rerank on that subset.

usage:
  python -I scripts/eval_retrieval.py queries --vectors V --out Q [--n 2000]
     -> Q/queries.json and per-model query inputs Q/<model>.jsonl (embed them with flashembed, query side)
  python -I scripts/eval_retrieval.py score --vectors V --queries Q --models gemma,neomme --out R
"""
from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import math
import sys
from pathlib import Path

import numpy as np


def h(s: str) -> int:
    return int(hashlib.sha256(s.encode()).hexdigest()[:12], 16)


def load_units(vectors: Path):
    units = {}
    for f in sorted((vectors / "units").glob("*.jsonl.gz")):
        with gzip.open(f, "rt") as fh:
            for line in fh:
                r = json.loads(line)
                r["bundle"] = f.name[: -len(".jsonl.gz")]
                units[r["unit_id"]] = r
    return units


def cmd_queries(a):
    units = load_units(a.vectors)
    done = {m: {p.name for p in (a.vectors / m).iterdir() if (p / "meta.json").exists()} for m in ("gemma", "neomme")}
    both = done["gemma"] & done["neomme"]
    titles, figs, caps = {}, [], {}
    for u in units.values():
        if u["bundle"] not in both:
            continue
        if u["kind"] == "article" and u.get("title") and len(u["title"].split()) >= 4:
            titles[u["article_id"]] = u["title"]
        elif u["kind"] == "figure" and u.get("caption") and len(u["caption"].split()) >= 5:
            figs.append(u)
        elif u["kind"] == "caption":
            caps[(u["article_id"], u.get("figure_id"))] = u["unit_id"]
    figs.sort(key=lambda u: h(u["unit_id"]))
    arts = sorted(titles, key=h)
    q = []
    for u in figs[: a.n]:
        q.append({"qid": "c2f-" + u["unit_id"], "task": "caption->figure", "text": u["caption"][:2000],
                  "target": u["unit_id"], "article_id": u["article_id"], "bundle": u["bundle"],
                  "caption_unit": caps.get((u["article_id"], u.get("figure_id")))})
    for aid in arts[: a.n]:
        q.append({"qid": "t2a-" + aid, "task": "title->article", "text": titles[aid], "target_article": aid,
                  "bundle": next(u["bundle"] for u in units.values() if u["article_id"] == aid and u["kind"] == "article")})
    a.out.mkdir(parents=True, exist_ok=True)
    (a.out / "queries.json").write_text(json.dumps(q))
    with open(a.out / "gemma.jsonl", "w") as f:
        for x in q:
            f.write(json.dumps({"id": x["qid"], "text": f"task: search result | query: {x['text']}"}) + "\n")
    with open(a.out / "neomme.jsonl", "w") as f:
        for x in q:
            f.write(json.dumps({"id": x["qid"], "text": x["text"]}) + "\n")
    print(json.dumps({"queries": len(q), "caption->figure": sum(x["task"] == "caption->figure" for x in q),
                      "title->article": sum(x["task"] == "title->article" for x in q), "bundles_both_models": len(both)}))


class Corpus:
    def __init__(self, vectors: Path, model: str, bundles):
        dense, ids, mods, self.late_bundles = [], [], [], {}
        for b in sorted(bundles):
            d = vectors / model / b
            v = np.load(d / "dense.npy")
            j = json.loads((d / "ids.json").read_text())
            if len(v):
                dense.append(v)
                ids += j["unit_id"]
                mods += j["modality"]
            if (d / "late.npy").exists():
                self.late_bundles[b] = d
        self.dense = np.concatenate(dense).astype(np.float32)
        self.ids = np.array(ids)
        self.mods = np.array(mods)
        self.index = {u: i for i, u in enumerate(ids)}

    def late_for(self, unit_ids):
        """{unit_id: [T,128] float32} for units inside late-sample bundles."""
        out = {}
        for b, d in self.late_bundles.items():
            j = json.loads((d / "ids.json").read_text())["unit_id"]
            want = [i for i, u in enumerate(j) if u in unit_ids]
            if not want:
                continue
            lv = np.load(d / "late.npy", mmap_mode="r")
            off = np.load(d / "late_offsets.npy")
            for i in want:
                out[j[i]] = np.asarray(lv[off[i]: off[i + 1]], dtype=np.float32)
        return out


def topk(qv: np.ndarray, docs: np.ndarray, k: int):
    out_i, out_s = [], []
    for s in range(0, len(qv), 256):
        sc = qv[s: s + 256] @ docs.T
        idx = np.argpartition(-sc, min(k, sc.shape[1] - 1), axis=1)[:, :k]
        ss = np.take_along_axis(sc, idx, 1)
        o = np.argsort(-ss, 1)
        out_i.append(np.take_along_axis(idx, o, 1))
        out_s.append(np.take_along_axis(ss, o, 1))
    return np.concatenate(out_i), np.concatenate(out_s)


def metrics(ranks):
    """ranks: 1-based rank of the first relevant item or None if not in the top 100."""
    n = len(ranks)
    r = [x if x is not None else math.inf for x in ranks]
    rr = [1 / x if x <= 10 else 0.0 for x in r]
    return {"n": n, "R@1": sum(x <= 1 for x in r) / n, "R@10": sum(x <= 10 for x in r) / n,
            "R@100": sum(x <= 100 for x in r) / n, "MRR@10": sum(rr) / n,
            "nDCG@10": sum(1 / math.log2(x + 1) if x <= 10 else 0.0 for x in r) / n}


def boot_diff(a, b, iters=2000, seed=0):
    """Paired bootstrap 95% CI of mean(a - b) for per-query reciprocal ranks."""
    rng = np.random.default_rng(seed)
    d = np.asarray(a) - np.asarray(b)
    m = [d[rng.integers(0, len(d), len(d))].mean() for _ in range(iters)]
    return [float(np.percentile(m, 2.5)), float(np.percentile(m, 97.5))]


def maxsim(q: np.ndarray, d: np.ndarray) -> float:
    return float((q @ d.T).max(1).sum())


def cmd_score(a):
    qs = json.loads((a.queries / "queries.json").read_text())
    models = a.models.split(",")
    units = load_units(a.vectors)
    bundles = set.intersection(*[{p.name for p in (a.vectors / m).iterdir() if (p / "meta.json").exists()} for m in models])
    corp = {m: Corpus(a.vectors, m, bundles) for m in models}
    shared = set(corp[models[0]].index)
    for m in models[1:]:
        shared &= set(corp[m].index)
    res, per_query = {"corpus_bundles": len(bundles), "tasks": {}}, {}
    for m in models:
        c = corp[m]
        qd = a.queries / f"out-{m}"
        qv = np.load(qd / "embeddings.dense.npy").astype(np.float32)
        qids = json.loads((qd / "embeddings.ids.json").read_text())
        qids = qids["ids"] if isinstance(qids, dict) else qids
        qi = {q: i for i, q in enumerate(qids)}
        keep = np.array([u in shared for u in c.ids])
        art = np.array([units.get(u, {}).get("article_id") for u in c.ids])
        kind = np.array([units.get(u, {}).get("kind") for u in c.ids])
        pools = {"caption->figure": keep & (c.mods == "image"),
                 "title->article": keep & (c.mods == "text") & (kind != "caption"),
                 "figure->caption": keep & (kind == "caption")}
        for task, pool in pools.items():
            pidx = np.where(pool)[0]
            docs = c.dense[pidx]
            if task == "figure->caption":
                tq = [q for q in qs if q["task"] == "caption->figure" and q.get("caption_unit") in c.index
                      and q["target"] in c.index and q["caption_unit"] in shared]
                qvec = c.dense[[c.index[q["target"]] for q in tq]]
                targets = [q["caption_unit"] for q in tq]
            else:
                tq = [q for q in qs if q["task"] == task and q["qid"] in qi and (task != "caption->figure" or q["target"] in shared)]
                qvec = qv[[qi[q["qid"]] for q in tq]]
                targets = [q.get("target") or q.get("target_article") for q in tq]
            if not tq:
                continue
            I, S = topk(qvec, docs, 100)
            ranks = []
            for row, t in zip(I, targets):
                hit = None
                if task == "title->article":
                    seen = []
                    for j in row:
                        aj = art[pidx[j]]
                        if aj not in seen:
                            seen.append(aj)
                        if aj == t:
                            hit = len(seen)
                            break
                else:
                    for r, j in enumerate(row, 1):
                        if c.ids[pidx[j]] == t:
                            hit = r
                            break
                ranks.append(hit)
            res["tasks"].setdefault(task, {})[m] = {**metrics(ranks), "corpus_size": int(len(pidx))}
            per_query[(task, m)] = {q["qid"]: (1 / r if r and r <= 10 else 0.0) for q, r in zip(tq, ranks)}
            # NeoMME late-interaction rerank of the dense top-100, on queries whose target is in a late bundle
            if m == "neomme" and c.late_bundles and task != "figure->caption" and (qd / "embeddings.late.npy").exists():
                ql = np.load(qd / "embeddings.late.npy").astype(np.float32)
                qo = np.load(qd / "embeddings.late_offsets.npy")
                if len(qo) == len(qids):
                    qo = np.append(qo, len(ql))
                sub = [(n, q) for n, q in enumerate(tq) if q["bundle"] in c.late_bundles]
                lpool = np.array([units.get(c.ids[p], {}).get("bundle") in c.late_bundles for p in pidx])
                lidx = np.where(lpool)[0]
                if sub and len(lidx):
                    Il, _ = topk(qvec[[n for n, _ in sub]], docs[lidx], 100)
                    need = {c.ids[pidx[lidx[j]]] for row in Il for j in row}
                    lat = c.late_for(need)
                    dr, lr = [], []
                    for (n, q), row in zip(sub, Il):
                        qlate = ql[qo[qi[q["qid"]]]: qo[qi[q["qid"]] + 1]]
                        cand = [c.ids[pidx[lidx[j]]] for j in row]
                        scored = sorted(cand, key=lambda u: -maxsim(qlate, lat[u]) if u in lat else math.inf)
                        t = targets[n]
                        for lst, out in ((cand, dr), (scored, lr)):
                            hit = None
                            if task == "title->article":
                                seen = []
                                for u in lst:
                                    au = units[u]["article_id"]
                                    if au not in seen:
                                        seen.append(au)
                                    if au == t:
                                        hit = len(seen)
                                        break
                            else:
                                hit = next((r for r, u in enumerate(lst, 1) if u == t), None)
                            out.append(hit)
                    res["tasks"][task]["neomme-late-subset"] = {"dense": metrics(dr), "late_rerank": metrics(lr),
                                                                "corpus_size": int(len(lidx))}
    for task in res["tasks"]:
        if all((task, m) in per_query for m in models) and len(models) == 2:
            a0, a1 = per_query[(task, models[0])], per_query[(task, models[1])]
            common = sorted(set(a0) & set(a1))
            res["tasks"][task]["paired_MRR@10_diff"] = {
                "models": f"{models[0]} - {models[1]}", "n": len(common),
                "mean": float(np.mean([a0[q] - a1[q] for q in common])),
                "ci95": boot_diff([a0[q] for q in common], [a1[q] for q in common])}
    a.out.mkdir(parents=True, exist_ok=True)
    (a.out / "results.json").write_text(json.dumps(res, indent=2))
    print(json.dumps(res, indent=2))


def main():
    ap = argparse.ArgumentParser()
    sp = ap.add_subparsers(dest="cmd", required=True)
    q = sp.add_parser("queries")
    q.add_argument("--vectors", type=Path, required=True)
    q.add_argument("--out", type=Path, required=True)
    q.add_argument("--n", type=int, default=2000)
    s = sp.add_parser("score")
    s.add_argument("--vectors", type=Path, required=True)
    s.add_argument("--queries", type=Path, required=True)
    s.add_argument("--models", default="gemma")
    s.add_argument("--out", type=Path, required=True)
    a = ap.parse_args()
    {"queries": cmd_queries, "score": cmd_score}[a.cmd](a)


if __name__ == "__main__":
    sys.exit(main())
