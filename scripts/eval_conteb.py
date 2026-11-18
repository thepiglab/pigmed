"""ConTEB (illuin-conteb, the 8 evaluation sets): EmbeddingGemma 2 with naive chunks vs zero-shot late chunking.

ConTEB's harness ranks every chunk of a dataset for each query and reports nDCG@10 (chunk-level gold). We do the
same, so our numbers are comparable to the paper's per-model tables. Late chunking follows the reference
implementation (illuin-tech/contextual-embeddings): a document's chunks are encoded together in one pass and each
chunk's token span is mean-pooled. Here the chunks are joined with "\n" into one prompted text and the runner pools
each chunk's char span (flashembed `text_late`).

  fetch  --out DATA                         download the parquet files (login node; small)
  prep   --data DATA --out WORK             WORK/<ds>/{naive,late,queries}.jsonl
  score  --work WORK --out RESULTS.json     needs WORK/<ds>/{naive,late,queries}/embeddings.{dense.npy,ids.json}
"""
import argparse
import json
import math
import sys
from pathlib import Path

import numpy as np

DATASETS = {  # name -> split, as in ConTEB's eval collection
    "covid-qa": "train", "geography": "train", "esg-reports": "test", "insurance": "train",
    "football": "train", "squad-conteb-eval": "validation", "narrative-qa": "test", "mldr-conteb-eval": "test",
}
DOC_PROMPT = "title: none | text: "
QUERY_PROMPT = "task: search result | query: "


def cmd_fetch(a):
    from huggingface_hub import hf_hub_download
    for ds, split in DATASETS.items():
        for part in ("documents", "queries"):
            p = hf_hub_download(f"illuin-conteb/{ds}", f"{part}/{split}-00000-of-00001.parquet", repo_type="dataset",
                                local_dir=a.out / ds)
            print(p, flush=True)


def split_id(cid):
    doc, idx = cid.rsplit("_", 1)
    return doc, int(idx)


def cmd_prep(a):
    import pyarrow.parquet as pq
    stats = {}
    for ds, split in DATASETS.items():
        docs = pq.read_table(a.data / ds / "documents" / f"{split}-00000-of-00001.parquet").to_pylist()
        qs = pq.read_table(a.data / ds / "queries" / f"{split}-00000-of-00001.parquet").to_pylist()
        out = a.out / ds
        out.mkdir(parents=True, exist_ok=True)
        by_doc = {}
        for d in docs:
            by_doc.setdefault(split_id(d["chunk_id"])[0], []).append(d)
        with open(out / "naive.jsonl", "w") as fn, open(out / "late.jsonl", "w") as fl:
            for doc, chunks in sorted(by_doc.items()):
                chunks.sort(key=lambda d: split_id(d["chunk_id"])[1])
                text, spans = DOC_PROMPT, []
                for k, c in enumerate(chunks):
                    fn.write(json.dumps({"id": c["chunk_id"], "text": DOC_PROMPT + c["chunk"]}) + "\n")
                    if k:
                        text += "\n"
                    if c["chunk"].strip():  # empty chunks get a zero vector at scoring (as in ConTEB's pooling)
                        spans.append({"id": c["chunk_id"], "start": len(text), "end": len(text) + len(c["chunk"])})
                    text += c["chunk"]
                fl.write(json.dumps({"id": doc, "text": text, "prefix_chars": len(DOC_PROMPT), "spans": spans}) + "\n")
        gold = {}
        with open(out / "queries.jsonl", "w") as fq:
            for i, q in enumerate(qs):
                ids = q.get("chunk_ids") or [q["chunk_id"]]
                gold[f"q{i}"] = ids
                fq.write(json.dumps({"id": f"q{i}", "text": QUERY_PROMPT + q["query"]}) + "\n")
        (out / "gold.json").write_text(json.dumps(gold))
        stats[ds] = {"docs": len(by_doc), "chunks": len(docs), "queries": len(qs),
                     "median_doc_chars": int(np.median([sum(len(c["chunk"]) for c in v) for v in by_doc.values()]))}
        print(ds, json.dumps(stats[ds]), flush=True)
    (a.out / "prep.json").write_text(json.dumps(stats, indent=1))


def load(d: Path):
    ids = json.loads((d / "embeddings.ids.json").read_text())
    ids = ids["ids"] if isinstance(ids, dict) else ids
    v = np.load(d / "embeddings.dense.npy").astype(np.float32)
    assert len(ids) == len(v), (d, len(ids), v.shape)
    return ids, v


def ndcg10(scores, doc_ids, gold):
    order = np.argsort(-scores)[:10]
    rel = set(gold)
    dcg = sum(1 / math.log2(r + 2) for r, j in enumerate(order) if doc_ids[j] in rel)
    idcg = sum(1 / math.log2(r + 2) for r in range(min(len(rel), 10)))
    return dcg / idcg


def cmd_score(a):
    res = {}
    for ds in DATASETS:
        w = a.work / ds
        if not (w / "queries" / "embeddings.dense.npy").exists():
            continue
        gold = json.loads((w / "gold.json").read_text())
        qids, qv = load(w / "queries")
        res[ds] = {}
        per = {}
        all_ids = [json.loads(x)["id"] for x in open(w / "naive.jsonl")]
        for variant in ("naive", "late"):
            if not (w / variant / "embeddings.dense.npy").exists():
                continue
            dids, dv = load(w / variant)
            missing = sorted(set(all_ids) - set(dids))
            if missing:  # empty chunks (no span): zero vector, never retrieved
                dids = list(dids) + missing
                dv = np.concatenate([dv, np.zeros((len(missing), dv.shape[1]), dv.dtype)])
                res[ds][f"{variant}_zero_vectors"] = len(missing)
            sc = qv @ dv.T
            per[variant] = [ndcg10(sc[i], dids, gold[q]) for i, q in enumerate(qids)]
            res[ds][variant] = round(float(np.mean(per[variant])), 4)
        if "naive" in per and "late" in per:
            d = np.array(per["late"]) - np.array(per["naive"])
            rng = np.random.default_rng(0)
            bs = [d[rng.integers(0, len(d), len(d))].mean() for _ in range(2000)]
            res[ds]["late_minus_naive_ci95"] = [round(float(np.percentile(bs, 2.5)), 4), round(float(np.percentile(bs, 97.5)), 4)]
        res[ds]["queries"] = len(qids)
        print(ds, json.dumps(res[ds]), flush=True)
    for v in ("naive", "late"):
        vals = [r[v] for r in res.values() if v in r]
        if vals:
            res.setdefault("_average", {})[v] = round(float(np.mean(vals)), 4)
    a.out.write_text(json.dumps(res, indent=1))
    print(json.dumps(res.get("_average")))


def main():
    ap = argparse.ArgumentParser()
    sp = ap.add_subparsers(dest="cmd", required=True)
    f = sp.add_parser("fetch")
    f.add_argument("--out", type=Path, required=True)
    p = sp.add_parser("prep")
    p.add_argument("--data", type=Path, required=True)
    p.add_argument("--out", type=Path, required=True)
    s = sp.add_parser("score")
    s.add_argument("--work", type=Path, required=True)
    s.add_argument("--out", type=Path, required=True)
    a = ap.parse_args()
    {"fetch": cmd_fetch, "prep": cmd_prep, "score": cmd_score}[a.cmd](a)


if __name__ == "__main__":
    sys.exit(main())
