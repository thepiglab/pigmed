"""S2 contextual trainer: late chunking over whole PMC articles, dense + late heads, InSeNT/pplx losses
(contextual.py), GradCache over article windows, negatives shared within the gather group, optional interleaved S1
pair steps (protects NanoBEIR, which fell 3-8 points under InSeNT-only training in ConTEB).

Data (--ctx, written by the data side; see contextual.py and docs/RETRIEVER_TRAINING_PLAN.md):
    articles.jsonl.gz   {article_id, group, title, abstract, chunks: [{id, heading, text}]}   (reading order)
    ctx_queries.jsonl   {id, text (raw), article_id, pos: [chunk ids], kind}
    qvec/, cvec/        optional teacher vectors (flashembed layout): base EG2 queries / plain chunks ("title: none |
                        text: heading\\nchunk" = pigmed-pmc-eg2 v2), keyed by query id / chunk id
Each step every rank takes articles (shuffled) until it holds exactly --q-per-rank queries (<= --q-per-article each).

    torchrun --nproc_per_node 8 -m eg2late.train_s2 --base SNAP --init S1_SNAP --ctx DIR --out RUN --steps 2000 \\
        [--s1-data DIR --s1-mix-config cfg.json --s1-every 2]
"""
from __future__ import annotations

import argparse
import gzip
import json
import math
import os
import random
import shutil
import time
from pathlib import Path

import numpy as np
import torch
import torch.distributed as dist

from .contextual import article_input, build_article, ctx_train_step
from .dist_utils import init_gather_groups, rank, world
from .model import QUERY_PROMPT, EG2Dual, Tok
from .train_s1 import MAX_GOLD, load_vecs, sid


def join_chunk_text(arts: dict, root: Path | None) -> int:
    """Fill chunk["text"] for ids-only articles from the published chunk parquet (pigmed-pmc-eg2 chunks/*.parquet:
    unit_id, text) under `root`. Returns how many chunks were filled; raises if any stay missing."""
    need = {c["id"] for r in arts.values() for c in r["chunks"] if c.get("text") is None}
    if not need:
        return 0
    if root is None:
        raise ValueError(f"{len(need)} chunks have no text: pass --chunk-text <chunks parquet dir>")
    import pyarrow.compute as pc
    import pyarrow.dataset as ds
    tab = ds.dataset(str(root), format="parquet").to_table(columns=["unit_id", "text"],
                                                           filter=pc.field("unit_id").isin(list(need)))
    text = dict(zip(tab.column("unit_id").to_pylist(), tab.column("text").to_pylist()))
    miss = need - text.keys()
    if miss:
        raise ValueError(f"{len(miss)} chunk ids not found under {root}, e.g. {sorted(miss)[:3]}")
    for r in arts.values():
        for c in r["chunks"]:
            if c.get("text") is None:
                c["text"] = text[c["id"]]
    return len(need)


def _has(d: Path, name: str) -> bool:
    return (d / name).exists() or (d / (name + ".gz")).exists()


def _open(d: Path, name: str):
    return gzip.open(d / (name + ".gz"), "rt") if (d / (name + ".gz")).exists() else open(d / name)


class CtxData(torch.utils.data.Dataset):
    """Item i = one rank-step batch (articles chosen by the plan), tokenised on the DataLoader workers."""

    def __init__(self, root: Path, tok: Tok, a, plan):
        """root: one shard dir (articles.jsonl[.gz], ctx_queries.jsonl[.gz], optional chunk_text.parquet), a dir of
        such shard dirs, or the data side's ctx/ (root ctx_queries + shards/<published shard name>/). --ctx-shards limits which shards
        load (comma list or a count). Queries may sit per shard or at the root. Ids-only chunks are filled from the
        shard's own chunk_text.parquet, else from --chunk-text."""
        self.a, self.tok, self.plan = a, tok, plan
        self.arts = {}
        self.qs: dict[str, list[dict]] = {}
        base = root / "shards" if (root / "shards").is_dir() else root  # ctx/ (root queries) or ctx/shards/
        shards = [root] if _has(root, "articles.jsonl") else sorted(p for p in base.iterdir() if _has(p, "articles.jsonl"))
        sel = getattr(a, "ctx_shards", None)
        if sel:
            shards = shards[: int(sel)] if str(sel).isdigit() else [p for p in shards if p.name in set(sel.split(","))]
        if not shards:
            raise FileNotFoundError(f"no articles.jsonl(.gz) under {root}")
        qfiles = [p for p in [root] + shards if _has(p, "ctx_queries.jsonl")]
        for sh in shards:
            arts = {}
            with _open(sh, "articles.jsonl") as f:
                for line in f:
                    r = json.loads(line)
                    arts[r["article_id"]] = r
            own = sh / "chunk_text.parquet"
            join_chunk_text(arts, own if own.exists() else getattr(a, "chunk_text", None))
            self.arts.update(arts)
        for qf in dict.fromkeys(qfiles):
            with _open(qf, "ctx_queries.jsonl") as f:
                for line in f:
                    q = json.loads(line)
                    r = self.arts.get(q["article_id"])
                    if r is None or not q.get("pos"):
                        continue
                    ids = {c["id"] for c in r["chunks"]}
                    q["pos"] = [p for p in q["pos"] if p in ids]
                    if q["pos"]:
                        self.qs.setdefault(q["article_id"], []).append(q)
        self.order = sorted(self.qs)
        self.qv = load_vecs(root / "qvec") if (root / "qvec").exists() else None
        self.cv = load_vecs(root / "cvec") if (root / "cvec").exists() else None

    def __len__(self):
        return len(self.plan)

    def __getitem__(self, i):
        a = self.a
        arts, seed = self.plan[i]
        rng = random.Random(seed)
        wins, place, cdoc, chash, qtext, qids, lab, qdoc, gold, cids = [], [], [], [], [], [], [], [], [], []
        need = a.q_per_rank
        for aid in arts:
            if need <= 0:
                break
            r = self.arts[aid]
            row = article_input(r.get("title"), r.get("abstract"), r["chunks"])
            art = build_article(self.tok.tk, row, a.wmax, a.overlap)
            base = len(wins)
            wins += [torch.tensor(w) for w in art["wins"]]
            local = {}
            for (w, lo, hi), cid in zip(art["place"], art["ids"]):
                local[cid] = len(place)
                place.append((base + w, lo, hi))
                cdoc.append(sid(f"art:{aid}"))
                chash.append(sid(f"chunk:{cid}"))
                cids.append(cid)
            qs = [q for q in self.qs[aid] if any(p in local for p in q["pos"])]
            rng.shuffle(qs)
            for q in qs[: min(a.q_per_article, need)]:
                pos = [p for p in q["pos"] if p in local]
                lab.append(local[rng.choice(pos)])
                g = [sid(f"chunk:{p}") for p in pos][:MAX_GOLD]
                gold.append(g + [-1] * (MAX_GOLD - len(g)))
                qtext.append(QUERY_PROMPT + q["text"])
                qids.append(q["id"])
                qdoc.append(sid(f"art:{aid}"))
                need -= 1
        if need > 0:
            raise RuntimeError(f"plan item {i}: articles ran out {need} queries short")
        q = self.tok(qtext, len(QUERY_PROMPT), a.q_len)
        q.pop("offsets", None)
        out = {"q": q, "wins": wins, "place": place, "chunk_doc": torch.tensor(cdoc), "chunk_hash": torch.tensor(chash),
               "q_doc": torch.tensor(qdoc), "lab": torch.tensor(lab), "gold": torch.tensor(gold)}
        if self.qv is not None and self.cv is not None:
            (qi, qv), (ci, cv) = self.qv, self.cv
            if all(x in qi for x in qids) and all(x in ci for x in cids):
                out["tq"] = torch.from_numpy(np.stack([qv[qi[x]] for x in qids]).astype(np.float32))
                out["tc"] = torch.from_numpy(np.stack([cv[ci[x]] for x in cids]).astype(np.float32))
        return out


def make_ctx_plan(order: list[str], qcount: dict[str, int], q_per_rank: int, q_per_article: int, steps: int,
                  seed: int) -> list[tuple[list[str], int]]:
    """Per step: a list of article ids holding >= q_per_rank usable queries (cycling, reshuffled per pass)."""
    rng = random.Random(seed)
    pool: list[str] = []
    plan = []
    for _ in range(steps):
        arts, n = [], 0
        while n < q_per_rank:
            if not pool:
                pool = list(order)
                rng.shuffle(pool)
            aid = pool.pop()
            arts.append(aid)
            n += min(q_per_article, qcount[aid])
        plan.append((arts, rng.randrange(1 << 30)))
    return plan


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--base", required=True)
    ap.add_argument("--init", default=None, help="start from this exported snapshot (e.g. the S1.5 best)")
    ap.add_argument("--ctx", type=Path, required=True)
    ap.add_argument("--ctx-shards", default=None, help="subset of shard dirs under --ctx: comma list or a count")
    ap.add_argument("--chunk-text", type=Path, default=None,
                    help="published chunk parquet dir, for ids-only articles (chunks without text)")
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--steps", type=int, required=True, help="optimiser steps (per rank)")
    ap.add_argument("--q-per-rank", type=int, default=64)
    ap.add_argument("--q-per-article", type=int, default=8)
    ap.add_argument("--wmax", type=int, default=8192)
    ap.add_argument("--overlap", type=int, default=1024)
    ap.add_argument("--chunk-len", type=int, default=256, help="late tokens kept per chunk (spans are pooled fully)")
    ap.add_argument("--q-len", type=int, default=96)
    ap.add_argument("--mb-q", type=int, default=64)
    ap.add_argument("--lam-seq", type=float, default=0.15)
    ap.add_argument("--seq-in-batch", default="exclude", choices=["exclude", "include"])
    ap.add_argument("--beta-start", type=float, default=0.2)
    ap.add_argument("--beta-end", type=float, default=0.5)
    ap.add_argument("--tau-d", type=float, default=0.05)
    ap.add_argument("--tau-l", type=float, default=0.02)
    ap.add_argument("--tau-t", type=float, default=0.05)
    ap.add_argument("--w-dense", type=float, default=1.0)
    ap.add_argument("--w-late", type=float, default=1.0)
    ap.add_argument("--w-align", type=float, default=0.1)
    ap.add_argument("--w-kd", type=float, default=0.5)
    ap.add_argument("--lr", type=float, default=1e-5)
    ap.add_argument("--lr-head", type=float, default=2e-4)
    ap.add_argument("--wd", type=float, default=0.01)
    ap.add_argument("--warmup", type=int, default=100)
    ap.add_argument("--l2sp", type=float, default=0.01)
    ap.add_argument("--gather-group", type=int, default=0)
    ap.add_argument("--s1-data", type=Path, default=None, help="interleave S1 pair steps from these sources")
    ap.add_argument("--s1-mix-config", type=Path, default=None)
    ap.add_argument("--s1-every", type=int, default=0, help="every k-th step is an S1 pair step (0 = none)")
    ap.add_argument("--ckpt-every", type=int, default=200)
    ap.add_argument("--export-every", type=int, default=500, help="rolling export to <out>/latest (one kept)")
    ap.add_argument("--log-every", type=int, default=10)
    ap.add_argument("--workers", type=int, default=3)
    ap.add_argument("--seed", type=int, default=0)
    ap.add_argument("--dry-run", action="store_true")
    a = ap.parse_args()
    if a.dry_run:
        print(json.dumps({k: str(v) for k, v in vars(a).items()}, indent=1))
        return
    if "LOCAL_RANK" in os.environ:
        dist.init_process_group("nccl")
        torch.cuda.set_device(int(os.environ["LOCAL_RANK"]))
        init_gather_groups(a.gather_group)
    is0 = rank() == 0
    torch.manual_seed(a.seed)
    a.out.mkdir(parents=True, exist_ok=True)
    log = open(a.out / "train_log.jsonl", "a") if is0 else None

    def emit(r):
        if is0:
            r["t"] = round(time.time(), 1)
            log.write(json.dumps(r) + "\n")
            log.flush()
            print(json.dumps(r), flush=True)

    tok = Tok(a.base)
    model = EG2Dual.load(a.init or a.base, base_snapshot=a.base).cuda()
    probe = CtxData(a.ctx, tok, a, [])
    qcount = {k: len(v) for k, v in probe.qs.items()}
    plan = make_ctx_plan(probe.order, qcount, a.q_per_rank, a.q_per_article, a.steps * world(), a.seed)
    plan = plan[rank()::world()][: a.steps]
    emit({"event": "data", "articles": len(probe.order), "queries": sum(qcount.values()), "world": world()})
    s1 = None
    if a.s1_every:
        from types import SimpleNamespace
        from .mixcfg import apply_mix_config
        from .train_s1 import Batches, Source, make_plan
        sa = SimpleNamespace(sources="", mix="", steps=a.steps // a.s1_every + 1, base=a.base, n_neg=7, neg_pool=15,
                             q_len=96, d_len=512, mb_q=128, mb_d=128, pack_tokens=0, tau_d=a.tau_d, tau_l=a.tau_l,
                             tau_t=a.tau_t, w_dense=1.0, w_late=1.0, w_align=a.w_align, w_kd_dense=1.0,
                             w_kd_late=0.5, w_rev=1.0, qq_neg=0.0, qq_margin=0.1, batch=a.q_per_rank,
                             image_doc_text=None)
        apply_mix_config(sa, a.s1_mix_config, argv=[])
        srcs = [Source(a.s1_data / s, None, a.seed) for s in sa.sources.split(",")]
        mix = {k: float(v) for k, v in (x.split(":") for x in sa.mix.split(","))}
        s1_plan = make_plan(srcs, sa.batch, a.seed, 1, mix, sa.steps * world())[rank()::world()]
        s1 = (iter(torch.utils.data.DataLoader(Batches(srcs, s1_plan, tok, sa), batch_size=None, num_workers=1)), sa)
    head = list(model.late.parameters())
    body = list(model.backbone.parameters())
    groups = [{"params": head, "lr": a.lr_head, "weight_decay": 0.0}, {"params": body, "lr": a.lr, "weight_decay": a.wd}]
    opt = torch.optim.AdamW(groups, betas=(0.9, 0.98), eps=1e-6, fused=True)
    base_lr = [g["lr"] for g in groups]
    anchor = None
    if a.l2sp > 0:
        b0 = EG2Dual.load(a.base, base_snapshot=a.base)
        anchor = [p.detach().cuda() for p in b0.backbone.parameters()]
        del b0
    ck, start = a.out / "ckpt", 0
    if (ck / "state.pt").exists():
        st = torch.load(ck / "state.pt", map_location="cuda", weights_only=False)
        model.backbone.load_state_dict(st["backbone"])
        model.late.load_state_dict(st["late"])
        opt.load_state_dict(st["opt"])
        start = st["step"]
        emit({"event": "resume", "step": start})

    def sched(step):
        if step < a.warmup:
            return (step + 1) / a.warmup
        p = (step - a.warmup) / max(1, a.steps - a.warmup)
        return 0.1 + 0.9 * 0.5 * (1 + math.cos(math.pi * min(1.0, p)))

    def save_ckpt(done):
        if is0:
            tmp = a.out / "ckpt.tmp"
            shutil.rmtree(tmp, ignore_errors=True)
            tmp.mkdir()
            torch.save({"backbone": model.backbone.state_dict(), "late": model.late.state_dict(),
                        "opt": opt.state_dict(), "step": done}, tmp / "state.pt")
            shutil.rmtree(ck, ignore_errors=True)
            tmp.rename(ck)
            emit({"event": "ckpt", "step": done})
        if world() > 1:
            dist.barrier()

    loader = torch.utils.data.DataLoader(CtxData(a.ctx, tok, a, plan[start:]), batch_size=None, shuffle=False,
                                         num_workers=a.workers, persistent_workers=a.workers > 0,
                                         prefetch_factor=2 if a.workers else None)
    params = [p for g in groups for p in g["params"]]
    model.train()
    from .train_s1 import train_step
    tw = time.time()
    step = start - 1
    for step, batch in enumerate(loader, start):
        for g, b in zip(opt.param_groups, base_lr):
            g["lr"] = b * sched(step)
        prog = step / max(1, a.steps - 1)  # beta: cosine from beta_start to beta_end (pplx v1)
        a.beta = a.beta_end + (a.beta_start - a.beta_end) * 0.5 * (1 + math.cos(math.pi * prog))
        if s1 is not None and a.s1_every and step % a.s1_every == a.s1_every - 1:
            out, _ = train_step(model, next(s1[0]), s1[1])
            out["kind"] = "s1"
        else:
            out = ctx_train_step(model, batch, a)
            out["kind"] = "ctx"
        gn = torch.nn.utils.clip_grad_norm_(params, 1.0)
        opt.step()
        opt.zero_grad(set_to_none=True)
        if anchor is not None:
            with torch.no_grad():
                lr_b = opt.param_groups[-1]["lr"]
                for p, p0 in zip(body, anchor):
                    p.add_(p - p0, alpha=-lr_b * a.l2sp)
        if step % a.log_every == 0:
            emit({"event": "train", "step": step, "beta": round(a.beta, 3), "gnorm": round(float(gn), 3),
                  "s_step": round((time.time() - tw) / (a.log_every if step > start else 1), 2),
                  "mem_gb": round(torch.cuda.max_memory_allocated() / 2**30, 1),
                  **{k: (round(v, 4) if isinstance(v, float) else v) for k, v in out.items()}})
            tw = time.time()
        halt = torch.tensor([float(is0 and (a.out / "STOP").exists())], device="cuda")
        if world() > 1:
            dist.all_reduce(halt, op=dist.ReduceOp.MAX)
        if bool(halt.item()) or (step + 1) % a.ckpt_every == 0:
            save_ckpt(step + 1)
        if bool(halt.item()):
            emit({"event": "halted", "step": step + 1})
            break
        if is0 and (step + 1) % a.export_every == 0:
            model.export_snapshot(a.out / "latest", {"step": step + 1, "args": {k: str(v) for k, v in vars(a).items()}})
    else:
        if is0:
            model.export_snapshot(a.out / "last", {"step": step + 1, "args": {k: str(v) for k, v in vars(a).items()}})
            shutil.rmtree(ck, ignore_errors=True)
            emit({"event": "done", "steps": step + 1})
    if world() > 1:
        dist.barrier()
        dist.destroy_process_group()


if __name__ == "__main__":
    main()
