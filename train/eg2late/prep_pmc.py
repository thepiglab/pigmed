"""PMC training sources for S1 from retriever-data's pair files + the 8-shard candidate corpus (pmc_corpus.py).

  corpus  --pmc8 DIR --manifest articles.jsonl.gz --out DATA/pmc_corpus
          keep text units of train + commercial + not-retracted articles; docs.jsonl {"id","text","group"} and
          dvec/ (published EG2 vectors, same ids) = the mining pool shared by every PMC source
  pairs   --pairs title.jsonl.gz --name pmc_title --corpus DATA/pmc_corpus --out DATA
          DATA/<name>/queries.jsonl (prompted), pairs.jsonl {"q","pos","gold","group"}; docs.jsonl and dvec are
          symlinks to the corpus; file `corpus` names the shared corpus (id namespace for the trainer's masks)
  compact --src DATA/<name>
          after mining: a memory-mapped store of the docs the triples use (positives + negatives):
          docs.blob / docs.off.npy (int64 [n+1]) / docs.ids.json / docs.groups.json, plus docs_used.jsonl (flashembed input for the
          teacher vectors tvec/ = base EG2 on exactly the model text the trainer sees)
Pair rows from retriever-data: {"qid","query","pos":[unit_id..],"gold":[unit_id..],"article_id","group"}.
"""
from __future__ import annotations

import argparse
import gzip
import json
import os
from pathlib import Path

import numpy as np

from .model import QUERY_PROMPT


def cmd_corpus(a):
    out = a.out
    (out / "dvec").mkdir(parents=True, exist_ok=True)
    shards = sorted(p.name[: -len(".jsonl.gz")] for p in (a.pmc8 / "docs").glob("*.jsonl.gz"))
    arts = set()
    for s in shards:
        for line in gzip.open(a.pmc8 / "docs" / f"{s}.jsonl.gz", "rt"):
            arts.add(json.loads(line)["article_id"])
    meta = {}
    for line in gzip.open(a.manifest, "rt"):
        r = json.loads(line)
        if r["article_id"] in arts:
            meta[r["article_id"]] = r
    ok = {k: v["group"] for k, v in meta.items()
          if v["split"] == "train" and v.get("commercial") and not v.get("retracted")}
    ids, vecs, n_in = [], [], 0
    with open(out / "docs.jsonl", "w") as f:
        for s in shards:
            v = np.load(a.pmc8 / "vec" / f"{s}.npy", mmap_mode="r")
            keep = []
            for i, line in enumerate(gzip.open(a.pmc8 / "docs" / f"{s}.jsonl.gz", "rt")):
                n_in += 1
                r = json.loads(line)
                g = ok.get(r["article_id"])
                if g is None:
                    continue
                f.write(json.dumps({"id": r["id"], "text": r["text"], "group": g, "abs": r["abs"],
                                    "article_id": r["article_id"]}) + "\n")
                ids.append(r["id"])
                keep.append(i)
            vecs.append(np.asarray(v[keep]))
    np.save(out / "dvec" / "embeddings.dense.npy", np.concatenate(vecs))
    (out / "dvec" / "embeddings.ids.json").write_text(json.dumps(ids))
    st = {"units_in": n_in, "units_kept": len(ids), "articles_in": len(arts), "articles_in_manifest": len(meta),
          "articles_kept": len(ok)}
    (out / "corpus_stats.json").write_text(json.dumps(st, indent=1))
    print(json.dumps(st))


def cmd_pairs(a):
    out = a.out / a.name
    out.mkdir(parents=True, exist_ok=True)
    ids = set(json.loads((a.corpus / "dvec" / "embeddings.ids.json").read_text()))
    n = kept = 0
    with open(out / "queries.jsonl", "w") as fq, open(out / "pairs.jsonl", "w") as fp:
        for line in gzip.open(a.pairs, "rt"):
            n += 1
            r = json.loads(line)
            pos = [u for u in r["pos"] if u in ids]
            if not pos:
                continue
            gold = sorted(set(r.get("gold") or []) | set(r["pos"]))
            fq.write(json.dumps({"id": r["qid"], "text": QUERY_PROMPT + r["query"]}) + "\n")
            fp.write(json.dumps({"q": r["qid"], "pos": pos[0], "gold": gold, "group": r["group"]}) + "\n")
            kept += 1
    for x in ("docs.jsonl", "dvec"):
        p = out / x
        if not (p.exists() or p.is_symlink()):
            os.symlink(os.path.relpath(a.corpus / x, out), p)
    (out / "corpus").write_text(a.corpus.name)
    print(json.dumps({"source": a.name, "pairs_in": n, "pairs_kept": kept}))


def cmd_compact(a):
    s = a.src
    used = set()
    for line in open(s / "triples.jsonl"):
        t = json.loads(line)
        used.add(t["pos"])
        used.update(t["negs"])
    ids, groups = [], []
    off = [0]
    with open(s / "docs.blob", "wb") as fb, open(s / "docs_used.jsonl", "w") as fu:
        for line in open(s / "docs.jsonl"):
            r = json.loads(line)
            if r["id"] not in used:
                continue
            b = r["text"].encode()
            fb.write(b)
            off.append(off[-1] + len(b))
            ids.append(r["id"])
            groups.append(r.get("group"))
            fu.write(json.dumps({"id": r["id"], "text": r["text"]}) + "\n")
    np.save(s / "docs.off.npy", np.array(off, dtype=np.int64))
    (s / "docs.ids.json").write_text(json.dumps(ids))
    (s / "docs.groups.json").write_text(json.dumps(groups))
    print(json.dumps({"source": s.name, "docs_used": len(ids), "blob_gb": round(off[-1] / 1e9, 2)}))


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("corpus")
    c.add_argument("--pmc8", type=Path, required=True)
    c.add_argument("--manifest", type=Path, required=True)
    c.add_argument("--out", type=Path, required=True)
    p = sub.add_parser("pairs")
    p.add_argument("--pairs", type=Path, required=True)
    p.add_argument("--name", required=True)
    p.add_argument("--corpus", type=Path, required=True)
    p.add_argument("--out", type=Path, required=True)
    k = sub.add_parser("compact")
    k.add_argument("--src", type=Path, required=True)
    a = ap.parse_args()
    {"corpus": cmd_corpus, "pairs": cmd_pairs, "compact": cmd_compact}[a.cmd](a)


if __name__ == "__main__":
    main()
