"""S1: train the late-interaction head on EG2 alongside the dense head (1..n GPUs, GradCache).

Loss per global batch (B queries per GPU x W GPUs; N = B * (1 + n_neg) documents per GPU; in-batch + cross-GPU +
mined hard negatives; each GPU's batch is single-source):
  dense  : InfoNCE on cosine / tau_d at 768-d, plus 0.25 x each MRL truncation (512, 256, 128)
  late   : InfoNCE on mean-MaxSim / tau_l
  align  : w_align * symmetric KL between the dense and late score distributions (jina-v4 style)
  kd     : w_kd_dense * KL(teacher || dense) + w_kd_late * KL(teacher || late). Teacher = scores over the same
           candidates; default = base EG2 dense (frozen vectors from the mining pass), an anti-forgetting anchor.
           A cleared cross-encoder can replace it (same [B, N] matrix).
  l2sp   : optional decoupled pull of the backbone towards the base weights (anti-forgetting), after each step.
False negatives: a candidate whose id hash is in query i's gold set (other than its label) is masked for row i,
across GPUs too (stable 62-bit id hashes).

GradCache: 1) encode every query/doc without grad, 2) all-gather the reps, compute the loss on detached leaf reps
and get their grads, 3) re-encode mini-batch by mini-batch with grad and backprop the local slice of the cached
rep-grads; gradients are summed across GPUs (all_reduce), which gives the exact full-batch gradient.

Attention: --attn sdpa (padded micro-batches) or --pack-tokens T (flex attention over packed rows of T tokens:
block-diagonal per sequence, positions restart per sequence; no padding compute).

Data dir layout (from prep_data.py + mine.py):
  <data>/<source>/queries.jsonl  {"id","text"}          text already prompted (QUERY_PROMPT + query)
  <data>/<source>/docs.jsonl     {"id","text"}          text already prompted (DOC_PROMPT + doc)
  <data>/<source>/triples.jsonl  {"q","pos","gold":[...],"negs":[...]}   ids
  <data>/<source>/qvec, dvec     flashembed outputs (embeddings.dense.npy + embeddings.ids.json), base EG2
Multi-GPU: torchrun --nproc_per_node=W -m eg2late.train_s1 ...
"""
from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import math
import os
import random
import time
from pathlib import Path

import numpy as np
import torch
import torch.distributed as dist
import torch.nn.functional as F
from torch.utils.checkpoint import checkpoint

from .model import DOC_PROMPT, QUERY_PROMPT, EG2Dual, Tok, pack

MRL = (512, 256, 128)
MAX_GOLD = 8


def sid(x: str) -> int:
    """Stable 62-bit id hash (Python's hash() is salted per process)."""
    return int.from_bytes(hashlib.blake2b(x.encode(), digest_size=8).digest(), "little") & ((1 << 62) - 1)


from .dist_utils import all_reduce_grads, gather_cat, grank, gworld, init_gather_groups, rank, world  # noqa: E402,F401


def load_vecs(d: Path):
    ids = json.loads((d / "embeddings.ids.json").read_text())
    ids = ids["ids"] if isinstance(ids, dict) else ids
    return {x: i for i, x in enumerate(ids)}, np.load(d / "embeddings.dense.npy", mmap_mode="r")


class DocStore:
    """Document texts: a memory-mapped blob (docs.blob + docs.off.npy + docs.ids.json, from prep_pmc compact) or,
    for small sources, docs.jsonl in memory. Index by integer position; `ids` holds the string ids."""

    def __init__(self, root: Path):
        if (root / "docs.off.npy").exists():
            self.ids = json.loads((root / "docs.ids.json").read_text())
            self.off = np.load(root / "docs.off.npy", mmap_mode="r")
            self.blob = np.memmap(root / "docs.blob", dtype=np.uint8, mode="r")
            self.texts = None
        else:
            self.ids, self.texts = [], []
            for line in open(root / "docs.jsonl"):
                r = json.loads(line)
                self.ids.append(r["id"])
                self.texts.append(r["text"])
        self.index = {x: i for i, x in enumerate(self.ids)}
        self.group = None
        if (root / "docs.groups.json").exists():
            self.group = json.loads((root / "docs.groups.json").read_text())

    def __len__(self):
        return len(self.ids)

    def text(self, i: int) -> str:
        if self.texts is not None:
            return self.texts[i]
        return bytes(self.blob[self.off[i]:self.off[i + 1]]).decode()


class Source:
    """One training source. Triples are held as numpy arrays (fork-friendly for DataLoader workers):
    q [n] (index into qids), pos [n], negs [n, K] (-1 = none), gold as CSR (gold_ptr, gold_ids), group hash [n]."""

    def __init__(self, root: Path, max_q: int | None, seed: int):
        self.name = root.name
        self.ns = (root / "corpus").read_text().strip() if (root / "corpus").exists() else root.name
        # source.json (optional): {"symmetric": bool, "q_prompt": str, "d_prompt": str, "licence": ..., "origin": ...}
        meta = json.loads((root / "source.json").read_text()) if (root / "source.json").exists() else {}
        self.symmetric = bool(meta.get("symmetric", False))
        self.q_prompt = meta.get("q_prompt", meta.get("prompt", QUERY_PROMPT))
        self.d_prompt = meta.get("d_prompt", self.q_prompt if self.symmetric else DOC_PROMPT)
        if (root / "figures.jsonl").exists():  # S3: documents are figure images
            from .images import FigureStore
            self.store = FigureStore(root)
        else:
            self.store = DocStore(root)
        self.qids, self.qtext = [], []
        for line in open(root / "queries.jsonl"):
            r = json.loads(line)
            self.qids.append(r["id"])
            self.qtext.append(r["text"])
        qindex = {x: i for i, x in enumerate(self.qids)}
        di = self.store.index
        rows = []
        for line in open(root / "triples.jsonl"):
            t = json.loads(line)
            if t["pos"] in di and t["q"] in qindex:
                rows.append(t)
        rng = random.Random(seed)
        rng.shuffle(rows)
        if max_q:
            rows = rows[:max_q]
        K = max([len(t["negs"]) for t in rows] + [1])
        self.q = np.array([qindex[t["q"]] for t in rows], dtype=np.int64)
        self.pos = np.array([di[t["pos"]] for t in rows], dtype=np.int64)
        self.negs = np.full((len(rows), K), -1, dtype=np.int64)
        for r, t in enumerate(rows):
            n = [di[x] for x in t["negs"] if x in di]
            self.negs[r, :len(n)] = n
        gold_ptr, gold_ids = [0], []
        for t in rows:  # gold as hashes (gold docs need no text)
            g = [t["pos"]] + [x for x in (t.get("gold") or []) if x != t["pos"]]
            gold_ids += [sid(f"{self.ns}:{x}") for x in g[:MAX_GOLD]]
            gold_ptr.append(len(gold_ids))
        self.gold_ptr, self.gold_ids = np.array(gold_ptr), np.array(gold_ids, dtype=np.int64)
        self.qgroup = np.array([sid(f"g:{t['group']}") if t.get("group") else -1 for t in rows], dtype=np.int64)
        self.doc_hash = None  # computed lazily per doc
        self.qi, self.qv = load_vecs(root / "qvec") if (root / "qvec").exists() else (None, None)
        tv = root / "tvec" if (root / "tvec").exists() else root / "dvec"
        self.di, self.dv = load_vecs(tv) if tv.exists() else (None, None)

    def __len__(self):
        return len(self.q)

    def subset(self, sl: slice) -> "Source":
        v = Source.__new__(Source)
        v.__dict__.update(self.__dict__)
        idx = np.arange(len(self))[sl]
        v.q, v.pos, v.negs, v.qgroup = self.q[idx], self.pos[idx], self.negs[idx], self.qgroup[idx]
        v.gold_ptr = np.concatenate([[0], np.cumsum(self.gold_ptr[idx + 1] - self.gold_ptr[idx])])
        v.gold_ids = np.concatenate([self.gold_ids[self.gold_ptr[i]:self.gold_ptr[i + 1]] for i in idx]) \
            if len(idx) else np.zeros(0, dtype=np.int64)
        return v


class Batches(torch.utils.data.Dataset):
    def __init__(self, sources, plan, tok: Tok, a):
        self.sources, self.plan, self.tok, self.a = sources, plan, tok, a

    def __len__(self):
        return len(self.plan)

    def __getitem__(self, i):
        si, rows, seed = self.plan[i]
        s, a = self.sources[si], self.a
        rng = random.Random(seed)
        rows = np.asarray(rows)
        qx = s.q[rows]
        dx = list(s.pos[rows])
        for r in rows:
            negs = [int(x) for x in s.negs[r, : a.neg_pool] if x >= 0]
            negs = rng.sample(negs, min(a.n_neg, len(negs)))
            while len(negs) < a.n_neg:  # rare: pad with random docs of the source
                negs.append(rng.randrange(len(s.store)))
            dx += negs
        gold = torch.full((len(rows), MAX_GOLD), -1, dtype=torch.long)
        for j, r in enumerate(rows):
            g = s.gold_ids[s.gold_ptr[r]:s.gold_ptr[r + 1]]
            gold[j, :len(g)] = torch.from_numpy(g)
        ids = [s.store.ids[x] for x in dx]
        doc_hash = torch.tensor([sid(f"{s.ns}:{x}") for x in ids], dtype=torch.long)
        if s.store.group is not None:
            dgroup = torch.tensor([sid(f"g:{s.store.group[x]}") if s.store.group[x] else -1 for x in dx])
        else:
            dgroup = torch.full((len(dx),), -1, dtype=torch.long)
        qgroup = torch.from_numpy(s.qgroup[rows])
        q = self.tok([s.qtext[x] for x in qx], len(s.q_prompt), a.q_len)
        if getattr(s.store, "is_image", False):
            if getattr(self, "_img", None) is None:  # one processor per DataLoader worker
                from .images import IMAGE_DOC_TEXT, ImageEncoderInputs
                self._img = ImageEncoderInputs(a.base, getattr(a, "image_doc_text", None) or IMAGE_DOC_TEXT)
            d = self._img([s.store.image(x) for x in dx])
        else:
            d = self.tok([s.store.text(x) for x in dx], len(s.d_prompt), a.d_len)
        q.pop("offsets", None)
        d.pop("offsets", None)
        tq = td = None
        if s.qv is not None and s.dv is not None:
            tq = torch.from_numpy(np.stack([s.qv[s.qi[s.qids[x]]] for x in qx]).astype(np.float32))
            td = torch.from_numpy(np.stack([s.dv[s.di[x]] for x in ids]).astype(np.float32))
        out = {"q": q, "d": d, "gold": gold, "doc_hash": doc_hash, "qgroup": qgroup, "dgroup": dgroup,
               "tq": tq, "td": td, "source": s.name, "sym": torch.full((len(rows),), s.symmetric)}
        if getattr(a, "pack_tokens", 0):  # packed micro-batches (flex attention), built here on the CPU workers
            out["qp"] = pack(q, a.pack_tokens_q)
            out["dp"] = None if "pixel_values" in d else pack(d, a.pack_tokens)
        return out


def make_plan(sources, B, seed, epochs=1, mix: dict | None = None, steps: int = 0):
    """List of (source index, rows, seed). Without `mix`: every full batch of every source, `epochs` times, shuffled.
    With `mix` {name: weight} and `steps`: sources drawn by weight per batch; each source cycles through its rows
    (reshuffled every pass), so small sources are replayed and big ones subsampled."""
    rng = random.Random(seed)
    if not mix:
        plan = []
        for ep in range(epochs):
            for si, s in enumerate(sources):
                idx = list(range(len(s)))
                rng.shuffle(idx)
                for b in range(0, len(idx) - B + 1, B):
                    plan.append((si, idx[b:b + B], rng.randrange(1 << 30)))
        rng.shuffle(plan)
        return plan
    names = [s.name for s in sources]
    w = [float(mix.get(n, 0.0)) for n in names]
    assert sum(w) > 0, f"mix {mix} matches none of {names}"
    order = {si: [] for si in range(len(sources))}
    plan = []
    for si in rng.choices(range(len(sources)), weights=w, k=steps):
        if len(order[si]) < B:
            idx = list(range(len(sources[si])))
            rng.shuffle(idx)
            order[si] = idx  # drop the remainder of the previous pass
        rows, order[si] = order[si][:B], order[si][B:]
        plan.append((si, rows, rng.randrange(1 << 30)))
    return plan


def slice_batch(x, s, e):
    return {k: v[s:e] for k, v in x.items() if k != "offsets"}


def to_dev(p, dev="cuda"):
    return {k: (v.to(dev, non_blocking=True) if torch.is_tensor(v) else v) for k, v in p.items()}


def encode(model, x, mb, grad=False, dev="cuda"):
    dense, late = [], []
    for s in range(0, x["input_ids"].shape[0], mb):
        b = {k: v.to(dev, non_blocking=True) for k, v in slice_batch(x, s, s + mb).items()}
        with torch.set_grad_enabled(grad), _ac(dev):
            dn, lt = model(b["input_ids"], b["attention_mask"], b["late_mask"])
        dense.append(dn.float())
        late.append(lt.to(torch.bfloat16))
    return torch.cat(dense), torch.cat(late)


def encode_packed(model, x, packs, grad=False, dev="cuda"):
    """Same outputs as `encode` (dense [N, 768] fp32, late [N, L, 128] bf16 in the padded layout) from packed
    micro-batches; `packs` must already be on the device (their flex masks are cached in place)."""
    N, L = x["input_ids"].shape
    dense = torch.zeros(N, model.backbone.config.embedding_dim, device=dev)
    late = torch.zeros(N, L, model.late.cfg["d_out"], device=dev, dtype=torch.bfloat16)
    for p in packs:
        with torch.set_grad_enabled(grad), torch.autocast("cuda", dtype=torch.bfloat16):
            dn, lt = model.forward_packed(p)
        dense[p["rows"]] = dn.float()
        late[p["tok_row"], p["tok_col"]] = lt[: p["n_real"]].to(torch.bfloat16)
    return dense, late


def _maxsim_chunk(q, qm, qn, d, dm):
    sim = torch.einsum("bqd,nkd->bnqk", q, d)
    sim = sim.masked_fill(~dm[None, :, None, :], -1e4).amax(-1)  # [B, n, Lq]
    return (sim * qm[:, None, :].to(sim.dtype)).sum(-1) / qn[:, None]


def maxsim_train(q, qm, d, dm, chunk=256):
    """Mean-over-query-tokens MaxSim [B, N]; each doc chunk is recomputed in backward (activation checkpoint), so the
    [B, n, Lq, Ld] similarity tensors are never kept for backward."""
    qn = qm.sum(1).clamp_min(1).to(q.dtype)
    out = []
    for s in range(0, d.shape[0], chunk):
        args = (q, qm, qn, d[s:s + chunk], dm[s:s + chunk])
        out.append(checkpoint(_maxsim_chunk, *args, use_reentrant=False) if torch.is_grad_enabled()
                   else _maxsim_chunk(*args))
    return torch.cat(out, 1)


def kl(p_log, q_log):  # KL(p || q), rows are distributions
    return (p_log.exp() * (p_log - q_log)).sum(-1).mean()


def loss_fn(qd, ql, qm, dd, dl, dm, forbid, teacher, lab, a, sym=None, qgroup=None):
    """sym [B] bool: rows from symmetric sources also get the reverse loss (positive doc -> its query among the
    batch's queries), weight a.w_rev. a.qq_neg > 0 adds the other queries as dense negatives (GTE-style), masking any
    whose score exceeds the positive's by more than a.qq_margin (false negatives, as in Qwen3-Embedding)."""
    big = -1e4

    def logits(s, tau):
        return (s / tau).masked_fill(forbid, big)

    sd = qd @ dd.T
    ld = logits(sd, a.tau_d)
    if getattr(a, "qq_neg", 0) > 0:
        sqq = (qd @ qd.T) / a.tau_d
        pos = ld.gather(1, lab[:, None])
        bad = torch.eye(len(lab), dtype=torch.bool, device=ld.device) | (sqq > pos + a.qq_margin / a.tau_d)
        if qgroup is not None:
            bad |= (qgroup[:, None] == qgroup[None, :]) & (qgroup[:, None] >= 0)
        loss_d = F.cross_entropy(torch.cat([ld, sqq.masked_fill(bad, big)], 1), lab)
    else:
        loss_d = F.cross_entropy(ld, lab)
    for k in MRL:
        s_k = F.normalize(qd[:, :k], dim=-1) @ F.normalize(dd[:, :k], dim=-1).T
        loss_d = loss_d + 0.25 * F.cross_entropy(logits(s_k, a.tau_d), lab)
    sl = maxsim_train(ql, qm, dl, dm).float()
    ll = logits(sl, a.tau_l)
    loss_l = F.cross_entropy(ll, lab)
    pd, pl = F.log_softmax(ld, -1), F.log_softmax(ll, -1)
    align = 0.5 * (kl(pd, pl) + kl(pl, pd))
    out = {"dense": loss_d, "late": loss_l, "align": align}
    total = a.w_dense * loss_d + a.w_late * loss_l + a.w_align * align
    if sym is not None and bool(sym.any()) and getattr(a, "w_rev", 0) > 0:
        tgt = torch.arange(len(lab), device=ld.device)[sym]
        rev = F.cross_entropy(ld[:, lab].T[sym], tgt) + F.cross_entropy(ll[:, lab].T[sym], tgt)
        out["rev"] = rev
        total = total + a.w_rev * rev
    if teacher is not None and (a.w_kd_dense > 0 or a.w_kd_late > 0):
        pt = F.log_softmax(logits(teacher, a.tau_t), -1)
        kd_d, kd_l = kl(pt, pd), kl(pt, pl)
        out["kd"] = kd_d + kd_l
        total = total + a.w_kd_dense * kd_d + a.w_kd_late * kd_l
    with torch.no_grad():
        out["acc_d"] = (ld.argmax(-1) == lab).float().mean()
        out["acc_l"] = (ll.argmax(-1) == lab).float().mean()
    out["total"] = total
    return out


def targets(gold, doc_hash, B, N, tq, td, W, qgroup=None, dgroup=None):
    """Forbid mask [W*B, W*N], labels [W*B], teacher [W*B, W*N] or None, over a (gathered) batch. A candidate is
    forbidden for query i if it is one of i's gold docs or shares i's paper group (PMC), unless it is i's label."""
    dev = gold.device
    lab = torch.cat([torch.arange(B, device=dev) + r * N for r in range(W)])
    forbid = (gold[:, :, None] == doc_hash[None, None, :]).any(1)
    if qgroup is not None:
        forbid |= (qgroup[:, None] == dgroup[None, :]) & (qgroup[:, None] >= 0)
    forbid[torch.arange(len(lab), device=dev), lab] = False
    teacher = tq @ td.T if tq is not None else None
    return forbid, lab, teacher


def _ac(dev):
    dev = str(dev)
    return torch.autocast("cuda", dtype=torch.bfloat16) if dev.startswith("cuda") else torch.autocast("cpu", enabled=False)


def encode_img(model, x, mb, grad=False, dev="cuda"):
    """Image documents (S3): EG2Dual.forward_mm per micro-batch -> dense [N, D] fp32, late [N, L, 128] bf16."""
    from .images import slice_images
    N, L = x["input_ids"].shape
    dense, late = [], []
    for s in range(0, N, mb):
        b = {k: v.to(dev, non_blocking=True) for k, v in slice_images(x, s, s + mb).items()}
        with torch.set_grad_enabled(grad), _ac(dev):
            dn, lt = model.forward_mm(b["input_ids"], b["attention_mask"], b["late_mask"], b["pixel_values"],
                                      b["image_position_ids"])
        dense.append(dn.float())
        late.append(F.pad(lt, (0, 0, 0, L - lt.shape[1])).to(torch.bfloat16))
    return torch.cat(dense), torch.cat(late)


def side_reps(model, x, packs, mb, dev):
    """No-grad reps of one side (queries or docs): images, packed rows, or padded micro-batches."""
    if x.get("pixel_values") is not None:
        return encode_img(model, x, mb, dev=dev)
    if packs is not None:
        return encode_packed(model, x, packs, dev=dev)
    return encode(model, x, mb, dev=dev)


def side_backward(model, x, packs, mb, gd, gl, dev):
    """GradCache replay of one side: re-encode with grad and backprop sum(rep * cached grad)."""
    if x.get("pixel_values") is not None:
        from .images import slice_images
        for s in range(0, x["input_ids"].shape[0], mb):
            b = {k: v.to(dev, non_blocking=True) for k, v in slice_images(x, s, s + mb).items()}
            with _ac(dev):
                dn, lt = model.forward_mm(b["input_ids"], b["attention_mask"], b["late_mask"], b["pixel_values"],
                                          b["image_position_ids"])
            Lb = lt.shape[1]
            ((dn.float() * gd[s:s + mb]).sum() + (lt.float() * gl[s:s + mb, :Lb].float()).sum()).backward()
    elif packs is not None:
        for p in packs:
            with _ac(dev):
                dn, lt = model.forward_packed(p)
            sur = (dn.float() * gd[p["rows"]]).sum() + \
                (lt[: p["n_real"]].float() * gl[p["tok_row"], p["tok_col"]].float()).sum()
            sur.backward()
    else:
        for s in range(0, x["input_ids"].shape[0], mb):
            b = {k: v.to(dev, non_blocking=True) for k, v in slice_batch(x, s, s + mb).items()}
            with _ac(dev):
                dn, lt = model(b["input_ids"], b["attention_mask"], b["late_mask"])
            sur = (dn.float() * gd[s:s + mb]).sum() + (lt.float() * gl[s:s + mb].float()).sum()
            sur.backward()


def local_reps(model, batch, a, dev="cuda"):
    q, d = batch["q"], batch["d"]
    for k in ("qp", "dp"):
        if batch.get(k) is not None:
            batch[k] = [to_dev(p, dev) for p in batch[k]]
    qd, ql = side_reps(model, q, batch.get("qp"), a.mb_q, dev)
    dd, dl = side_reps(model, d, batch.get("dp"), a.mb_d, dev)
    return qd, ql, q["late_mask"].to(dev), dd, dl, d["late_mask"].to(dev)


def train_step(model, batch, a, dev="cuda"):
    q, d = batch["q"], batch["d"]
    # 1) no-grad local reps, then gather across GPUs
    qd, ql, qm, dd, dl, dm = local_reps(model, batch, a, dev)
    Lq, Ld, B, N = ql.shape[1], dl.shape[1], qd.shape[0], dd.shape[0]
    Qd, Ql, Qm = gather_cat(qd), gather_cat(ql, 1), gather_cat(qm.to(torch.uint8), 1).bool()
    Dd, Dl, Dm = gather_cat(dd), gather_cat(dl, 1), gather_cat(dm.to(torch.uint8), 1).bool()
    has_t = batch["tq"] is not None
    forbid, lab, teacher = targets(gather_cat(batch["gold"].to(dev)), gather_cat(batch["doc_hash"].to(dev)), B, N,
                                   gather_cat(batch["tq"].to(dev)) if has_t else None,
                                   gather_cat(batch["td"].to(dev)) if has_t else None, gworld(),
                                   gather_cat(batch["qgroup"].to(dev)), gather_cat(batch["dgroup"].to(dev)))
    leaves = [t.detach().requires_grad_(True) for t in (Qd, Ql, Dd, Dl)]
    # 2) loss on cached global reps
    sym = gather_cat(batch["sym"].to(dev)) if "sym" in batch else None
    out = loss_fn(leaves[0], leaves[1], Qm, leaves[2], leaves[3], Dm, forbid, teacher, lab, a, sym,
                  gather_cat(batch["qgroup"].to(dev)))
    out["total"].backward()
    r = grank()
    gqd = leaves[0].grad[r * B:(r + 1) * B]
    gql = leaves[1].grad[r * B:(r + 1) * B, :Lq]
    gdd = leaves[2].grad[r * N:(r + 1) * N]
    gdl = leaves[3].grad[r * N:(r + 1) * N, :Ld]
    # 3) re-encode with grad, backprop the local slice of the cached grads
    side_backward(model, q, batch.get("qp"), a.mb_q, gqd, gql, dev)
    side_backward(model, d, batch.get("dp"), a.mb_d, gdd, gdl, dev)
    # sum of a gather group's partial gradients = that group's full-batch gradient; mean over groups
    all_reduce_grads(model.parameters())
    ntok = int(q["attention_mask"].sum() + d["attention_mask"].sum())
    return {k: float(v.detach()) for k, v in out.items()}, ntok


@torch.no_grad()
def validate(model, val_batches, a):
    """Rank-local validation (rank 0 only): the training loss over each held-out batch alone."""
    acc = {}
    for vb in val_batches:
        qd, ql, qm, dd, dl, dm = local_reps(model, vb, a)
        has_t = vb["tq"] is not None
        forbid, lab, teacher = targets(vb["gold"].cuda(), vb["doc_hash"].cuda(), qd.shape[0], dd.shape[0],
                                       vb["tq"].cuda() if has_t else None, vb["td"].cuda() if has_t else None, 1,
                                       vb["qgroup"].cuda(), vb["dgroup"].cuda())
        out = loss_fn(qd, ql, qm, dd, dl, dm, forbid, teacher, lab, a,
                      vb["sym"].cuda() if "sym" in vb else None, vb["qgroup"].cuda())
        for k, v in out.items():
            acc.setdefault(k, []).append(float(v))
        for k in ("qp", "dp"):  # drop cached flex masks between validations
            for p in vb.get(k) or []:
                p.pop("masks", None)
    return {k: float(np.mean(v)) for k, v in acc.items()}


@torch.no_grad()
def init_late_head(model, sources, tok, a):
    """SVD init of the late head's linear path from document token outputs of the base model."""
    texts = []
    rng = random.Random(a.seed)
    for s in sources:
        texts += [s.store.text(i) for i in rng.sample(range(len(s.store)), min(256, len(s.store)))]
    toks = []
    for i in range(0, len(texts), 64):
        x = tok(texts[i:i + 64], len(DOC_PROMPT), a.d_len)
        with torch.autocast("cuda", dtype=torch.bfloat16):
            h = model.token_outputs(x["input_ids"].cuda(), x["attention_mask"].cuda())
        toks.append(h[x["late_mask"].cuda()].float())
    model.late.init_from_tokens(torch.cat(toks))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", required=True, help="base EG2 HF snapshot")
    ap.add_argument("--init", default=None, help="start from this exported snapshot (backbone + late head)")
    ap.add_argument("--data", type=Path, required=True)
    ap.add_argument("--sources", default="nq,hotpotqa,squad")
    ap.add_argument("--mix", default="", help="name:weight,... sample batches by source weight (needs --steps)")
    ap.add_argument("--steps", type=int, default=0, help="total optimiser steps with --mix")
    ap.add_argument("--max-q", type=int, default=0, help="max training queries per source (0 = all)")
    ap.add_argument("--val-q", type=int, default=512, help="held-out queries per source for validation")
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--batch", type=int, default=128, help="queries per GPU")
    ap.add_argument("--n-neg", type=int, default=7)
    ap.add_argument("--neg-pool", type=int, default=15)
    ap.add_argument("--mb-q", type=int, default=128)
    ap.add_argument("--mb-d", type=int, default=128)
    ap.add_argument("--q-len", type=int, default=64)
    ap.add_argument("--d-len", type=int, default=320)
    ap.add_argument("--attn", default="sdpa", help="sdpa | flex_attention | eager (backbone attention)")
    ap.add_argument("--pack-tokens", type=int, default=0, help="pack docs into rows of this many tokens (flex)")
    ap.add_argument("--pack-tokens-q", type=int, default=8192, help="packed row length for queries")
    ap.add_argument("--compile", action="store_true", help="torch.compile the backbone layers")
    ap.add_argument("--flex-small", action="store_true", help="(default now) small flex tiles")
    ap.add_argument("--flex-default", action="store_true", help="flex default tiles (fail on RTX/B200 at d=512)")
    ap.add_argument("--epochs", type=int, default=1)
    ap.add_argument("--max-steps", type=int, default=0)
    ap.add_argument("--max-minutes", type=float, default=0)
    ap.add_argument("--lr", type=float, default=2e-5)
    ap.add_argument("--lr-head", type=float, default=5e-4)
    ap.add_argument("--wd", type=float, default=0.01)
    ap.add_argument("--warmup", type=int, default=100)
    ap.add_argument("--tau-d", type=float, default=0.05)
    ap.add_argument("--tau-l", type=float, default=0.02)
    ap.add_argument("--tau-t", type=float, default=0.05)
    ap.add_argument("--w-dense", type=float, default=1.0)
    ap.add_argument("--w-late", type=float, default=1.0)
    ap.add_argument("--w-align", type=float, default=0.1)
    ap.add_argument("--w-kd", type=float, default=0.5, help="default for --w-kd-dense and --w-kd-late")
    ap.add_argument("--w-kd-dense", type=float, default=None)
    ap.add_argument("--w-kd-late", type=float, default=None)
    ap.add_argument("--l2sp", type=float, default=0.0, help="decoupled pull of the backbone to the base weights")
    ap.add_argument("--w-rev", type=float, default=1.0, help="reverse (doc -> query) loss weight on symmetric sources")
    ap.add_argument("--qq-neg", type=float, default=0.0, help=">0: other queries as dense negatives (ablation)")
    ap.add_argument("--qq-margin", type=float, default=0.1, help="q-q negatives above positive + margin are masked")
    ap.add_argument("--dry-run", action="store_true", help="print the resolved flags and exit")
    ap.add_argument("--vision", default="off", choices=["off", "frozen", "lora", "full"],
                    help="S3: keep EG2's vision tower for image sources (figures.jsonl); frozen / LoRA / full tuning")
    ap.add_argument("--vision-lora-r", type=int, default=16)
    ap.add_argument("--lr-vision", type=float, default=1e-4, help="LR for vision LoRA / tower and embed_vision")
    ap.add_argument("--image-doc-text", default=None, help="image document template ({image} = EG2 image token)")
    ap.add_argument("--mix-config", type=Path, default=None,
                    help="JSON {sources: {name: {weight}}, steps?, args?: {flag: value}}; sets --sources/--mix/--steps")
    ap.add_argument("--freeze-backbone", action="store_true", help="train the late head only (ablation)")
    ap.add_argument("--gather-group", type=int, default=0,
                    help="ranks that share negatives (all_gather); 0 = one node (LOCAL_WORLD_SIZE). Needed multi-node")
    ap.add_argument("--eval-every", type=int, default=200)
    ap.add_argument("--ckpt-every", type=int, default=250,
                    help="resumable checkpoint (fp32 weights + AdamW + step) to <out>/ckpt every N steps; a rerun with "
                         "the same --out resumes from it. Touch <out>/STOP to checkpoint and exit at the next step.")
    ap.add_argument("--start-step", type=int, default=0,
                    help="warm start without a checkpoint: with --init, skip the first N plan steps (same LR schedule, "
                         "fresh AdamW re-warmed over --warmup steps)")
    ap.add_argument("--log-every", type=int, default=10)
    ap.add_argument("--workers", type=int, default=3)
    ap.add_argument("--seed", type=int, default=0)
    ap.add_argument("--init-only", action="store_true", help="write the SVD-init late head and stop")
    a = ap.parse_args()
    if a.mix_config:
        from .mixcfg import apply_mix_config
        apply_mix_config(a, a.mix_config)
    if a.dry_run:  # resolved flags only: nothing is loaded (used by launch.py --dry-run)
        print(json.dumps({k: (str(v) if isinstance(v, Path) else v) for k, v in vars(a).items()}, indent=1))
        return
    a.w_kd_dense = a.w_kd if a.w_kd_dense is None else a.w_kd_dense
    a.w_kd_late = a.w_kd if a.w_kd_late is None else a.w_kd_late

    if "LOCAL_RANK" in os.environ:
        dist.init_process_group("nccl")
        torch.cuda.set_device(int(os.environ["LOCAL_RANK"]))
        init_gather_groups(a.gather_group)
    is0 = rank() == 0
    torch.manual_seed(a.seed)
    torch.backends.cuda.matmul.allow_tf32 = True
    a.out.mkdir(parents=True, exist_ok=True)
    log = open(a.out / "train_log.jsonl", "a") if is0 else None

    def emit(r):
        if not is0:
            return
        r["t"] = round(time.time(), 1)
        log.write(json.dumps(r) + "\n")
        log.flush()
        print(json.dumps(r), flush=True)

    tok = Tok(a.base)
    if a.pack_tokens:
        a.attn = "flex_attention"
    model = EG2Dual.load(a.init or a.base, base_snapshot=a.base, attn=a.attn, vision=a.vision != "off")
    if a.vision != "off":
        model.vision_mode(a.vision, r=a.vision_lora_r, alpha=2.0 * a.vision_lora_r)
    model = model.cuda()
    if a.flex_default:
        model.flex_kernel_options = None
    sources = [Source(a.data / s, None, a.seed) for s in a.sources.split(",")]
    val_sources = []
    for i, s in enumerate(sources):  # first val_q shuffled triples are held out
        val_sources.append(s.subset(slice(0, a.val_q)))
        sources[i] = s.subset(slice(a.val_q, a.val_q + a.max_q if a.max_q else None))
    emit({"event": "data", "world": world(), **{s.name: len(s) for s in sources}})
    if not model.has_trained_late:
        init_late_head(model, sources, tok, a)
        if world() > 1:
            for p in model.late.parameters():
                dist.broadcast(p.data, 0)
        if is0:
            from safetensors.torch import save_file  # the untrained (SVD-init) head = the "base" late head in evals
            save_file({k: v.float().cpu().contiguous() for k, v in model.late.state_dict().items()},
                      a.out / "init_late_head.safetensors")
            (a.out / "init_late_head.json").write_text(json.dumps(model.late.cfg))
    if a.init_only:
        return
    if a.compile:
        for layer in model.backbone.layers:
            layer.compile(dynamic=False)

    mix = {k: float(v) for k, v in (x.split(":") for x in a.mix.split(",") if x)} if a.mix else None
    plan = make_plan(sources, a.batch, a.seed, a.epochs, mix, a.steps * world())
    plan = plan[rank()::world()]
    steps = len(plan) if not a.max_steps else min(a.max_steps, len(plan))
    if world() > 1:  # every rank runs the same number of steps
        t = torch.tensor([steps], device="cuda")
        dist.all_reduce(t, op=dist.ReduceOp.MIN)
        steps = int(t)
    plan = plan[:steps]
    val_batches = []
    if is0:
        val_plan = make_plan(val_sources, a.batch, a.seed + 1)
        val_batches = [Batches(val_sources, val_plan, tok, a)[i] for i in range(len(val_plan))]

    head = list(model.late.parameters())
    body = [p for p in model.backbone.parameters()]
    if a.freeze_backbone:
        for p in body:
            p.requires_grad_(False)
    groups = [{"params": head, "lr": a.lr_head, "weight_decay": 0.0}]
    if a.vision != "off":
        vis = [p for m in (model.vision, model.embed_vision) for p in m.parameters() if p.requires_grad]
        if vis:
            groups.append({"params": vis, "lr": a.lr_vision, "weight_decay": 0.0})
    if not a.freeze_backbone:
        groups.append({"params": body, "lr": a.lr, "weight_decay": a.wd})
    opt = torch.optim.AdamW(groups, betas=(0.9, 0.98), eps=1e-6, fused=True)
    base_lr = [g["lr"] for g in groups]
    anchor = [p.detach().clone() for p in body] if a.l2sp > 0 and not a.freeze_backbone else None
    if anchor is not None and a.init:  # L2-SP pulls to the BASE weights, not to the --init snapshot
        b0 = EG2Dual.load(a.base, base_snapshot=a.base, attn="sdpa")
        anchor = [p.detach().cuda() for p in b0.backbone.parameters()]
        del b0
    ck = a.out / "ckpt"
    start, resumed, best = a.start_step, False, -1.0
    if (ck / "state.pt").exists():
        st = torch.load(ck / "state.pt", map_location="cuda", weights_only=False)
        model.backbone.load_state_dict(st["backbone"])
        model.late.load_state_dict(st["late"])
        if "vision" in st and getattr(model, "vision", None) is not None:
            model.vision.load_state_dict(st["vision"])
            model.embed_vision.load_state_dict(st["embed_vision"])
        opt.load_state_dict(st["opt"])
        start, best, resumed = st["step"], st["best"], True
        del st
        emit({"event": "resume", "step": start, "best": best})
    rewarm = start > 0 and not resumed

    def save_ckpt(done):
        if is0:
            tmp = a.out / "ckpt.tmp"
            if tmp.exists():
                shutil.rmtree(tmp)
            tmp.mkdir()
            st = {"backbone": model.backbone.state_dict(), "late": model.late.state_dict(),
                  "opt": opt.state_dict(), "step": done, "best": best}
            if getattr(model, "vision", None) is not None:  # LoRA A/B live inside the tower's state dict
                st["vision"], st["embed_vision"] = model.vision.state_dict(), model.embed_vision.state_dict()
            torch.save(st, tmp / "state.pt")
            if ck.exists():
                shutil.rmtree(ck)
            tmp.rename(ck)
            emit({"event": "ckpt", "step": done})
        if world() > 1:
            dist.barrier()

    def sched(step):
        if step < a.warmup:
            return (step + 1) / a.warmup
        p = (step - a.warmup) / max(1, steps - a.warmup)
        return 0.1 + 0.9 * 0.5 * (1 + math.cos(math.pi * min(1.0, p)))

    loader = torch.utils.data.DataLoader(Batches(sources, plan[start:], tok, a), batch_size=None, shuffle=False,
                                         num_workers=a.workers, pin_memory=True, persistent_workers=a.workers > 0,
                                         prefetch_factor=4 if a.workers else None)
    emit({"event": "start", "steps": steps, "start": start, "args": {k: str(v) for k, v in vars(a).items()}})
    if is0 and start == 0:
        v0 = validate(model, val_batches, a)
        emit({"event": "val", "step": 0, **v0})
        best = v0["acc_l"] + v0["acc_d"]
    if world() > 1:
        dist.barrier()
    t0 = time.time()
    tw, ntok_acc = time.time(), 0
    model.train()
    params = [p for g in groups for p in g["params"]]
    stop_file = a.out / "STOP"
    step = start - 1
    for step, batch in enumerate(loader, start):
        for g, b in zip(opt.param_groups, base_lr):
            g["lr"] = b * sched(step) * (min(1.0, (step - start + 1) / a.warmup) if rewarm else 1.0)
        out, ntok = train_step(model, batch, a)
        gn = torch.nn.utils.clip_grad_norm_(params, 1.0)
        opt.step()
        opt.zero_grad(set_to_none=True)
        if anchor is not None:
            with torch.no_grad():
                lr_b = opt.param_groups[-1]["lr"]
                for p, p0 in zip(body, anchor):
                    p.add_(p - p0, alpha=-lr_b * a.l2sp)
        ntok_acc += ntok * world()
        if step % a.log_every == 0:
            dt = time.time() - tw
            emit({"event": "train", "step": step, "src": batch["source"], "gnorm": round(float(gn), 3),
                  "lr": opt.param_groups[-1]["lr"], "tok_s": round(ntok_acc / max(dt, 1e-6)),
                  "s_step": round(dt / (a.log_every if step else 1), 2),
                  "mem_gb": round(torch.cuda.max_memory_allocated() / 2**30, 1),
                  **{k: round(v, 4) for k, v in out.items()}})
            tw, ntok_acc = time.time(), 0
        over = bool(a.max_minutes and (time.time() - t0) / 60 > a.max_minutes)
        stop = torch.tensor([float(step == steps - 1 or over), float(is0 and stop_file.exists())], device="cuda")
        if world() > 1:
            dist.all_reduce(stop, op=dist.ReduceOp.MAX)
        last, halt = bool(stop[0].item()), bool(stop[1].item())
        if (step + 1) % a.eval_every == 0 or last:
            if is0:
                model.eval()
                v = validate(model, val_batches, a)
                model.train()
                emit({"event": "val", "step": step + 1, **v})
                score = v["acc_l"] + v["acc_d"]
                meta = {"step": step + 1, "val": v, "args": {k: str(x) for k, x in vars(a).items()}}
                if score > best:
                    best = score
                    model.export_snapshot(a.out / "best", meta)
                    emit({"event": "export", "which": "best", "step": step + 1})
            if world() > 1:
                dist.barrier()
        if halt or (not last and (step + 1) % a.ckpt_every == 0) or (last and step + 1 < steps):  # time cap: resumable
            save_ckpt(step + 1)
        if halt:
            emit({"event": "halted", "step": step + 1})
            if world() > 1:
                dist.destroy_process_group()
            return
        if last:
            break
    if is0:
        model.export_snapshot(a.out / "last", {"step": step + 1, "args": {k: str(x) for k, x in vars(a).items()}})
        emit({"event": "done", "steps": step + 1, "minutes": round((time.time() - t0) / 60, 1)})
        if ck.exists() and step + 1 >= steps:  # finished: the resumable state is no longer needed
            shutil.rmtree(ck)
    if world() > 1:
        dist.barrier()
        dist.destroy_process_group()


if __name__ == "__main__":
    main()
