"""Long-context text inputs from real PigMed bundles: JATS paragraphs (article.json blocks[].text) concatenated
across CC-BY articles until each target length is reached, then cut to the exact token count for each model.

Writes into --out:
  ctx-gemma.jsonl / ctx-neomme.jsonl    documents at each target length (+ one over-max item to check truncation)
  mixed-gemma.jsonl / mixed-neomme.jsonl one mixed-length batch order (128, 2048, 512, 4096, ...)
  queries-gemma.jsonl / queries-neomme.jsonl short queries (16 / 64 / 256 tokens)
Row: {"id", "target", "tokens" (with the model's own specials/template), "text"}. Gemma rows already carry the card
prefix ("title: ... | text: " or "task: search result | query: "); NeoMME rows are raw (runner adds <doc>/<query>).
"""

from __future__ import annotations

import argparse
import json
import tarfile
from pathlib import Path


def load_articles(tars):
    arts = []
    for t in tars:
        with tarfile.open(t) as tf:
            for m in tf.getmembers():
                if m.name.endswith("/article.json"):
                    d = json.load(tf.extractfile(m))
                    paras = [b["text"].strip() for b in d.get("blocks", []) if b.get("text", "").strip()]
                    if sum(map(len, paras)) > 4000:
                        arts.append({"pmcid": d.get("pmcid") or d.get("article_id"), "title": d.get("title") or "none",
                                     "license": d.get("license"), "paras": paras})
    arts.sort(key=lambda a: a["pmcid"])
    return arts


def body_ids(tok, arts, start, need):
    """Token ids of paragraphs concatenated from article `start` on until >= need tokens."""
    ids, used, i = [], [], start
    while len(ids) < need:
        a = arts[i % len(arts)]
        ids += tok("\n\n".join(a["paras"]) + "\n\n", add_special_tokens=False)["input_ids"]
        used.append(a["pmcid"])
        i += 1
    return ids, used


def fit(tok, make_text, count, ids, target):
    """Cut body ids so that count(make_text(decode(body))) == target (decode/re-encode can shift by a token)."""
    n = target
    for _ in range(40):
        body = tok.decode(ids[: max(1, n)])
        c = count(make_text(body))
        if c == target:
            return make_text(body), c
        n += target - c
    return make_text(body), c


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--tars", nargs="+", required=True)
    ap.add_argument("--gemma", required=True)
    ap.add_argument("--neomme", required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--per-length", type=int, default=3)
    args = ap.parse_args()
    from transformers import AutoTokenizer

    arts = load_articles(args.tars)
    print(f"{len(arts)} articles with text")
    meta = {"articles": [{k: a[k] for k in ("pmcid", "title", "license")} for a in arts]}
    specs = {
        "gemma": (AutoTokenizer.from_pretrained(args.gemma), 8192,
                  lambda title: (lambda body: f"title: {title} | text: {body}"),
                  lambda body: f"task: search result | query: {body}",
                  lambda tok: (lambda text: len(tok(text)["input_ids"]))),
        "neomme": (AutoTokenizer.from_pretrained(args.neomme), 16384,
                   lambda title: (lambda body: body), lambda body: body,
                   lambda tok: (lambda text: len(tok("<doc>" + text, add_special_tokens=False)["input_ids"]))),
    }
    for model, (tok, maxlen, doc_fmt, q_fmt, counter) in specs.items():
        count = counter(tok)
        lengths = [128, 512, 1024, 2048, 4096] + ([8192] if maxlen >= 8192 else []) + ([16384] if maxlen >= 16384 else [])
        rows = []
        for L in lengths:
            for k in range(args.per_length):
                start = (k * 7 + L) % len(arts)
                ids, used = body_ids(tok, arts, start, L + 64)
                text, c = fit(tok, doc_fmt(arts[start % len(arts)]["title"]), count, ids, L)
                rows.append({"id": f"L{L}-{k}", "target": L, "tokens": c, "articles": used, "text": text})
        # over-max: must be truncated to exactly maxlen by both runtimes
        ids, used = body_ids(tok, arts, 3, maxlen + 600)
        text = doc_fmt(arts[3]["title"])(tok.decode(ids[: maxlen + 512]))
        rows.append({"id": f"over{maxlen}", "target": maxlen, "tokens": count(text), "articles": used, "text": text})
        with open(args.out / f"ctx-{model}.jsonl", "w") as f:
            for r in rows:
                f.write(json.dumps(r) + "\n")
        for L in lengths + ["over"]:
            sel = [r for r in rows if (r["id"].startswith("over") if L == "over" else r["target"] == L and r["id"].startswith("L"))]
            with open(args.out / f"ctx-{model}-{'over' if L == 'over' else f'L{L}'}.jsonl", "w") as f:
                for r in sel:
                    f.write(json.dumps(r) + "\n")
        mixed_order = [128, 2048, 512, 4096, 1024, 128, 4096, 512]
        by = {}
        for r in rows:
            by.setdefault(r["target"], []).append(r)
        with open(args.out / f"mixed-{model}.jsonl", "w") as f:
            for j, L in enumerate(mixed_order):
                r = dict(by[L][j % len(by[L])])
                r["id"] = f"mix{j}-{r['id']}"
                f.write(json.dumps(r) + "\n")
        with open(args.out / f"queries-{model}.jsonl", "w") as f:
            for L in (16, 64, 256):
                for k in range(args.per_length):
                    ids, used = body_ids(tok, arts, k + L, L + 8)
                    body = tok.decode(ids[:L])
                    f.write(json.dumps({"id": f"Q{L}-{k}", "target": L, "articles": used, "text": q_fmt(body)}) + "\n")
        print(model, [(r["id"], r["tokens"]) for r in rows])
    (args.out / "ctx-inputs.json").write_text(json.dumps(meta, indent=1))


if __name__ == "__main__":
    main()
