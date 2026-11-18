"""Build S1 smoke-run training sources from upstream, commercial-use text (no LightOn copies, no mined negatives).

  fetch --out RAW            download parquet files (small; NanoBEIR-en for eval too)
  build --raw RAW --out DATA [--max-per-source N]

Sources (licence, see docs/RETRIEVER_DATA_PLAN.md):
  nq        sentence-transformers/natural-questions `pair` (NQ train, CC BY-SA 3.0): query -> Wikipedia passage
  hotpotqa  hotpotqa/hotpot_qa distractor train (CC BY-SA 4.0): question -> one supporting paragraph; both
            supporting paragraphs are gold (masked as negatives); pool = every context paragraph
  squad     rajpurkar/squad_v2 train (CC BY-SA 4.0), answerable only, <= 3 questions per context: question -> context

Extra sources (ablation, user OK 2026-11-17 "try them out"): fetch-extra / build-extra
  fever     BeIR/fever (FEVER claims -> Wikipedia intro paragraphs; FEVER CC BY-SA 3.0, BEIR packaging tagged
            CC BY-SA 4.0): train qrels (SUPPORTS + REFUTES claims); pool = all train positives + a 1M stable-hash
            sample of the 5.4M-doc corpus
  miracl    miracl/miracl en train (Apache-2.0 annotations) over miracl/miracl-corpus en (Wikipedia, CC BY-SA 3.0):
            judged-relevant passages are positives (all gold); pool = every judged passage + a 500k sample

Output per source: queries.jsonl / docs.jsonl (prompted text, as flashembed and the trainer read them) and
pairs.jsonl {"q","pos","gold","answers"} (negatives are added by mine.py).
Documents are "<title> <text>" under EG2's document prompt, as in sentence-transformers' NanoBEIR evaluator.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import random
from pathlib import Path

from .model import DOC_PROMPT, QUERY_PROMPT

FILES = {
    "sentence-transformers/natural-questions": ["pair/train-00000-of-00001.parquet"],
    "hotpotqa/hotpot_qa": ["distractor/train-00000-of-00002.parquet", "distractor/train-00001-of-00002.parquet"],
    "rajpurkar/squad_v2": ["squad_v2/train-00000-of-00001.parquet"],
}
NANO_REPO = "sentence-transformers/NanoBEIR-en"


def hid(s: str) -> str:
    return hashlib.sha1(s.encode()).hexdigest()[:16]


def cmd_fetch(a):
    from huggingface_hub import hf_hub_download, snapshot_download
    for repo, files in FILES.items():
        for f in files:
            print(hf_hub_download(repo, f, repo_type="dataset", local_dir=a.out / repo.replace("/", "__")), flush=True)
    print(snapshot_download(NANO_REPO, repo_type="dataset", local_dir=a.out / "nanobeir",
                            allow_patterns=["corpus/*", "queries/*", "qrels/*"]), flush=True)


def write(out: Path, queries: dict, docs: dict, pairs: list):
    out.mkdir(parents=True, exist_ok=True)
    with open(out / "queries.jsonl", "w") as f:
        for k, v in queries.items():
            f.write(json.dumps({"id": k, "text": QUERY_PROMPT + v}) + "\n")
    with open(out / "docs.jsonl", "w") as f:
        for k, v in docs.items():
            f.write(json.dumps({"id": k, "text": DOC_PROMPT + v}) + "\n")
    with open(out / "pairs.jsonl", "w") as f:
        for p in pairs:
            f.write(json.dumps(p) + "\n")
    print(out.name, json.dumps({"queries": len(queries), "docs": len(docs), "pairs": len(pairs)}), flush=True)


def build_nq(raw: Path, n: int, rng):
    import pyarrow.parquet as pq
    rows = pq.read_table(raw / "sentence-transformers__natural-questions" / FILES["sentence-transformers/natural-questions"][0]).to_pylist()
    rng.shuffle(rows)
    queries, docs, pairs, seen = {}, {}, [], set()
    for r in rows:
        q, d = r["query"].strip(), r["answer"].strip()
        if not q or not d or q.lower() in seen:
            continue
        seen.add(q.lower())
        did = "d" + hid(d)
        docs[did] = d
        qid = "q" + hid("nq" + q)
        queries[qid] = q
        pairs.append({"q": qid, "pos": did, "gold": [did], "answers": []})
    pairs = pairs[:n] if n else pairs
    keep_q = {p["q"] for p in pairs}
    return {k: v for k, v in queries.items() if k in keep_q}, docs, pairs  # all docs stay in the pool


def build_hotpot(raw: Path, n: int, rng):
    import pyarrow.parquet as pq
    rows = []
    for f in FILES["hotpotqa/hotpot_qa"]:
        rows += pq.read_table(raw / "hotpotqa__hotpot_qa" / f, columns=["id", "question", "answer", "supporting_facts", "context"]).to_pylist()
    rng.shuffle(rows)
    queries, docs, pairs = {}, {}, []
    for r in rows:
        paras = {}
        for t, sents in zip(r["context"]["title"], r["context"]["sentences"]):
            text = (t + " " + "".join(sents).strip()).strip()
            did = "d" + hid(text)
            docs[did] = text
            paras[t] = did
        gold = sorted({paras[t] for t in r["supporting_facts"]["title"] if t in paras})
        if not gold:
            continue
        qid = "q" + hid("hp" + r["id"])
        queries[qid] = r["question"].strip()
        ans = r["answer"].strip()
        pairs.append({"q": qid, "pos": rng.choice(gold), "gold": gold,
                      "answers": [ans] if ans.lower() not in ("yes", "no") and len(ans) > 2 else []})
    pairs = pairs[:n] if n else pairs
    keep_q = {p["q"] for p in pairs}
    return {k: v for k, v in queries.items() if k in keep_q}, docs, pairs


def build_squad(raw: Path, n: int, rng, per_ctx=3):
    import pyarrow.parquet as pq
    rows = pq.read_table(raw / "rajpurkar__squad_v2" / FILES["rajpurkar/squad_v2"][0]).to_pylist()
    rng.shuffle(rows)
    queries, docs, pairs, cnt = {}, {}, [], {}
    for r in rows:
        text = (r["title"].replace("_", " ") + " " + r["context"].strip()).strip()
        did = "d" + hid(text)
        docs[did] = text
        ans = [x for x in r["answers"]["text"] if x.strip()]
        if not ans or cnt.get(did, 0) >= per_ctx:
            continue
        cnt[did] = cnt.get(did, 0) + 1
        qid = "q" + hid("sq" + r["id"])
        queries[qid] = r["question"].strip()
        pairs.append({"q": qid, "pos": did, "gold": [did], "answers": sorted(set(a for a in ans if len(a) > 2))})
    pairs = pairs[:n] if n else pairs
    keep_q = {p["q"] for p in pairs}
    return {k: v for k, v in queries.items() if k in keep_q}, docs, pairs


EXTRA = {"BeIR/fever": ["corpus/corpus-00000-of-00001.parquet", "queries/queries-00000-of-00001.parquet"],
         "BeIR/fever-qrels": ["train.tsv"],
         "miracl/miracl": ["miracl-v1.0-en/topics/topics.miracl-v1.0-en-train.tsv",
                           "miracl-v1.0-en/qrels/qrels.miracl-v1.0-en-train.tsv"]}


def cmd_fetch_extra(a):
    from huggingface_hub import hf_hub_download
    for repo, files in EXTRA.items():
        for f in files:
            print(hf_hub_download(repo, f, repo_type="dataset", local_dir=a.out / repo.replace("/", "__")), flush=True)


def keep_frac(key: str, frac: float) -> bool:
    return int(hashlib.sha1(key.encode()).hexdigest()[:8], 16) < frac * 0xFFFFFFFF


def build_fever(raw: Path, n: int, rng, pool=1_000_000):
    import pyarrow.parquet as pq
    R = raw / "BeIR__fever"
    rel = {}
    for line in open(raw / "BeIR__fever-qrels" / "train.tsv"):
        q, d, sc = line.rstrip("\n").split("\t")[:3]
        if q == "query-id" or not sc.isdigit() or int(sc) <= 0:
            continue
        rel.setdefault(q, set()).add(d)
    qtext = {}
    for r in pq.read_table(R / EXTRA["BeIR/fever"][1], columns=["_id", "text"]).to_pylist():
        if r["_id"] in rel:
            qtext[r["_id"]] = r["text"].strip()
    need = set().union(*rel.values())
    pf = pq.ParquetFile(R / EXTRA["BeIR/fever"][0])
    frac = pool / max(1, pf.metadata.num_rows)
    docs = {}
    for b in pf.iter_batches(columns=["_id", "title", "text"], batch_size=65536):
        for did, t, x in zip(b.column(0).to_pylist(), b.column(1).to_pylist(), b.column(2).to_pylist()):
            if did in need or keep_frac(did, frac):
                docs["f" + did] = ((t or "").strip() + " " + (x or "").strip()).strip()
    pairs, seen = [], set()
    qids = sorted(rel)
    rng.shuffle(qids)
    for q in qids:
        t = qtext.get(q)
        gold = sorted("f" + d for d in rel[q] if "f" + d in docs)
        if not t or not gold or t.lower() in seen:
            continue
        seen.add(t.lower())
        pairs.append({"q": "q" + hid("fv" + q), "pos": rng.choice(gold), "gold": gold, "answers": []})
        qtext["q" + hid("fv" + q)] = t
    pairs = pairs[:n] if n else pairs
    keep_q = {p["q"] for p in pairs}
    return {k: v for k, v in qtext.items() if k in keep_q}, docs, pairs


def build_miracl(raw: Path, n: int, rng, pool=500_000, scratch: Path | None = None):
    from huggingface_hub import HfApi, hf_hub_download
    R = raw / "miracl__miracl"
    topics = {}
    for line in open(R / EXTRA["miracl/miracl"][0]):
        q, t = line.rstrip("\n").split("\t")[:2]
        topics[q] = t.strip()
    pos, judged = {}, set()
    for line in open(R / EXTRA["miracl/miracl"][1]):
        q, _, d, r = line.split()[:4]
        judged.add(d)
        if int(r) > 0:
            pos.setdefault(q, set()).add(d)
    files = sorted(f for f in HfApi().list_repo_files("miracl/miracl-corpus", repo_type="dataset")
                   if f.startswith("miracl-corpus-v1.0-en/") and f.endswith(".jsonl.gz"))
    frac = pool / 32_893_221  # en corpus size (MIRACL card)
    docs = {}
    tmp = scratch or raw / "miracl__corpus_tmp"
    for f in files:
        lp = hf_hub_download("miracl/miracl-corpus", f, repo_type="dataset", local_dir=tmp)
        import gzip as _gz
        for line in _gz.open(lp, "rt"):
            r = json.loads(line)
            if r["docid"] in judged or keep_frac(r["docid"], frac):
                docs["m" + r["docid"]] = ((r.get("title") or "").strip() + " " + r["text"].strip()).strip()
        Path(lp).unlink()
        print(f, len(docs), flush=True)
    queries, pairs = {}, []
    for q in sorted(pos):
        gold = sorted("m" + d for d in pos[q] if "m" + d in docs)
        if q not in topics or not gold:
            continue
        qid = "q" + hid("mi" + q)
        queries[qid] = topics[q]
        pairs.append({"q": qid, "pos": rng.choice(gold), "gold": gold, "answers": []})
    rng.shuffle(pairs)
    pairs = pairs[:n] if n else pairs
    return queries, docs, pairs


def cmd_build_extra(a):
    for name in a.sources.split(","):
        fn = {"fever": build_fever, "miracl": build_miracl}[name]
        write(a.out / name, *fn(a.raw, a.max_per_source, random.Random(a.seed)))


def cmd_build(a):
    for name, fn in (("nq", build_nq), ("hotpotqa", build_hotpot), ("squad", build_squad)):
        write(a.out / name, *fn(a.raw, a.max_per_source, random.Random(a.seed)))


def main():
    ap = argparse.ArgumentParser()
    sp = ap.add_subparsers(dest="cmd", required=True)
    f = sp.add_parser("fetch")
    f.add_argument("--out", type=Path, required=True)
    b = sp.add_parser("build")
    b.add_argument("--raw", type=Path, required=True)
    b.add_argument("--out", type=Path, required=True)
    b.add_argument("--max-per-source", type=int, default=0)
    b.add_argument("--seed", type=int, default=0)
    fe = sp.add_parser("fetch-extra")
    fe.add_argument("--out", type=Path, required=True)
    be = sp.add_parser("build-extra")
    be.add_argument("--raw", type=Path, required=True)
    be.add_argument("--out", type=Path, required=True)
    be.add_argument("--sources", default="fever,miracl")
    be.add_argument("--max-per-source", type=int, default=0)
    be.add_argument("--seed", type=int, default=0)
    a = ap.parse_args()
    {"fetch": cmd_fetch, "build": cmd_build, "fetch-extra": cmd_fetch_extra, "build-extra": cmd_build_extra}[a.cmd](a)


if __name__ == "__main__":
    main()
