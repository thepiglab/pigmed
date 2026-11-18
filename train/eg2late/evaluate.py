"""PyTorch evaluation of an EG2Dual checkpoint: dense head AND late (MaxSim) head, on the same encoder pass.

  nanobeir --model SNAP --data DIR --out R.json         13 NanoBEIR-en sets (sentence-transformers/NanoBEIR-en), nDCG@10
  conteb   --model SNAP --work W --out R.json           ConTEB 8 sets from scripts/eval_conteb.py prep (naive + late)
  pmc      --model SNAP --root ROOT --q Q --out R.json  98k abstract-sentence -> body (+ title -> article), article level
  compare  A.json B.json                                paired bootstrap of per-query metrics (B - A)

The dense numbers here are a torch re-implementation (parity check against flashembed); the official dense numbers
come from flashembed on the exported snapshot via scripts/eval_conteb.py / eval_chunking.py. The late head has no
other runtime yet, so these are its numbers. --late-head FILE swaps in a late head (e.g. the SVD init for "base").
Prompts: query "task: search result | query: ", document "title: none | text: " everywhere (EG2's retrieval pair).
"""
from __future__ import annotations

import argparse
import gzip
import json
import math
import sys
import time
from pathlib import Path

import numpy as np
import torch
import torch.nn.functional as F

from .contextual import late_windows  # noqa: F401  (moved; ConTEB late chunking uses it)
from .model import DOC_PROMPT, QUERY_PROMPT, EG2Dual, LateHead, Tok

DEV = "cuda"


def ndcg_at_10(order, rel: dict):
    dcg = sum(rel.get(d, 0) / math.log2(r + 2) for r, d in enumerate(order[:10]))
    ideal = sorted(rel.values(), reverse=True)[:10]
    idcg = sum(g / math.log2(r + 2) for r, g in enumerate(ideal))
    return dcg / idcg if idcg > 0 else 0.0


class Enc:
    def __init__(self, model: EG2Dual, tok: Tok, budget: int = 96_000):
        self.m, self.tok, self.budget = model, tok, budget

    def _tok(self, texts, prefix, max_len):
        enc = self.tok.tk(texts, truncation=True, max_length=max_len, return_offsets_mapping=True,
                          return_special_tokens_mask=True)
        out = []
        for ids, off, sp in zip(enc["input_ids"], enc["offset_mapping"], enc["special_tokens_mask"]):
            lm = [(e > prefix and e > s and not x) for (s, e), x in zip(off, sp)]
            out.append((ids, lm))
        return out

    @torch.no_grad()
    def run(self, texts, prefix, max_len, fn, block=65536, late=True):
        """Encode texts (prompted) in length-sorted token-budget batches; fn(idx, dense, late, lmask) per batch.
        dense [b, 768] fp32 normalised; late [b, L, 128] bf16 (zeros at masked tokens); lmask [b, L] bool."""
        for b0 in range(0, len(texts), block):
            toks = self._tok(texts[b0:b0 + block], prefix, max_len)
            order = sorted(range(len(toks)), key=lambda i: -len(toks[i][0]))
            i = 0
            while i < len(order):
                L = len(toks[order[i]][0])
                n = max(1, min(len(order) - i, self.budget // max(L, 1)))
                idx = order[i:i + n]
                i += n
                ids = torch.zeros(n, L, dtype=torch.long)
                am = torch.zeros(n, L, dtype=torch.long)
                lm = torch.zeros(n, L, dtype=torch.bool)
                for r, j in enumerate(idx):
                    t, m = toks[j]
                    ids[r, :len(t)] = torch.tensor(t)
                    am[r, :len(t)] = 1
                    lm[r, :len(t)] = torch.tensor(m)
                ids, am, lm = ids.to(DEV), am.to(DEV), lm.to(DEV)
                with torch.autocast("cuda", dtype=torch.bfloat16):
                    h = self.m.token_outputs(ids, am)
                    m = am.unsqueeze(-1).to(h.dtype)
                    dense = F.normalize((h.float() * m.float()).sum(1) / m.float().sum(1), dim=-1)
                    lt = (self.m.late(h) * lm.unsqueeze(-1)).to(torch.bfloat16) if late else None
                fn([b0 + j for j in idx], dense, lt, lm)

    @torch.no_grad()
    def windows(self, win_ids: list[list[int]]):
        """Token outputs [L_i, 768] (bf16) for pre-tokenised windows."""
        out = [None] * len(win_ids)
        order = sorted(range(len(win_ids)), key=lambda i: -len(win_ids[i]))
        i = 0
        while i < len(order):
            L = len(win_ids[order[i]])
            n = max(1, min(len(order) - i, self.budget // L))
            idx = order[i:i + n]
            i += n
            ids = torch.zeros(n, L, dtype=torch.long)
            am = torch.zeros(n, L, dtype=torch.long)
            for r, j in enumerate(idx):
                ids[r, :len(win_ids[j])] = torch.tensor(win_ids[j])
                am[r, :len(win_ids[j])] = 1
            with torch.autocast("cuda", dtype=torch.bfloat16):
                h = self.m.token_outputs(ids.to(DEV), am.to(DEV))
            for r, j in enumerate(idx):
                out[j] = h[r, :len(win_ids[j])].clone()  # clone: a view would pin the whole batch tensor
            del h
        return out


class LateStore:
    """Document token vectors kept as padded GPU batches, for exhaustive MaxSim against a query set."""

    def __init__(self):
        self.batches = []  # (doc indices tensor, late [b, L, D], mask [b, L])

    def add(self, idx, late, mask):
        L = int(mask.sum(1).max().item()) if mask.any() else 1
        # compact: move kept tokens to the front so the padded length is the kept length
        key = (~mask).to(torch.int8)
        perm = torch.sort(key, dim=1, stable=True).indices[:, :L]
        late = torch.gather(late, 1, perm.unsqueeze(-1).expand(-1, -1, late.shape[-1]))
        mask = torch.gather(mask, 1, perm)
        self.batches.append((torch.tensor(idx, device=DEV), late.contiguous(), mask.contiguous()))

    def scores(self, ql, qm, n_docs, q_chunk=256):
        """ql [Q, Lq, D], qm [Q, Lq] -> [Q, n_docs] mean-over-query-tokens MaxSim (fp32)."""
        out = torch.full((ql.shape[0], n_docs), -1e4, device=DEV)
        for idx, dl, dm in self.batches:
            out[:, idx] = maxsim_big(ql, qm, dl, dm, q_chunk)
        return out


def maxsim_big(ql, qm, dl, dm, q_chunk=256, elems=2_000_000_000):
    """Exhaustive MaxSim, chunked so the [q_tokens, b, L] similarity tensor stays under `elems` elements."""
    Q, Lq, D = ql.shape
    b, L, _ = dl.shape
    qn = qm.sum(1).clamp_min(1).float()
    res = torch.empty(Q, b, device=DEV)
    qc = max(1, min(q_chunk, elems // max(1, Lq * b * L)))
    neg = -1e4
    for s in range(0, Q, qc):
        q = ql[s:s + qc].reshape(-1, D)
        sim = torch.einsum("td,bkd->tbk", q, dl)
        sim = sim.masked_fill(~dm[None], neg).amax(-1).float()  # [t, b]
        sim = sim.view(-1, Lq, b) * qm[s:s + qc, :, None].float()
        res[s:s + qc] = sim.sum(1) / qn[s:s + qc, None]
    return res


def encode_queries(enc: Enc, texts, max_len=128):
    """All query reps in memory: dense [Q, 768], late [Q, Lq, 128] (zero-padded), mask [Q, Lq]."""
    dense = [None] * len(texts)
    parts = []

    def fn(idx, d, l, m):
        for r, j in enumerate(idx):
            dense[j] = d[r]
        parts.append((idx, l, m))

    enc.run(texts, len(QUERY_PROMPT), max_len, fn)
    Lq = max(p[1].shape[1] for p in parts)
    late = torch.zeros(len(texts), Lq, parts[0][1].shape[-1], dtype=torch.bfloat16, device=DEV)
    mask = torch.zeros(len(texts), Lq, dtype=torch.bool, device=DEV)
    for idx, l, m in parts:
        ii = torch.tensor(idx, device=DEV)
        late[ii, :l.shape[1]] = l
        mask[ii, :m.shape[1]] = m
    return torch.stack(dense), late, mask


def load_model(a):
    tok = Tok(a.model)
    m = EG2Dual.load(a.model).to(DEV).eval()
    if a.late_head:
        from safetensors.torch import load_file
        p = Path(a.late_head)
        cfg = json.loads(p.with_suffix(".json").read_text()) if p.with_suffix(".json").exists() else {}
        lh = LateHead(**cfg)
        lh.load_state_dict(load_file(p))
        m.late = lh.to(DEV).eval()
    elif not m.has_trained_late:
        print("WARNING: no trained late head; late scores use a random head", file=sys.stderr)
    return m, tok


# ---------------------------------------------------------------- NanoBEIR
NANO = ["NanoArguAna", "NanoClimateFEVER", "NanoDBPedia", "NanoFEVER", "NanoFiQA2018", "NanoHotpotQA", "NanoMSMARCO",
        "NanoNFCorpus", "NanoNQ", "NanoQuoraRetrieval", "NanoSCIDOCS", "NanoSciFact", "NanoTouche2020"]


def cmd_nanobeir(a):
    import pyarrow.parquet as pq
    m, tok = load_model(a)
    enc = Enc(m, tok)
    res, per = {}, {}
    for ds in NANO:
        t0 = time.time()
        corpus = pq.read_table(next((a.data / "corpus").glob(f"{ds}-*.parquet"))).to_pylist()
        queries = pq.read_table(next((a.data / "queries").glob(f"{ds}-*.parquet"))).to_pylist()
        qrels = pq.read_table(next((a.data / "qrels").glob(f"{ds}-*.parquet"))).to_pylist()
        rel = {}
        for r in qrels:
            qid = str(r.get("query-id", r.get("query_id")))
            did = str(r.get("corpus-id", r.get("corpus_id")))
            rel.setdefault(qid, {})[did] = int(r.get("score", 1) or 1)
        dids = [str(d["_id"]) for d in corpus]
        dtexts = [DOC_PROMPT + ((d.get("title") or "").strip() + " " + (d.get("text") or "")).strip() for d in corpus]
        queries = [q for q in queries if str(q["_id"]) in rel]
        qids = [str(q["_id"]) for q in queries]
        qd, ql, qm = encode_queries(enc, [QUERY_PROMPT + q["text"] for q in queries])
        D = torch.zeros(len(dtexts), qd.shape[1], device=DEV)
        store = LateStore()

        def fn(idx, d, l, mk):
            D[torch.tensor(idx, device=DEV)] = d
            store.add(idx, l, mk)

        enc.run(dtexts, len(DOC_PROMPT), a.d_len, fn)
        out = {}
        for head, sc in (("dense", qd @ D.T), ("late", store.scores(ql, qm, len(dtexts)))):
            sc = sc.float()
            top = torch.topk(sc, min(110, sc.shape[1]), dim=1).indices.cpu().numpy()
            vals = []
            for i, q in enumerate(qids):
                order = [dids[j] for j in top[i] if dids[j] != q][:10]  # ignore identical ids (BEIR)
                vals.append(ndcg_at_10(order, rel[q]))
            out[head] = round(float(np.mean(vals)), 4)
            per[f"{ds}/{head}"] = vals
        out.update({"queries": len(qids), "docs": len(dids), "s": round(time.time() - t0, 1)})
        res[ds] = out
        print(ds, json.dumps(out), flush=True)
        # free this dataset's GPU state before the next one (the old locals stay alive until reassigned)
        del store, D, qd, ql, qm, fn
        torch.cuda.empty_cache()
    res["_average"] = {h: round(float(np.mean([res[d][h] for d in NANO])), 4) for h in ("dense", "late")}
    print(json.dumps(res["_average"]))
    a.out.write_text(json.dumps({"results": res, "per_query": per}, indent=1))


# ---------------------------------------------------------------- ConTEB
CONTEB = ["covid-qa", "geography", "esg-reports", "insurance", "football", "squad-conteb-eval", "narrative-qa",
          "mldr-conteb-eval"]


def cmd_conteb(a):
    m, tok = load_model(a)
    enc = Enc(m, tok)
    res, per = {}, {}
    for ds in CONTEB:
        w = a.work / ds
        if not (w / "naive.jsonl").exists():
            continue
        t0 = time.time()
        gold = json.loads((w / "gold.json").read_text())
        qrows = [json.loads(x) for x in open(w / "queries.jsonl")]
        naive = [json.loads(x) for x in open(w / "naive.jsonl")]
        cid = [r["id"] for r in naive]
        ci = {c: i for i, c in enumerate(cid)}
        qd, ql, qm = encode_queries(enc, [r["text"] for r in qrows])
        # naive: each chunk alone (empty chunks -> never retrieved, as in eval_conteb.py)
        D = torch.zeros(len(cid), qd.shape[1], device=DEV)
        store = LateStore()
        keep = [i for i, r in enumerate(naive) if r["text"][len(DOC_PROMPT):].strip()]

        def fn(idx, d, l, mk):
            gi = [keep[j] for j in idx]
            D[torch.tensor(gi, device=DEV)] = d
            store.add(gi, l, mk)

        enc.run([naive[i]["text"] for i in keep], len(DOC_PROMPT), 2048, fn)
        scores = {"naive/dense": qd @ D.T, "naive/late": store.scores(ql, qm, len(cid))}
        # late chunking: whole document pass (8k windows), pool each chunk span
        Dl = torch.zeros(len(cid), qd.shape[1], device=DEV)
        store_l = LateStore()
        docs = [json.loads(x) for x in open(w / "late.jsonl")]
        for d0 in range(0, len(docs), 64):
            grp = docs[d0:d0 + 64]
            wins, spans = [], []
            for r in grp:
                ws, sp = late_windows(tok.tk, r["text"], r["prefix_chars"], r["spans"])
                base = len(wins)
                wins += ws
                spans += [(ci[s["id"]], base + wi, lo, hi) for s, (wi, lo, hi) in zip(r["spans"], sp)]
            hs = enc.windows(wins)
            for c, wi, lo, hi in spans:
                h = hs[wi][lo:hi]
                Dl[c] = F.normalize(h.float().mean(0), dim=-1)
                with torch.autocast("cuda", dtype=torch.bfloat16):
                    lt = m.late(h).to(torch.bfloat16)
                store_l.add([c], lt.unsqueeze(0), torch.ones(1, lt.shape[0], dtype=torch.bool, device=DEV))
        # merge single-span batches into padded batches for speed
        store_l = merge_store(store_l)
        scores["late/dense"] = qd @ Dl.T
        scores["late/late"] = store_l.scores(ql, qm, len(cid))
        out = {}
        for k, sc in scores.items():
            sc = sc.float()
            top = torch.topk(sc, min(10, sc.shape[1]), dim=1).indices.cpu().numpy()
            vals = []
            for i, q in enumerate(qrows):
                g = gold[q["id"]]
                vals.append(ndcg_at_10([cid[j] for j in top[i]], {x: 1 for x in g}))
            out[k] = round(float(np.mean(vals)), 4)
            per[f"{ds}/{k}"] = vals
        out.update({"queries": len(qrows), "chunks": len(cid), "s": round(time.time() - t0, 1)})
        res[ds] = out
        print(ds, json.dumps(out), flush=True)
        # free this dataset's GPU state before the next one (the old locals stay alive until reassigned)
        del scores, store, store_l, D, Dl, qd, ql, qm, hs, fn
        torch.cuda.empty_cache()
    keys = ["naive/dense", "naive/late", "late/dense", "late/late"]
    res["_average"] = {k: round(float(np.mean([v[k] for d, v in res.items() if k in v])), 4) for k in keys}
    print(json.dumps(res["_average"]))
    a.out.write_text(json.dumps({"results": res, "per_query": per}, indent=1))


def merge_store(st: LateStore, per=256):
    out = LateStore()
    items = []
    for idx, l, mk in st.batches:
        items.append((int(idx[0]), l[0], mk[0]))
    items.sort(key=lambda x: -x[1].shape[0])
    for s in range(0, len(items), per):
        grp = items[s:s + per]
        L = grp[0][1].shape[0]
        lt = torch.zeros(len(grp), L, grp[0][1].shape[-1], dtype=torch.bfloat16, device=DEV)
        mk = torch.zeros(len(grp), L, dtype=torch.bool, device=DEV)
        for r, (_, l, m) in enumerate(grp):
            lt[r, :l.shape[0]] = l
            mk[r, :l.shape[0]] = m
        out.batches.append((torch.tensor([g[0] for g in grp], device=DEV), lt, mk))
    return out


# ---------------------------------------------------------------- 98k PMC: abstract sentence -> body
def iter_units(root: Path):
    for f in sorted(root.glob("runs/*/ready/*/items.jsonl.gz")):
        for line in gzip.open(f, "rt"):
            r = json.loads(line)
            if r.get("kind") == "text" and (r.get("text") or "").strip():
                yield r


def cmd_pmc(a):
    m, tok = load_model(a)
    enc = Enc(m, tok)
    qs = json.loads((a.q / "queries.json").read_text())
    qtext = {json.loads(x)["id"]: json.loads(x)["text"] for x in open(a.q / "queries.jsonl")}
    tasks = ["abstract->body", "title->article"]
    texts, arts, abst = [], [], []
    for r in iter_units(a.root):
        et = r.get("embed_text")
        texts.append(et if et is not None else DOC_PROMPT + r["text"])
        arts.append(r["article_id"])
        abst.append(bool(r.get("is_abstract")))
    if a.limit:
        texts, arts, abst = texts[:a.limit], arts[:a.limit], abst[:a.limit]
    art_ids = sorted(set(arts))
    ai = {x: i for i, x in enumerate(art_ids)}
    art_idx = torch.tensor([ai[x] for x in arts], device=DEV)
    is_abs = torch.tensor(abst, device=DEV)
    print(json.dumps({"units": len(texts), "articles": len(art_ids)}), flush=True)
    sel = {t: [q for q in qs if q["task"] == t and q["article_id"] in ai] for t in tasks}
    allq = sel[tasks[0]] + sel[tasks[1]]
    qd, ql, qm = encode_queries(enc, [qtext[q["qid"]] for q in allq])
    nA = len(sel[tasks[0]])
    acc = {(t, h): torch.full((len(sel[t]), len(art_ids)), -1e4, device=DEV) for t in tasks for h in ("dense", "late")}
    t0, done = time.time(), [0]

    def fn(idx, d, l, mk):
        ii = torch.tensor(idx, device=DEV)
        L = int(mk.sum(1).max().item())
        key = (~mk).to(torch.int8)
        perm = torch.sort(key, dim=1, stable=True).indices[:, :L]
        l = torch.gather(l, 1, perm.unsqueeze(-1).expand(-1, -1, l.shape[-1]))
        mk = torch.gather(mk, 1, perm)
        sd = qd @ d.T
        sl = maxsim_big(ql, qm, l, mk)
        aidx = art_idx[ii]
        for t, q0, q1 in ((tasks[0], 0, nA), (tasks[1], nA, len(allq))):
            keep = ~is_abs[ii] if t == "abstract->body" else torch.ones_like(is_abs[ii])
            if not keep.any():
                continue
            for h, s in (("dense", sd), ("late", sl)):
                src = s[q0:q1][:, keep]
                acc[(t, h)].scatter_reduce_(1, aidx[keep].unsqueeze(0).expand(q1 - q0, -1), src, reduce="amax")
        done[0] += len(idx)
        if done[0] // 50000 != (done[0] - len(idx)) // 50000:
            print(json.dumps({"units_done": done[0], "units_per_s": round(done[0] / (time.time() - t0))}), flush=True)

    enc.run(texts, len(DOC_PROMPT), a.d_len, fn)
    res, per = {"units": len(texts), "articles": len(art_ids), "s": round(time.time() - t0)}, {}
    for t in tasks:
        tgt = torch.tensor([ai[q["article_id"]] for q in sel[t]], device=DEV)
        for h in ("dense", "late"):
            sc = acc[(t, h)]
            ts = sc.gather(1, tgt[:, None])
            rank = (sc > ts).sum(1) + 1
            r = rank.float().cpu().numpy()
            rr = np.where(r <= 10, 1 / r, 0.0)
            res.setdefault(t, {})[h] = {"n": len(r), "R@1": float(np.mean(r <= 1)), "R@10": float(np.mean(r <= 10)),
                                        "R@100": float(np.mean(r <= 100)), "MRR@10": float(rr.mean())}
            per[f"{t}/{h}"] = rr.tolist()
    print(json.dumps({t: res[t] for t in tasks}, indent=1))
    a.out.write_text(json.dumps({"results": res, "per_query": per}, indent=1))


# ---------------------------------------------------------------- S3 figures
def rank_metrics(scores: torch.Tensor, gold: list[set[int]], ks=(1, 5, 10)):
    """scores [Q, N]; gold[q] = set of relevant columns -> R@k, MRR@10 (binary relevance)."""
    top = scores.topk(min(max(ks), scores.shape[1]), dim=1).indices.tolist()
    out = {f"R@{k}": 0.0 for k in ks}
    mrr = 0.0
    for t, g in zip(top, gold):
        for k in ks:
            out[f"R@{k}"] += float(any(x in g for x in t[:k]))
        mrr += next((1.0 / (r + 1) for r, x in enumerate(t[:10]) if x in g), 0.0)
    n = max(1, len(gold))
    return {**{k: round(v / n, 4) for k, v in out.items()}, "MRR@10": round(mrr / n, 4)}


def encode_images(m, inp, store, idx, mb=16):
    """Image docs -> dense [N, 768] fp32, LateStore of their soft-token vectors."""
    from .images import slice_images
    dense = torch.zeros(len(idx), m.backbone.config.embedding_dim, device=DEV)
    st = LateStore()
    for s0 in range(0, len(idx), mb):
        ii = list(range(s0, min(s0 + mb, len(idx))))
        x = slice_images(inp([store.image(idx[j]) for j in ii]), 0, len(ii))
        x = {k: v.to(DEV) for k, v in x.items()}
        with torch.autocast("cuda", dtype=torch.bfloat16):
            d, lt = m.forward_mm(x["input_ids"], x["attention_mask"], x["late_mask"], x["pixel_values"],
                                 x["image_position_ids"])
        dense[ii] = d.float()
        st.add(ii, lt.to(torch.bfloat16), x["late_mask"])
    return dense, st


def cmd_figures(a):
    """caption -> figure and figure -> caption (and mention -> figure when the manifest has mentions), both heads.
    The figure side is the image only (images.IMAGE_DOC_TEXT); captions are queries (QUERY_PROMPT) for c->f and
    documents (DOC_PROMPT) for f->c, where the figure's soft tokens are the MaxSim query tokens."""
    from .images import FigureStore, ImageEncoderInputs
    root = a.manifest.parent
    store = FigureStore(root, a.manifest.name)
    idx = list(range(len(store)))[: a.limit or None]
    tok = Tok(a.model)
    m = EG2Dual.load(a.model, vision=True).to(DEV).eval()
    if not m.has_trained_late:
        print("WARNING: no trained late head; late scores use a random head", file=sys.stderr)
    enc = Enc(m, tok)
    inp = ImageEncoderInputs(a.model, a.image_doc_text or "{image}")
    t0 = time.time()
    fd, fstore = encode_images(m, inp, store, idx)
    caps = [store.caption[i] for i in idx]
    res = {"figures": len(idx)}
    # caption -> figure
    qd, ql, qm = encode_queries(enc, [QUERY_PROMPT + c for c in caps], max_len=a.q_len)
    gold = [{j} for j in range(len(idx))]
    res["caption->figure"] = {"dense": rank_metrics(qd @ fd.T, gold), "late": rank_metrics(fstore.scores(ql, qm, len(idx)), gold)}
    # mention -> figure
    rows = [json.loads(x) for x in open(a.manifest)][: len(idx)]
    men = [(j, t) for j, r in enumerate(rows) for t in (r.get("mentions") or [])]
    if men:
        md, ml, mm = encode_queries(enc, [QUERY_PROMPT + t for _, t in men], max_len=a.q_len)
        g = [{j} for j, _ in men]
        res["mention->figure"] = {"queries": len(men), "dense": rank_metrics(md @ fd.T, g),
                                  "late": rank_metrics(fstore.scores(ml, mm, len(idx)), g)}
    # figure -> caption: captions as documents
    cd = torch.zeros(len(idx), fd.shape[1], device=DEV)
    cst = LateStore()

    def fn(ii, d, lt, lm):
        cd[ii] = d
        cst.add(ii, lt, lm)

    enc.run([DOC_PROMPT + c for c in caps], len(DOC_PROMPT), a.d_len, fn)
    fl = torch.zeros(len(idx), max(b[1].shape[1] for b in fstore.batches), ql.shape[-1], dtype=torch.bfloat16, device=DEV)
    fm = torch.zeros(fl.shape[:2], dtype=torch.bool, device=DEV)
    for ii, lt, lm in fstore.batches:
        fl[ii, :lt.shape[1]] = lt
        fm[ii, :lm.shape[1]] = lm
    res["figure->caption"] = {"dense": rank_metrics(fd @ cd.T, gold), "late": rank_metrics(cst.scores(fl, fm, len(idx)), gold)}
    res["s"] = round(time.time() - t0, 1)
    a.out.parent.mkdir(parents=True, exist_ok=True)
    a.out.write_text(json.dumps(res, indent=1))
    print(json.dumps(res, indent=1))


def cmd_compare(a):
    A = json.loads(a.a.read_text())["per_query"]
    B = json.loads(a.b.read_text())["per_query"]
    rng = np.random.default_rng(0)
    for k in sorted(set(A) & set(B)):
        x, y = np.array(A[k]), np.array(B[k])
        if len(x) != len(y):
            continue
        d = y - x
        bs = [d[rng.integers(0, len(d), len(d))].mean() for _ in range(2000)]
        print(f"{k:40s} A {x.mean():.4f}  B {y.mean():.4f}  B-A {d.mean():+.4f} "
              f"({np.percentile(bs, 2.5):+.4f}, {np.percentile(bs, 97.5):+.4f})")


def main():
    ap = argparse.ArgumentParser()
    sp = ap.add_subparsers(dest="cmd", required=True)
    for name in ("nanobeir", "conteb", "pmc"):
        p = sp.add_parser(name)
        p.add_argument("--model", required=True)
        p.add_argument("--late-head")
        p.add_argument("--out", type=Path, required=True)
        p.add_argument("--d-len", type=int, default=512 if name == "nanobeir" else 1024)
        if name == "nanobeir":
            p.add_argument("--data", type=Path, required=True)
        if name == "conteb":
            p.add_argument("--work", type=Path, required=True)
        if name == "pmc":
            p.add_argument("--root", type=Path, required=True)
            p.add_argument("--q", type=Path, required=True)
            p.add_argument("--limit", type=int, default=0)
    f = sp.add_parser("figures")
    f.add_argument("--model", required=True)
    f.add_argument("--manifest", type=Path, required=True, help="held-out figures.jsonl (images relative to it)")
    f.add_argument("--out", type=Path, required=True)
    f.add_argument("--limit", type=int, default=0)
    f.add_argument("--q-len", type=int, default=128)
    f.add_argument("--d-len", type=int, default=512)
    f.add_argument("--image-doc-text", default=None)
    c = sp.add_parser("compare")
    c.add_argument("a", type=Path)
    c.add_argument("b", type=Path)
    a = ap.parse_args()
    torch.backends.cuda.matmul.allow_tf32 = True
    # no_grad everywhere: the late head has trainable params, so without it every MaxSim chunk keeps its similarity
    # tensor alive for a backward that never runs (this OOMed the first ConTEB runs)
    with torch.no_grad():
        {"nanobeir": cmd_nanobeir, "conteb": cmd_conteb, "pmc": cmd_pmc, "figures": cmd_figures,
         "compare": cmd_compare}[a.cmd](a)


if __name__ == "__main__":
    main()
