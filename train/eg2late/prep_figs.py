"""S3 training sources from the data side's figure shards (rust/pigmed-ctx):
    <ctx>/shards/<shard>/figures.jsonl.gz   {id, article_id, group, image (relative to the shard dir), caption,
                                             mentions, licence, s3_key, md5, pmcid, doi}
    <ctx>/shards/<shard>/figvec/            published v2 image vectors (flashembed layout, keyed by figure id)
    <ctx>/shards/<shard>/capvec/            base EG2 caption vectors with the QUERY prompt (keyed by figure id)

    build    -> <out>/pmc_fig_caption/  figures.jsonl (absolute image paths), queries.jsonl (QUERY_PROMPT + caption,
                                        query id = "cap:<fig id>"), pairs.jsonl {q, pos, gold, group}, qvec/ (capvec
                                        re-keyed to query ids), dvec/ = tvec/ (figvec), corpus, source.json
                                        (symmetric: caption <-> figure, reverse loss on)
    (then mine.py on pmc_fig_caption -> triples.jsonl: hard negatives from the v2 image vectors, paper-group masked)
    mentions -> <out>/pmc_fig_mention/  in-text mentions as queries ("men:<fig id>:<k>"), sharing the figure store;
                                        each mention reuses its figure's mined caption negatives (no mention vectors,
                                        so no teacher on this source)
Shards are merged in name order; --shards limits them (count or comma list).

Negatives: other figures of the SAME article are the intended hard negatives (they are different figures), so pairs
carry no paper group by default (no group masking); true duplicates (the same image file in another version of the
paper) are masked through `gold` = every figure id sharing the image's md5. --mask-group restores article-group masking.
"""
from __future__ import annotations

import argparse
import gzip
import json
import os
from pathlib import Path

import numpy as np

from .model import QUERY_PROMPT


def _rows(p: Path):
    op = gzip.open if p.suffix == ".gz" else open
    with op(p, "rt") as f:
        for line in f:
            yield json.loads(line)


def _shards(ctx: Path, sel: str | None):
    base = ctx / "shards" if (ctx / "shards").is_dir() else ctx
    sh = sorted(p for p in base.iterdir() if (p / "figures.jsonl.gz").exists() or (p / "figures.jsonl").exists())
    if sel:
        sh = sh[: int(sel)] if sel.isdigit() else [p for p in sh if p.name in set(sel.split(","))]
    return sh


def _vecs(d: Path):
    ids = json.loads((d / "embeddings.ids.json").read_text())
    ids = ids["ids"] if isinstance(ids, dict) else ids
    return ids, np.load(d / "embeddings.dense.npy", mmap_mode="r")


def _write_vecs(d: Path, ids: list[str], v: np.ndarray):
    d.mkdir(parents=True, exist_ok=True)
    np.save(d / "embeddings.dense.npy", v.astype(np.float16))
    (d / "embeddings.ids.json").write_text(json.dumps(ids))


def cmd_build(a):
    out = a.out / "pmc_fig_caption"
    out.mkdir(parents=True, exist_ok=True)
    fig_ids, cap_ids, fv, cv = [], [], [], []
    rows_kept, by_md5 = [], {}
    n = {"shards": 0, "figures": 0, "no_figvec": 0, "no_capvec": 0, "no_caption": 0, "missing_image": 0}
    with open(out / "figures.jsonl", "w") as ff, open(out / "queries.jsonl", "w") as fq, \
            open(out / "pairs.jsonl", "w") as fp:
        for sh in _shards(a.ctx, a.shards):
            n["shards"] += 1
            man = sh / "figures.jsonl.gz" if (sh / "figures.jsonl.gz").exists() else sh / "figures.jsonl"
            fid, fvec = _vecs(sh / "figvec")
            cid, cvec = _vecs(sh / "capvec")
            fi, ci = {x: i for i, x in enumerate(fid)}, {x: i for i, x in enumerate(cid)}
            for r in _rows(man):
                img = r["image"] if os.path.isabs(r["image"]) else str((sh / r["image"]).resolve())
                if r["id"] not in fi:
                    n["no_figvec"] += 1
                    continue
                if a.check_images and not os.path.exists(img):
                    n["missing_image"] += 1
                    continue
                n["figures"] += 1
                ff.write(json.dumps({**r, "image": img}) + "\n")
                if r.get("md5"):
                    by_md5.setdefault(r["md5"], []).append(r["id"])
                fig_ids.append(r["id"])
                fv.append(np.asarray(fvec[fi[r["id"]]]))
                cap = (r.get("caption") or "").strip()[:4000]
                if not cap:
                    n["no_caption"] += 1
                    continue
                if r["id"] not in ci:
                    n["no_capvec"] += 1
                    continue
                q = f"cap:{r['id']}"
                fq.write(json.dumps({"id": q, "text": QUERY_PROMPT + cap}) + "\n")
                rows_kept.append((q, r["id"], r.get("md5"), r.get("group") if a.mask_group else None))
                cap_ids.append(q)
                cv.append(np.asarray(cvec[ci[r["id"]]]))
        for q, fid, md5, grp in rows_kept:  # gold = the figure + its byte-identical duplicates
            dup = [x for x in by_md5.get(md5, []) if x != fid][:7] if md5 else []
            fp.write(json.dumps({"q": q, "pos": fid, "gold": [fid] + dup, "group": grp}) + "\n")
    n["duplicate_images"] = sum(len(v) - 1 for v in by_md5.values() if len(v) > 1)
    _write_vecs(out / "dvec", fig_ids, np.stack(fv))
    if not (out / "tvec").exists():
        os.symlink("dvec", out / "tvec")
    _write_vecs(out / "qvec", cap_ids, np.stack(cv))
    (out / "corpus").write_text("pmcfig\n")
    (out / "source.json").write_text(json.dumps({"symmetric": True, "origin": "PMC OA commercial figures (pigmed-ctx)",
                                                 "licence": "per row (figures.jsonl licence)",
                                                 "note": "caption -> figure; reverse loss = figure -> caption"}))
    print(json.dumps({**n, "caption_queries": len(cap_ids)}))


def cmd_mentions(a):
    cap = a.out / "pmc_fig_caption"
    out = a.out / "pmc_fig_mention"
    out.mkdir(parents=True, exist_ok=True)
    if not (cap / "triples.jsonl").exists():
        raise SystemExit("run mine.py on pmc_fig_caption first")
    negs, gold, grp = {}, {}, {}
    for t in _rows(cap / "triples.jsonl"):
        negs[t["pos"]], gold[t["pos"]], grp[t["pos"]] = t["negs"], t.get("gold") or [t["pos"]], t.get("group")
    for f in ("figures.jsonl", "corpus"):
        p = out / f
        if not p.exists():
            os.symlink(os.path.relpath(cap / f, out), p)
    n = {"mentions": 0, "figures_without_negs": 0}
    with open(out / "queries.jsonl", "w") as fq, open(out / "triples.jsonl", "w") as ft:
        for r in _rows(cap / "figures.jsonl"):
            ms = [m.strip() for m in (r.get("mentions") or []) if m and len(m.strip()) >= a.min_chars]
            if not ms:
                continue
            if r["id"] not in negs:
                n["figures_without_negs"] += 1
                continue
            for k, m in enumerate(ms[: a.max_per_figure]):
                q = f"men:{r['id']}:{k}"
                fq.write(json.dumps({"id": q, "text": QUERY_PROMPT + m[:2000]}) + "\n")
                ft.write(json.dumps({"q": q, "pos": r["id"], "negs": negs[r["id"]], "gold": gold[r["id"]],
                                     "group": grp[r["id"]]}) + "\n")
                n["mentions"] += 1
    (out / "source.json").write_text(json.dumps({"symmetric": True, "origin": "PMC OA commercial figure mentions",
                                                 "licence": "per row (figures.jsonl licence)"}))
    print(json.dumps(n))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sp = ap.add_subparsers(dest="cmd", required=True)
    b = sp.add_parser("build")
    b.add_argument("--ctx", type=Path, required=True)
    b.add_argument("--out", type=Path, required=True)
    b.add_argument("--shards", default=None)
    b.add_argument("--check-images", action="store_true")
    b.add_argument("--mask-group", action="store_true", help="mask same-paper-group figures (default: only md5 dups)")
    m = sp.add_parser("mentions")
    m.add_argument("--out", type=Path, required=True)
    m.add_argument("--min-chars", type=int, default=30)
    m.add_argument("--max-per-figure", type=int, default=3)
    a = ap.parse_args()
    {"build": cmd_build, "mentions": cmd_mentions}[a.cmd](a)


if __name__ == "__main__":
    main()
