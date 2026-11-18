"""Score chunking variants (scripts/chunk_variants.py) on article-level retrieval.

Tasks (2,000-3,000 queries each, the same queries for every variant):
  abstract->body   query = one sentence (12-45 words) from the article's abstract; corpus = every variant's
                   NON-abstract text units; hit = rank of the first unit from the source article. No variant
                   puts abstract text in the corpus, and the title never appears in the query, so this task is
                   fair to variants that add the title to the chunk prompt.
  title->article   query = the article title; corpus = all text units. Variants B/CB/D put the title in the
                   prompt, so this task is LEAKY for them (reported, flagged, not used to pick a winner).

  queries  --variant-root DIR(A's root) --out Q --n 3000      -> Q/queries.json + Q/queries.jsonl (Gemma query prompt)
  score    --q Q --qvec DIR(embeddings.*) --variant NAME=ROOT:VECTORS ... --out R
"""
import argparse
import gzip
import hashlib
import json
import math
import re
import sys
from pathlib import Path

import numpy as np

SENT_SPLIT = re.compile(r"(?<=[.!?])\s+(?=[A-Z0-9(])")


def h(s):
    return int(hashlib.sha256(s.encode()).hexdigest()[:12], 16)


def iter_units(root: Path):
    for f in sorted(root.glob("runs/*/ready/*/items.jsonl.gz")):
        for line in gzip.open(f, "rt"):
            r = json.loads(line)
            if r.get("kind") in ("text", "text_meta"):
                yield r


def cmd_queries(a):
    titles, abstracts, has_body = {}, {}, set()
    for r in iter_units(a.variant_root):
        aid = r["article_id"]
        titles[aid] = r.get("title") or ""
        if r.get("is_abstract"):
            abstracts.setdefault(aid, []).append(r["text"])
        else:
            has_body.add(aid)
    q = []
    arts = sorted((x for x in abstracts if x in has_body), key=h)
    for aid in arts:
        sents = [s.strip() for t in abstracts[aid] for s in SENT_SPLIT.split(t)]
        sents = [s for s in sents if 12 <= len(s.split()) <= 45 and not s.lower().startswith(("background", "objective"))]
        if not sents:
            continue
        s = sorted(sents, key=lambda x: h(aid + x))[0]
        q.append({"qid": "abs-" + aid, "task": "abstract->body", "text": s, "article_id": aid})
        if len(q) >= a.n:
            break
    for aid in sorted((x for x in titles if x in has_body and len(titles[x].split()) >= 4), key=h)[: a.n]:
        q.append({"qid": "title-" + aid, "task": "title->article", "text": titles[aid], "article_id": aid})
    a.out.mkdir(parents=True, exist_ok=True)
    (a.out / "queries.json").write_text(json.dumps(q))
    with open(a.out / "queries.jsonl", "w") as f:
        for x in q:
            f.write(json.dumps({"id": x["qid"], "text": "task: search result | query: " + x["text"]}) + "\n")
    print(json.dumps({"queries": len(q), "abstract->body": sum(x["task"] == "abstract->body" for x in q),
                      "title->article": sum(x["task"] == "title->article" for x in q)}))


def load_corpus(root: Path, vectors: Path):
    meta = {}
    for r in iter_units(root):
        meta[r["unit_id"]] = (r["article_id"], bool(r.get("is_abstract")))
    vecs, arts, abst = [], [], []
    for d in sorted((vectors / "gemma").glob("*/meta.json")):
        ids = json.loads((d.parent / "ids.json").read_text())
        v = np.load(d.parent / "dense.npy")
        keep = [i for i, (u, m) in enumerate(zip(ids["unit_id"], ids["modality"])) if m == "text" and u in meta]
        if not keep:
            continue
        vecs.append(v[keep].astype(np.float32))
        for i in keep:
            aid, ab = meta[ids["unit_id"][i]]
            arts.append(aid)
            abst.append(ab)
    return np.concatenate(vecs), np.array(arts), np.array(abst)


def metrics(ranks):
    r = np.array([x if x is not None else math.inf for x in ranks], dtype=float)
    return {"n": len(r), "R@1": float(np.mean(r <= 1)), "R@10": float(np.mean(r <= 10)), "R@100": float(np.mean(r <= 100)),
            "MRR@10": float(np.mean(np.where(r <= 10, 1 / r, 0.0)))}


def article_ranks(qv, docs, arts, targets, k=1000):
    out = []
    for s in range(0, len(qv), 256):
        sc = qv[s: s + 256] @ docs.T
        idx = np.argpartition(-sc, min(k, sc.shape[1] - 1), axis=1)[:, :k]
        o = np.argsort(-np.take_along_axis(sc, idx, 1), 1)
        idx = np.take_along_axis(idx, o, 1)
        for row, t in zip(idx, targets[s: s + 256]):
            seen, hit = [], None
            for j in row:
                aj = arts[j]
                if aj not in seen:
                    seen.append(aj)
                    if aj == t:
                        hit = len(seen)
                        break
                    if len(seen) >= 100:
                        break
            out.append(hit)
    return out


def boot(a, b, n=2000):
    d = np.asarray(a) - np.asarray(b)
    rng = np.random.default_rng(0)
    m = [d[rng.integers(0, len(d), len(d))].mean() for _ in range(n)]
    return [float(np.percentile(m, 2.5)), float(np.percentile(m, 97.5))]


def cmd_score(a):
    qs = json.loads((a.q / "queries.json").read_text())
    qids = json.loads((a.qvec / "embeddings.ids.json").read_text())
    qids = qids["ids"] if isinstance(qids, dict) else qids
    qv = np.load(a.qvec / "embeddings.dense.npy").astype(np.float32)
    qi = {x: i for i, x in enumerate(qids)}
    res, rr = {"tasks": {}}, {}
    for spec in a.variant:
        name, rest = spec.split("=", 1)
        root, vec = rest.split(":", 1)
        docs, arts, abst = load_corpus(Path(root), Path(vec))
        res.setdefault("corpus", {})[name] = {"units": int(len(arts)), "abstract_units": int(abst.sum())}
        for task in ("abstract->body", "title->article"):
            tq = [x for x in qs if x["task"] == task and x["qid"] in qi]
            pool = ~abst if task == "abstract->body" else np.ones(len(arts), bool)
            ranks = article_ranks(qv[[qi[x["qid"]] for x in tq]], docs[pool], arts[pool], np.array([x["article_id"] for x in tq]))
            m = metrics(ranks)
            m["corpus_units"] = int(pool.sum())
            if task == "title->article" and name in ("B", "CB", "D"):
                m["leaky"] = "title is in this variant's document prompt"
            res["tasks"].setdefault(task, {})[name] = m
            rr[(task, name)] = [1 / x if x and x <= 10 else 0.0 for x in ranks]
        print(name, json.dumps({t: res["tasks"][t][name] for t in res["tasks"]}), flush=True)
    base = a.variant[0].split("=", 1)[0]
    for task in res["tasks"]:
        for name in res["tasks"][task]:
            if name != base:
                d = np.mean(rr[(task, name)]) - np.mean(rr[(task, base)])
                res["tasks"][task][name][f"MRR@10_vs_{base}"] = {"diff": float(d), "ci95": boot(rr[(task, name)], rr[(task, base)])}
    a.out.mkdir(parents=True, exist_ok=True)
    (a.out / "results.json").write_text(json.dumps(res, indent=1))
    print(json.dumps(res, indent=1))


def main():
    ap = argparse.ArgumentParser()
    sp = ap.add_subparsers(dest="cmd", required=True)
    q = sp.add_parser("queries")
    q.add_argument("--variant-root", type=Path, required=True)
    q.add_argument("--out", type=Path, required=True)
    q.add_argument("--n", type=int, default=3000)
    s = sp.add_parser("score")
    s.add_argument("--q", type=Path, required=True)
    s.add_argument("--qvec", type=Path, required=True)
    s.add_argument("--variant", action="append", required=True, help="NAME=ROOT:VECTORS (first is the baseline)")
    s.add_argument("--out", type=Path, required=True)
    a = ap.parse_args()
    {"queries": cmd_queries, "score": cmd_score}[a.cmd](a)


if __name__ == "__main__":
    sys.exit(main())
