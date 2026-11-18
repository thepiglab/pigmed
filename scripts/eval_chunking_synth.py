"""Score chunking / embedding variants on LLM-generated queries (rust/pigmed-synth generations) at article level.

Each generation row has a source chunk (`chunk_id`, a unit of the baseline corpus), its `article_id` and up to four
query styles (keyword, clinician, patient, contextual). A query's target is its source article; ranks are exact
(1 + number of articles scoring strictly higher), over every body-text unit of each variant's corpus.

  queries  --gens G1.jsonl [G2.jsonl ...] --out Q [--per-arm N] [--prompt P ...]
           -> Q/queries.json and, per prompt, a one-bundle root Q/root-<k>/ whose rows carry the exact model input
              (`embed_text`), so `scripts/embed_bundles.py --serve --text-only` embeds them on GPU.
  prep     --src ROOT --out ROOT2 [--heading-prefix]   keep only body-text rows of ROOT's bundles (optionally
           prefixing the top-level heading to the model input), for text-only GPU embedding of new variants.
  score    --q Q --qvec QVEC_DIR --variant NAME=ROOT:VECTORS ... [--mix NAME=BASE:OTHER:BETA ...]
           [--fuse LAMBDA ...] [--dims 768,512,256] --out R
           article score = max over the article's units (+ LAMBDA x mean over its units, for each --fuse value);
           --mix scores normalize(v_BASE + BETA * v_OTHER) for units present in both (e.g. naive C + late D).
"""
import argparse
import gzip
import hashlib
import json
import math
import sys
from pathlib import Path

import numpy as np

STYLES = ("keyword", "clinician", "patient", "contextual")


def h(s):
    return int(hashlib.sha256(s.encode()).hexdigest()[:12], 16)


def write_bundle(root: Path, rows):
    d = root / "runs" / "q" / "ready" / "q-000000000"
    d.mkdir(parents=True, exist_ok=True)
    with gzip.open(d / "items.jsonl.gz", "wt") as f:
        for r in rows:
            f.write(json.dumps(r) + "\n")
    (d / "manifest.json").write_text(json.dumps({"bundle": d.name, "rows": len(rows), "experiment": True}))


def cmd_queries(a):
    qs = []
    for g in a.gens:
        arm = Path(g).parent.name
        rows = [json.loads(l) for l in open(g)]
        rows = [r for r in rows if r.get("ok") and isinstance(r.get("queries"), dict)]
        rows.sort(key=lambda r: h(arm + r["chunk_id"]))
        if a.per_arm:
            rows = rows[: a.per_arm]
        for r in rows:
            for st in STYLES:
                t = (r["queries"].get(st) or "").strip()
                if len(t.split()) >= 2:
                    qs.append({"qid": f"{arm}:{r['chunk_id']}:{st}", "task": st, "arm": arm, "text": t,
                               "article_id": r["article_id"], "chunk_id": r["chunk_id"]})
    a.out.mkdir(parents=True, exist_ok=True)
    (a.out / "queries.json").write_text(json.dumps(qs))
    for k, p in enumerate(a.prompt):
        write_bundle(a.out / f"root-{k}", [{"unit_id": f"p{k}|{q['qid']}", "article_id": q["article_id"], "kind": "text",
                                           "text": q["text"], "embed_text": p + q["text"]} for q in qs])
    (a.out / "prompts.json").write_text(json.dumps(a.prompt))
    print(json.dumps({"queries": len(qs), "arms": sorted({q["arm"] for q in qs}), "articles": len({q["article_id"] for q in qs}),
                      "by_style": {s: sum(q["task"] == s for q in qs) for s in STYLES}}))


def cmd_prep(a):
    n = 0
    for f in sorted(a.src.glob("runs/*/ready/*/items.jsonl.gz")):
        rows = []
        for line in gzip.open(f, "rt"):
            r = json.loads(line)
            if r.get("kind") == "text" and (r.get("text") or "").strip():
                if a.heading_prefix:
                    sec = ((r.get("locator") or {}).get("section") or [""])[0] or ""
                    r["embed_text"] = "title: none | text: " + (f"{sec}\n" if sec else "") + r["text"]
                rows.append(r)
        d = a.out / "runs" / f.parent.parent.parent.name / "ready" / f.parent.name
        d.mkdir(parents=True, exist_ok=True)
        with gzip.open(d / "items.jsonl.gz", "wt") as o:
            for r in rows:
                o.write(json.dumps(r) + "\n")
        (d / "manifest.json").write_text(json.dumps({"bundle": d.name, "rows": len(rows), "experiment": True}))
        n += len(rows)
    print(json.dumps({"units": n}))


def cmd_late(a):
    """Late-chunking root from a chunked root, as scripts/chunk_variants.py variant D: per article one `late_doc` row
    ("title: <title> | text: " + chunks in order, each preceded by its section path line) with one span per chunk
    (span id = chunk unit id, so naive and late vectors can be mixed by id) plus `text_meta` rows."""
    import re
    n = 0
    for f in sorted(a.src.glob("runs/*/ready/*/items.jsonl.gz")):
        arts = {}
        for line in gzip.open(f, "rt"):
            r = json.loads(line)
            if r.get("kind") == "text" and (r.get("text") or "").strip():
                arts.setdefault(r["article_id"], []).append(r)
        rows = []
        for aid, rs in arts.items():
            title = re.sub(r"\s+", " ", rs[0].get("title") or "none").strip() or "none"
            prefix = f"title: {title} | text: "
            doc, spans = "", []
            for r in rs:
                path = (r.get("locator") or {}).get("section") or []
                head = (" > ".join(x for x in path if x) + "\n") if path else ""
                if doc:
                    doc += "\n\n"
                doc += head
                s0 = len(doc)
                doc += r["text"]
                spans.append({"id": r["unit_id"], "start": s0 + len(prefix), "end": len(doc) + len(prefix)})
                m = dict(r); m["kind"] = "text_meta"; rows.append(m)
            rows.append({"article_id": aid, "unit_id": hashlib.sha256(f"{aid}|late_doc".encode()).hexdigest()[:32], "kind": "late_doc",
                         "text": prefix + doc, "prefix_chars": len(prefix), "spans": spans})
            n += len(spans)
        d = a.out / "runs" / f.parent.parent.parent.name / "ready" / f.parent.name
        d.mkdir(parents=True, exist_ok=True)
        with gzip.open(d / "items.jsonl.gz", "wt") as o:
            for r in rows:
                o.write(json.dumps(r) + "\n")
        (d / "manifest.json").write_text(json.dumps({"bundle": d.name, "rows": len(rows), "experiment": True}))
    print(json.dumps({"spans": n}))


UNIT_KINDS = {"text", "text_meta"}  # score-qrels --kinds adds e.g. caption, table_text, supplement_text, table_card


def iter_units(root: Path):
    for f in sorted(root.glob("runs/*/ready/*/items.jsonl.gz")):
        for line in gzip.open(f, "rt"):
            r = json.loads(line)
            if r.get("kind") in UNIT_KINDS:
                yield r
            elif r.get("kind") == "late_doc":  # late-only roots (chunk3_arms latewin): one pseudo unit per span id
                for sp in r.get("spans") or []:
                    yield {"unit_id": sp["id"], "article_id": r["article_id"], "kind": "text", "is_abstract": False}


def load_vectors(vectors: Path, keep_ids=None):
    out = {}
    for d in sorted((vectors / "gemma").glob("*/meta.json")):
        ids = json.loads((d.parent / "ids.json").read_text())
        v = np.load(d.parent / "dense.npy")
        for i, (u, m) in enumerate(zip(ids["unit_id"], ids["modality"])):
            if m == "text" and (keep_ids is None or u in keep_ids):
                out[u] = v[i]
    return out


def load_corpus(root: Path, vectors: Path):
    meta = {}
    for r in iter_units(root):
        meta.setdefault(r["unit_id"], (r["article_id"], bool(r.get("is_abstract"))))  # first record wins
    vec = load_vectors(vectors, set(meta))
    uids = sorted(vec, key=lambda u: (meta[u][0], u))  # contiguous per article
    return uids, np.stack([vec[u] for u in uids]).astype(np.float16), np.array([meta[u][0] for u in uids]), \
        np.array([meta[u][1] for u in uids])


def norm(x):
    return x / np.maximum(np.linalg.norm(x, axis=1, keepdims=True), 1e-12)


def ranks_for(qv, docs, arts, targets, fuses):
    """Exact article rank of each query's target for article score = max (+ lam * mean) over the article's units."""
    starts = np.flatnonzero(np.r_[True, arts[1:] != arts[:-1]])
    names = arts[starts]
    cnt = np.diff(np.r_[starts, len(arts)]).astype(np.float32)
    pos = {a: i for i, a in enumerate(names)}
    tix = np.array([pos.get(t, -1) for t in targets])
    out = {f: [] for f in fuses}
    docs = docs.astype(np.float32)
    A = np.stack([ARTVEC.get(n, np.zeros(docs.shape[1], np.float32))[:docs.shape[1]] for n in names]) if ARTVEC else None
    for s in range(0, len(qv), 128):
        sc = qv[s:s + 128] @ docs.T
        mx = np.maximum.reduceat(sc, starts, axis=1)
        mean = np.add.reduceat(sc, starts, axis=1) / cnt
        av = qv[s:s + 128] @ A.T if A is not None else None
        for f in fuses:
            lam, mu = f if isinstance(f, tuple) else (f, 0.0)
            art = mx + lam * mean if lam else mx
            if mu and av is not None:
                art = art + mu * av
            for row, t in zip(art, tix[s:s + 128]):
                out[f].append(None if t < 0 else int((row > row[t]).sum()) + 1)
    return out


def ranks_for_torch(qv, docs, arts, targets, fuses, dev):
    """GPU version of ranks_for (same definition): fp32 matmul, scatter-max / scatter-mean per article."""
    import torch
    names, inv = np.unique(arts, return_inverse=True)
    pos = {a: i for i, a in enumerate(names)}
    tix = torch.tensor([pos.get(t, -1) for t in targets], device=dev)
    D = torch.from_numpy(np.ascontiguousarray(docs, dtype=np.float32)).to(dev)
    idx = torch.from_numpy(inv.astype(np.int64)).to(dev)
    cnt = torch.bincount(idx, minlength=len(names)).float()
    A = None
    if ARTVEC and any(isinstance(f, tuple) for f in fuses):
        dim = docs.shape[1]
        A = torch.from_numpy(np.stack([ARTVEC.get(n, np.zeros(dim, np.float32))[:dim] for n in names]).astype(np.float32)).to(dev)
    out = {lam: [] for lam in fuses}
    B = 128
    for s in range(0, len(qv), B):
        q = torch.from_numpy(np.ascontiguousarray(qv[s:s + B], dtype=np.float32)).to(dev)
        sc = q @ D.T
        ii = idx.unsqueeze(0).expand(sc.shape[0], -1)
        mx = torch.full((sc.shape[0], len(names)), float("-inf"), device=dev).scatter_reduce_(1, ii, sc, "amax", include_self=True)
        mean = torch.zeros((sc.shape[0], len(names)), device=dev).scatter_add_(1, ii, sc) / cnt if any(l for l in fuses) else None
        av = (q @ A.T) if A is not None else None
        t = tix[s:s + B]
        for f in fuses:
            lam, mu = f if isinstance(f, tuple) else (f, 0.0)
            art = mx if lam == 0 else mx + lam * mean
            if mu and av is not None:
                art = art + mu * av
            tv = art.gather(1, t.clamp(min=0).unsqueeze(1))
            r = ((art > tv).sum(1) + 1).tolist()
            out[f] += [None if ti < 0 else int(x) for ti, x in zip(t.tolist(), r)]
        del sc, ii, mx, mean
    del D
    return out


def metrics(r):
    r = np.array([x if x is not None else math.inf for x in r], float)
    return {"n": int(len(r)), "MRR@10": float(np.mean(np.where(r <= 10, 1 / r, 0))), "R@1": float(np.mean(r <= 1)),
            "R@10": float(np.mean(r <= 10)), "R@100": float(np.mean(r <= 100))}


def rr(r):
    return np.array([1 / x if x and x <= 10 else 0.0 for x in r])


def boot(a, b, n=2000):
    d = a - b
    rng = np.random.default_rng(0)
    m = [d[rng.integers(0, len(d), len(d))].mean() for _ in range(n)]
    return [float(np.percentile(m, 2.5)), float(np.percentile(m, 97.5))]


def cmd_score(a):
    qs = json.loads((a.q / "queries.json").read_text())
    prompts = json.loads((a.q / "prompts.json").read_text())
    qvecs = []
    for k in range(len(prompts)):
        v = load_vectors(Path(str(a.qvec).replace("{k}", str(k))))
        qvecs.append({u.split("|", 1)[1]: x for u, x in v.items()})
    res = {"prompts": prompts, "variants": {}}
    base_rr = {}
    if getattr(a, "artvec", None):
        load_artvec(a.artvec)
    dims = [int(x) for x in a.dims.split(",")]
    specs = [s.split("=", 1) for s in a.variant]
    corp = {}
    for name, rest in specs:  # ROOT:VECTORS, or several joined with "+" (union corpus, e.g. two granularities)
        parts = [load_corpus(Path(x.split(":", 1)[0]), Path(x.split(":", 1)[1])) for x in rest.split("+")]
        if len(parts) == 1:
            corp[name] = parts[0]
        else:
            u = sum((p[0] for p in parts), [])
            d, ar, ab = (np.concatenate([p[i] for p in parts]) for i in (1, 2, 3))
            o = np.lexsort((np.array(u), ar))  # contiguous per article
            corp[name] = ([u[i] for i in o], d[o], ar[o], ab[o])
        print(name, "units", len(corp[name][0]), flush=True)
    jobs = [(name, name, None) for name, _ in specs]
    for m in a.mix:
        name, rest = m.split("=", 1)
        b, o, beta = rest.split(":")
        jobs.append((name, b, (o, float(beta))))
    if getattr(a, "base", None):  # the named variant is the baseline for every diff/CI
        jobs.sort(key=lambda j: j[0] != a.base)
    for name, base, mix in jobs:
        uids, docs, arts, abst = corp[base]
        if mix:
            ou, od, _, _ = corp[mix[0]]
            oi = {u: i for i, u in enumerate(ou)}
            keep = np.array([u in oi for u in uids])
            if getattr(a, "mix_keep_unmatched", False):  # units without a twin (captions, tables) keep their vector
                docs = docs.astype(np.float32).copy()
                idx = np.flatnonzero(keep)
                docs[idx] = norm(docs[idx] + mix[1] * od[[oi[uids[i]] for i in idx]].astype(np.float32))
                docs = docs.astype(np.float16)
            else:
                docs = norm(docs[keep].astype(np.float32) + mix[1] * od[[oi[u] for u, k in zip(uids, keep) if k]].astype(np.float32)).astype(np.float16)
                arts, abst = arts[keep], abst[keep]
        pool = ~abst if a.no_abstract else np.ones(len(arts), bool)
        for k in range(len(prompts)):
            if k > 0 and name != jobs[0][0]:
                continue  # prompt variants only on the baseline variant
            for d in dims:
                if d != 768 and (name != jobs[0][0] and name not in a.dims_on):
                    continue
                key = f"{name}" + (f"|p{k}" if k else "") + (f"|d{d}" if d != 768 else "")
                aset = set(arts[pool].tolist())
                tq = [q for q in qs if q["qid"] in qvecs[k] and q["article_id"] in aset]
                qv = np.stack([qvecs[k][q["qid"]] for q in tq]).astype(np.float32)
                dv = docs[pool]
                if d != 768:
                    qv, dv = norm(qv[:, :d]), norm(dv[:, :d].astype(np.float32)).astype(np.float16)
                tg = np.array([q["article_id"] for q in tq])
                fl = [0.0] + a.fuse + [(lam, mu) for mu in a.art_weight for lam in [0.0] + a.fuse]
                rk = ranks_for_torch(qv, dv, arts[pool], tg, fl, a.device) if a.device != "cpu" \
                    else ranks_for(qv, dv, arts[pool], tg, fl)
                for f, r in rk.items():
                    lam, mu = f if isinstance(f, tuple) else (f, 0.0)
                    kk = key + (f"|fuse{lam}" if lam else "") + (f"|art{mu}" if mu else "")
                    out = {"all": metrics(r)}
                    for st in STYLES:
                        idx = [i for i, q in enumerate(tq) if q["task"] == st]
                        out[st] = metrics([r[i] for i in idx])
                    arms = sorted({q["arm"] for q in tq})
                    out["by_arm_MRR@10"] = {arm: metrics([r[i] for i, q in enumerate(tq) if q["arm"] == arm])["MRR@10"] for arm in arms}
                    order = {q["qid"]: i for i, q in enumerate(tq)}
                    cur = rr(r)
                    if not base_rr and (not getattr(a, "base_key", None) or kk == a.base_key):
                        base_rr.update({"name": kk, "qids": [q["qid"] for q in tq], "rr": cur})
                    elif base_rr:
                        common = [order[x] for x in base_rr["qids"] if x in order]
                        bi = [i for i, x in enumerate(base_rr["qids"]) if x in order]
                        out[f"MRR@10_vs_{base_rr['name']}"] = {"diff": float(cur[common].mean() - base_rr["rr"][bi].mean()),
                                                               "ci95": boot(cur[common], base_rr["rr"][bi])}
                    out["units"] = int(pool.sum())
                    res["variants"][kk] = out
                    print(kk, json.dumps({x: out[x] for x in out if x in ("all", "units") or x.startswith("MRR@10_vs")}), flush=True)
    a.out.mkdir(parents=True, exist_ok=True)
    (a.out / "results.json").write_text(json.dumps(res, indent=1))


# ---------------------------------------------------------------- real qrels (graded, many relevant per query)
def cmd_qroot(a):
    """Query roots for a qrels set: Q/queries.json [{qid, task, text}] -> Q/root-<k> per prompt (+ Q/prompts.json)."""
    qs = json.loads((a.q / "queries.json").read_text())
    for k, p in enumerate(a.prompt):
        write_bundle(a.q / f"root-{k}", [{"unit_id": f"p{k}|{q['qid']}", "article_id": q["qid"], "kind": "text",
                                          "text": q["text"], "embed_text": p + q["text"]} for q in qs])
    (a.q / "prompts.json").write_text(json.dumps(a.prompt))
    print(json.dumps({"queries": len(qs), "prompts": a.prompt}))


def corpora(a):
    corp = {}
    for name, rest in (s.split("=", 1) for s in a.variant):
        parts = [load_corpus(Path(x.split(":", 1)[0]), Path(x.split(":", 1)[1])) for x in rest.split("+")]
        u = sum((p[0] for p in parts), [])
        d, ar, ab = (np.concatenate([p[i] for p in parts]) for i in (1, 2, 3))
        if a.pmcid_level:  # one article per PMCID (versions from different corpora merge)
            ar = np.array([x.split(".", 1)[0] for x in ar])
        o = np.lexsort((np.array(u), ar))
        corp[name] = ([u[i] for i in o], d[o], ar[o], ab[o])
        print(name, "units", len(u), "articles", len(set(ar.tolist())), flush=True)
    jobs = [(n, n, None) for n in corp]
    for m in a.mix:
        name, rest = m.split("=", 1)
        b, o, beta = rest.split(":")
        jobs.append((name, b, (o, float(beta))))
    if getattr(a, "base", None):
        jobs.sort(key=lambda j: j[0] != a.base)
    for name, base, mix in jobs:
        uids, docs, arts, abst = corp[base]
        if mix:
            ou, od, _, _ = corp[mix[0]]
            oi = {u: i for i, u in enumerate(ou)}
            keep = np.array([u in oi for u in uids])
            if a.mix_keep_unmatched:  # units without a twin keep their own vector (e.g. captions/tables when OTHER is late)
                docs = docs.astype(np.float32).copy()
                idx = np.flatnonzero(keep)
                docs[idx] = norm(docs[idx] + mix[1] * od[[oi[uids[i]] for i in idx]].astype(np.float32))
                docs = docs.astype(np.float16)
            else:
                docs = norm(docs[keep].astype(np.float32) + mix[1] * od[[oi[u] for u, k in zip(uids, keep) if k]].astype(np.float32)).astype(np.float16)
                arts, abst = arts[keep], abst[keep]
        yield name, docs, arts, abst


ARTVEC = {}  # article id -> unit vector (--artvec), mixed into article scores as + mu * cos(q, art)


def load_artvec(spec, pmcid_level=False):
    for part in spec.split("+"):
        root, vec = part.split(":", 1)
        for u, x in load_vectors(Path(vec)).items():
            aid = u.split("|", 1)[1]
            if pmcid_level:
                aid = aid.split(".", 1)[0]
            ARTVEC[aid] = x.astype(np.float32) / max(float(np.linalg.norm(x)), 1e-12)


def topk_articles(qv, docs, arts, fuses, dev, k=100):
    """-> ({lam: [[article ids top-k] per query]}) with article score = max (+ lam * mean) over units."""
    import torch
    names, inv = np.unique(arts, return_inverse=True)
    D = torch.from_numpy(np.ascontiguousarray(docs, dtype=np.float32)).to(dev)
    idx = torch.from_numpy(inv.astype(np.int64)).to(dev)
    cnt = torch.bincount(idx, minlength=len(names)).float()
    A = None
    if ARTVEC:
        dim = docs.shape[1]
        A = torch.from_numpy(np.stack([ARTVEC.get(n, np.zeros(dim, np.float32))[:dim] for n in names]).astype(np.float32)).to(dev)
    out = {lam: [] for lam in fuses}
    for s in range(0, len(qv), 64):
        q = torch.from_numpy(np.ascontiguousarray(qv[s:s + 64], dtype=np.float32)).to(dev)
        sc = q @ D.T
        ii = idx.unsqueeze(0).expand(sc.shape[0], -1)
        mx = torch.full((sc.shape[0], len(names)), float("-inf"), device=dev).scatter_reduce_(1, ii, sc, "amax", include_self=True)
        mean = torch.zeros((sc.shape[0], len(names)), device=dev).scatter_add_(1, ii, sc) / cnt
        av = (q @ A.T) if A is not None else None
        for f in fuses:
            lam, mu = f if isinstance(f, tuple) else (f, 0.0)
            art = mx + lam * mean if lam else mx
            if mu and av is not None:
                art = art + mu * av
            top = torch.topk(art, min(k, art.shape[1]), dim=1).indices.cpu().numpy()
            out[f] += [[names[j] for j in row] for row in top]
        del sc, ii, mx, mean
    return out


def qrel_metrics(top, rel, present):
    """top: ranked article ids; rel: {article: grade}; present: set of corpus articles."""
    relp = {d: g for d, g in rel.items() if d in present}
    gains = [relp.get(d, 0) for d in top[:10]]
    dcg = sum(g / math.log2(i + 2) for i, g in enumerate(gains))
    ideal = sorted(relp.values(), reverse=True)[:10]
    idcg = sum(g / math.log2(i + 2) for i, g in enumerate(ideal))
    first = next((i + 1 for i, d in enumerate(top[:10]) if relp.get(d, 0) > 0), None)
    nrel = sum(1 for g in relp.values() if g > 0)
    return {"nDCG@10": dcg / idcg if idcg else 0.0, "MRR@10": 1 / first if first else 0.0,
            "R@100": (sum(1 for d in top[:100] if relp.get(d, 0) > 0) / nrel) if nrel else 0.0,
            "judged@10": sum(1 for d in top[:10] if d in rel) / 10}


def cmd_score_qrels(a):
    qs = json.loads((a.q / "queries.json").read_text())
    qrels = json.loads((a.q / "qrels.json").read_text())
    prompts = json.loads((a.q / "prompts.json").read_text())
    qvecs = [{u.split("|", 1)[1]: x for u, x in load_vectors(Path(str(a.qvec).replace("{k}", str(k)))).items()} for k in range(len(prompts))]
    res, base = {"prompts": prompts, "variants": {}}, None
    tasks = sorted({q["task"] for q in qs})
    if a.artvec:
        load_artvec(a.artvec, a.pmcid_level)
    # --alt PREFIX=QVEC_PATTERN@ARTVEC_SPEC: variants whose name starts with PREFIX were embedded by another model, so
    # they use that model's query vectors (and article vectors). Paired diffs still compare per query id.
    alts = []
    for x in a.alt:
        pre, rest = x.split("=", 1)
        qpat, _, art = rest.partition("@")
        alts.append((pre, [{u.split("|", 1)[1]: v for u, v in load_vectors(Path(qpat.replace("{k}", str(k)))).items()} for k in range(len(prompts))], art))
    cur_art = a.artvec
    for name, docs, arts, abst in corpora(a):
        present = set(arts.tolist())
        alt = next((x for x in alts if name.startswith(x[0])), None)
        qvs = alt[1] if alt else qvecs
        want_art = (alt[2] or a.artvec) if alt else a.artvec
        if want_art and want_art != cur_art:
            ARTVEC.clear()
            load_artvec(want_art, a.pmcid_level)
            cur_art = want_art
        for k in range(len(prompts)):
            if k > 0 and base is not None and name != base[0].split("|")[0]:
                continue
            tq = [q for q in qs if q["qid"] in qvs[k] and any(g > 0 and d in present for d, g in qrels.get(q["qid"], {}).items())]
            qv = np.stack([qvs[k][q["qid"]] for q in tq]).astype(np.float32)
            for d in [int(x) for x in a.dims.split(",")]:
                qd, dd = (qv, docs) if d == 768 else (norm(qv[:, :d]), norm(docs[:, :d].astype(np.float32)).astype(np.float16))
                fl = [0.0] + a.fuse + [(lam, mu) for mu in a.art_weight for lam in [0.0] + a.fuse]
                tops = topk_articles(qd, dd, arts, fl, a.device)
                for f, tl in tops.items():
                    lam, mu = f if isinstance(f, tuple) else (f, 0.0)
                    kk = name + (f"|p{k}" if k else "") + (f"|d{d}" if d != 768 else "") + (f"|fuse{lam}" if lam else "") + (f"|art{mu}" if mu else "")
                    per = {q["qid"]: qrel_metrics(t, qrels[q["qid"]], present) for q, t in zip(tq, tl)}
                    out = {"units": int(len(arts)), "articles": len(present)}
                    for task in tasks + ["all"]:
                        ids = [q["qid"] for q in tq if task == "all" or q["task"] == task]
                        if ids:
                            out[task] = {m: float(np.mean([per[i][m] for i in ids])) for m in ("nDCG@10", "MRR@10", "R@100", "judged@10")}
                            out[task]["n"] = len(ids)
                    if base is None and (not a.base_key or kk == a.base_key):
                        base = (kk, per)
                    elif base is not None:
                        for task in tasks:
                            ids = [q["qid"] for q in tq if q["task"] == task and q["qid"] in base[1]]
                            if ids:
                                x = np.array([per[i]["nDCG@10"] for i in ids]); y = np.array([base[1][i]["nDCG@10"] for i in ids])
                                out[task][f"nDCG@10_vs_{base[0]}"] = {"diff": float((x - y).mean()), "ci95": boot(x, y)}
                    res["variants"][kk] = out
                    print(kk, json.dumps({t: {m: round(v, 4) if isinstance(v, float) else v for m, v in out[t].items()} for t in tasks + ["all"] if t in out}), flush=True)
    a.out.mkdir(parents=True, exist_ok=True)
    (a.out / "results.json").write_text(json.dumps(res, indent=1))


def main():
    ap = argparse.ArgumentParser()
    sp = ap.add_subparsers(dest="cmd", required=True)
    q = sp.add_parser("queries")
    q.add_argument("--gens", nargs="+", required=True)
    q.add_argument("--out", type=Path, required=True)
    q.add_argument("--per-arm", type=int, default=0)
    q.add_argument("--prompt", action="append", default=None)
    p = sp.add_parser("prep")
    p.add_argument("--src", type=Path, required=True)
    p.add_argument("--out", type=Path, required=True)
    p.add_argument("--heading-prefix", action="store_true")
    l = sp.add_parser("late")
    l.add_argument("--src", type=Path, required=True)
    l.add_argument("--out", type=Path, required=True)
    s = sp.add_parser("score")
    s.add_argument("--q", type=Path, required=True)
    s.add_argument("--qvec", required=True, help="query vectors dir; {k} = prompt index")
    s.add_argument("--variant", action="append", required=True, help="NAME=ROOT:VECTORS[+ROOT2:VECTORS2] (first is the baseline)")
    s.add_argument("--mix", action="append", default=[], help="NAME=BASE:OTHER:BETA")
    s.add_argument("--fuse", type=float, action="append", default=[])
    s.add_argument("--dims", default="768")
    s.add_argument("--dims-on", action="append", default=[], help="also truncate these variants")
    s.add_argument("--no-abstract", action="store_true", help="exclude abstract units from the corpus")
    s.add_argument("--device", default="cpu", help="cpu (numpy) or cuda (torch)")
    s.add_argument("--base", help="baseline variant name for diffs (default: first)")
    s.add_argument("--base-key", help="exact result key used as the diff baseline, e.g. v2L50|fuse0.25|art0.25")
    s.add_argument("--artvec", help="ROOT:VECTORS of art|<article_id> units (chunk3_arms.py artvec)")
    s.add_argument("--mix-keep-unmatched", action="store_true")
    s.add_argument("--kinds", default="", help="extra unit kinds in the corpus, comma-separated")
    s.add_argument("--art-weight", type=float, action="append", default=[])
    s.add_argument("--out", type=Path, required=True)
    qr = sp.add_parser("qroot")
    qr.add_argument("--q", type=Path, required=True)
    qr.add_argument("--prompt", action="append", default=None)
    sq = sp.add_parser("score-qrels")
    sq.add_argument("--q", type=Path, required=True)
    sq.add_argument("--qvec", required=True)
    sq.add_argument("--variant", action="append", required=True)
    sq.add_argument("--mix", action="append", default=[])
    sq.add_argument("--mix-keep-unmatched", action="store_true")
    sq.add_argument("--fuse", type=float, action="append", default=[])
    sq.add_argument("--dims", default="768")
    sq.add_argument("--pmcid-level", action="store_true")
    sq.add_argument("--base", help="baseline variant name for diffs (default: first)")
    sq.add_argument("--base-key", help="exact result key used as the diff baseline")
    sq.add_argument("--alt", action="append", default=[], help="PREFIX=QVEC_PATTERN[@ARTVEC_SPEC] for variants from another model")
    sq.add_argument("--artvec", help="ROOT:VECTORS[+...] of art|<article_id> units (chunk3_arms.py artvec)")
    sq.add_argument("--art-weight", type=float, action="append", default=[])
    sq.add_argument("--device", default="cuda")
    sq.add_argument("--kinds", default="", help="extra unit kinds in the corpus, comma-separated")
    sq.add_argument("--out", type=Path, required=True)
    a = ap.parse_args()
    if getattr(a, "kinds", ""):
        UNIT_KINDS.update(k for k in a.kinds.split(",") if k)
    if a.cmd == "qroot" and not a.prompt:
        a.prompt = ["task: search result | query: "]
    if a.cmd == "queries" and not a.prompt:
        a.prompt = ["task: search result | query: "]
    {"queries": cmd_queries, "prep": cmd_prep, "late": cmd_late, "score": cmd_score, "qroot": cmd_qroot,
     "score-qrels": cmd_score_qrels}[a.cmd](a)


if __name__ == "__main__":
    sys.exit(main())
