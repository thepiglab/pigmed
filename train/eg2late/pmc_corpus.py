"""PMC candidate corpus for S1 mining/training, from published shards of mkieffer/pigmed-pmc-eg2 (recipe v2).

For each shard: download chunks/<s>.parquet and vectors/<s>.parquet, keep commercial-use `text` units (abstract and
body chunks), write
  <out>/docs/<s>.jsonl.gz     {"id": unit_id, "article_id", "abs": bool, "text": prompted model text}
  <out>/vec/<s>.npy           [n, 768] float16, the published EG2 vector of each kept unit, same order
then delete the downloads. The model text follows the card: body chunks "title: none | text: <top heading>\\n<chunk>",
other units "title: none | text: <text>". (Published body vectors mix in a late-chunking vector; fine for mining.)

python -m eg2late.pmc_corpus --shards pmcv2-c31-00008,... --out DIR
"""
import argparse
import gzip
import json
import os
from pathlib import Path

import numpy as np
import pyarrow.compute as pc
import pyarrow.parquet as pq

from .model import DOC_PROMPT

REPO = "mkieffer/pigmed-pmc-eg2"


def one(shard: str, out: Path, tmp: Path):
    from huggingface_hub import hf_hub_download
    done = out / "docs" / f"{shard}.done"
    if done.exists():
        return json.loads(done.read_text())
    cp = hf_hub_download(REPO, f"chunks/{shard}.parquet", repo_type="dataset", local_dir=tmp)
    t = pq.read_table(cp, columns=["unit_id", "article_id", "kind", "text", "section", "is_abstract",
                                   "commercial_use_ok"])
    n_all = t.num_rows
    t = t.filter(pc.and_(pc.equal(t["kind"], "text"), t["commercial_use_ok"]))
    ids = t["unit_id"].to_pylist()
    keep = {u: i for i, u in enumerate(ids)}
    rows = t.to_pylist()
    os.remove(cp)
    vp = hf_hub_download(REPO, f"vectors/{shard}.parquet", repo_type="dataset", local_dir=tmp)
    vec = np.zeros((len(ids), 768), dtype=np.float16)
    got = np.zeros(len(ids), dtype=bool)
    pf = pq.ParquetFile(vp)
    for g in range(pf.metadata.num_row_groups):
        v = pf.read_row_group(g, columns=["unit_id", "modality", "embedding"])
        v = v.filter(pc.equal(v["modality"], "text"))
        uid = v["unit_id"].to_pylist()
        emb = v["embedding"].combine_chunks().flatten().to_numpy(zero_copy_only=False).reshape(-1, 768)
        for j, u in enumerate(uid):
            i = keep.get(u)
            if i is not None:
                vec[i] = emb[j]
                got[i] = True
    os.remove(vp)
    (out / "docs").mkdir(parents=True, exist_ok=True)
    (out / "vec").mkdir(parents=True, exist_ok=True)
    with gzip.open(out / "docs" / f"{shard}.jsonl.gz", "wt") as f:
        for r, ok in zip(rows, got):
            if not ok:
                continue
            sec = r["section"] or []
            body = (not r["is_abstract"]) and sec
            text = DOC_PROMPT + (f"{sec[0]}\n{r['text']}" if body else r["text"])
            f.write(json.dumps({"id": r["unit_id"], "article_id": r["article_id"], "abs": bool(r["is_abstract"]),
                                "text": text}) + "\n")
    np.save(out / "vec" / f"{shard}.npy", vec[got])
    stats = {"shard": shard, "rows": n_all, "kept": int(got.sum()), "no_vector": int((~got).sum()),
             "articles": len({r["article_id"] for r in rows})}
    done.write_text(json.dumps(stats))
    return stats


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--shards", required=True)
    ap.add_argument("--out", type=Path, required=True)
    a = ap.parse_args()
    tmp = a.out / "_dl"
    tmp.mkdir(parents=True, exist_ok=True)
    for s in a.shards.split(","):
        print(json.dumps(one(s, a.out, tmp)), flush=True)


if __name__ == "__main__":
    main()
