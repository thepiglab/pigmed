"""Hard-negative mining with base EG2 (NV-Retriever "TopK-PercPos" rule), GPU top-k over flashembed vectors.

  mine --src DATA/<source> [--top 256] [--perc 0.95] [--keep 15]

Reads <src>/pairs.jsonl and the base-EG2 vectors <src>/qvec, <src>/dvec (flashembed `embed` output:
embeddings.dense.npy + embeddings.ids.json, inputs = queries.jsonl / docs.jsonl). For each query: candidates = top
`--top` documents by cosine; drop gold documents, candidates scoring above perc x the positive's score (likely false
negatives), candidates containing an answer string (SQuAD / HotpotQA), and candidates in the query's paper group
(PMC: pairs carry "group", docs.jsonl rows carry "group"); keep the first `--keep` as `negs` (the trainer samples
n_neg of them). Writes <src>/triples.jsonl and <src>/mine_stats.json. Scores stay fp16 on the GPU; the query block
size adapts to the corpus size.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path

import numpy as np
import torch


def load(d: Path):
    ids = json.loads((d / "embeddings.ids.json").read_text())
    ids = ids["ids"] if isinstance(ids, dict) else ids
    return ids, np.load(d / "embeddings.dense.npy")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--src", type=Path, required=True)
    ap.add_argument("--top", type=int, default=256)
    ap.add_argument("--perc", type=float, default=0.95)
    ap.add_argument("--keep", type=int, default=15)
    a = ap.parse_args()
    s = a.src
    pairs = [json.loads(x) for x in open(s / "pairs.jsonl")]
    qids, qv = load(s / "qvec")
    dids, dv = load(s / "dvec")
    qi = {x: i for i, x in enumerate(qids)}
    di = {x: i for i, x in enumerate(dids)}
    dtext, dgroup = {}, {}
    need_text = any(p.get("answers") for p in pairs)
    need_group = any(p.get("group") for p in pairs)
    if need_text or need_group:
        # S3 figure sources have figures.jsonl (id, group, caption) instead of docs.jsonl
        for line in open(s / "docs.jsonl" if (s / "docs.jsonl").exists() else s / "figures.jsonl"):
            r = json.loads(line)
            if need_text:
                dtext[r["id"]] = (r.get("text") or r.get("caption") or "").lower()
            if need_group and r.get("group"):
                dgroup[r["id"]] = r["group"]
    dev = "cuda" if torch.cuda.is_available() else "cpu"
    D = torch.from_numpy(dv).to(dev, torch.float16)
    pairs = [p for p in pairs if p["q"] in qi and p["pos"] in di]
    stats = {"pairs": len(pairs), "dropped_above_perc": 0, "dropped_answer": 0, "dropped_group": 0, "short": 0,
             "pos_rank": []}
    qb = max(16, min(2048, int(1.5e9 // max(1, D.shape[0]))))
    out = open(s / "triples.jsonl", "w")
    for b in range(0, len(pairs), qb):
        grp = pairs[b:b + qb]
        Q = torch.from_numpy(qv[[qi[p["q"]] for p in grp]]).to(dev, torch.float16)
        sc = Q @ D.T  # fp16 [qb, N]
        pos = torch.tensor([di[p["pos"]] for p in grp], device=dev)
        ps = sc.gather(1, pos[:, None]).squeeze(1)
        stats["pos_rank"] += ((sc > ps[:, None]).sum(1) + 1).cpu().tolist()
        vals, idx = torch.topk(sc, min(a.top, sc.shape[1]), dim=1)
        vals, idx, ps = vals.float().cpu().numpy(), idx.cpu().numpy(), ps.float().cpu().numpy()
        del sc
        for p, v, ix, pscore in zip(grp, vals, idx, ps):
            gold = set(p.get("gold") or []) | {p["pos"]}
            ans = [x.lower() for x in p.get("answers") or []]
            qg = p.get("group")
            negs = []
            for score, j in zip(v, ix):
                d = dids[j]
                if d in gold:
                    continue
                if score > a.perc * pscore:
                    stats["dropped_above_perc"] += 1
                    continue
                if qg and dgroup.get(d) == qg:
                    stats["dropped_group"] += 1
                    continue
                if ans and any(x in dtext[d] for x in ans):
                    stats["dropped_answer"] += 1
                    continue
                negs.append(d)
                if len(negs) >= a.keep:
                    break
            if len(negs) < 7:
                stats["short"] += 1
            p = dict(p)
            p["negs"] = negs
            p.pop("answers", None)
            out.write(json.dumps(p) + "\n")
    out.close()
    r = np.array(stats.pop("pos_rank"))
    stats.update({"pos_R@1": float(np.mean(r <= 1)), "pos_R@10": float(np.mean(r <= 10)),
                  "pos_MRR@10": float(np.mean(np.where(r <= 10, 1 / r, 0))), "docs": len(dids)})
    (s / "mine_stats.json").write_text(json.dumps(stats, indent=1))
    print(s.name, json.dumps(stats), flush=True)


if __name__ == "__main__":
    main()
