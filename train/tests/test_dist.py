"""Grouped GradCache across processes (CPU, gloo): 4 ranks in gather groups of 2. The all-reduced gradient of
ctx_train_step must equal the MEAN over groups of a direct (single-process) backward on each group's union batch;
ranks hold different numbers of chunks, so the padded uneven gather is covered too.
Run from train/: python -m pytest -q tests/test_dist.py"""
import os
import socket
import tempfile
from pathlib import Path

import pytest
import torch
import torch.distributed as dist
import torch.multiprocessing as mp
import torch.nn.functional as F

from tests.tiny import make_tiny, real_snapshot

pytestmark = pytest.mark.skipif(real_snapshot() is None, reason="no EG2 snapshot for tokenizer files")
W, G = 4, 2


def n_art(r):
    return 2 if r % 2 == 0 else 3


def start(r):
    return 10 * r


def qj(r):  # equal query counts per rank (6), different chunk counts (8 vs 12)
    return (0, 1, 2) if r % 2 == 0 else (0, 2)


def _model(tiny):
    from eg2late.model import EG2Dual
    torch.manual_seed(0)
    m = EG2Dual.load(tiny)
    m.late = type(m.late)(d_in=m.backbone.config.embedding_dim)
    torch.nn.init.normal_(m.late.down.weight, std=0.1)
    return m.train()


def _worker(r, tiny, port, out):
    os.environ.update(MASTER_ADDR="127.0.0.1", MASTER_PORT=str(port))
    dist.init_process_group("gloo", rank=r, world_size=W)
    from eg2late.contextual import ctx_train_step
    from eg2late.dist_utils import gworld, grank, init_gather_groups
    from tests.test_contextual import _args, _batch
    assert init_gather_groups(G) == G and gworld() == G and grank() == r % G
    torch.set_num_threads(1)
    m = _model(tiny)
    b = _batch(Path(tiny), n_art(r), start(r), qj(r))
    m.zero_grad()
    res = ctx_train_step(m, b, _args())
    if r == 0:
        torch.save({"grads": {n: p.grad for n, p in m.named_parameters() if p.grad is not None}, "loss": res["total"]}, out)
    dist.barrier()
    dist.destroy_process_group()


def _union(bs):
    from eg2late.train_s1 import slice_batch  # noqa: F401
    L = max(b["q"]["input_ids"].shape[1] for b in bs)
    pad = lambda x, v=0: F.pad(x, (0, L - x.shape[1]), value=v)  # noqa: E731
    q = {k: torch.cat([pad(b["q"][k]) for b in bs]) for k in ("input_ids", "attention_mask", "late_mask")}
    wins, place, lab = [], [], []
    for b in bs:
        lab += [x + len(place) for x in b["lab"].tolist()]
        place += [(w + len(wins), lo, hi) for w, lo, hi in b["place"]]
        wins += b["wins"]
    cat = lambda k: torch.cat([b[k] for b in bs])  # noqa: E731
    return {"q": q, "wins": wins, "place": place, "lab": torch.tensor(lab), "chunk_doc": cat("chunk_doc"),
            "chunk_hash": cat("chunk_hash"), "q_doc": cat("q_doc"), "gold": cat("gold"), "tq": cat("tq"), "tc": cat("tc")}


def test_grouped_gradcache_matches_mean_of_group_losses(tmp_path):
    from eg2late.contextual import ctx_loss, ctx_targets, encode_chunks
    from eg2late.train_s1 import encode
    from tests.test_contextual import _args, _batch
    tiny = make_tiny(tmp_path / "tiny", vision=False)
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    out = tmp_path / "rank0.pt"
    mp.spawn(_worker, args=(str(tiny), port, str(out)), nprocs=W, join=True)
    got = torch.load(out)
    m = _model(tiny)
    params = dict(m.named_parameters())
    ref = {n: torch.zeros_like(p) for n, p in params.items()}
    a = _args()
    losses = []
    for g in range(W // G):
        u = _union([_batch(tiny, n_art(r), start(r), qj(r)) for r in range(g * G, (g + 1) * G)])
        m.zero_grad()
        qd, ql = encode(m, u["q"], 64, grad=True, dev="cpu")
        cd, cl, cm = encode_chunks(m, u["wins"], u["place"], a.chunk_len, grad=True)
        forbid = ctx_targets(u["lab"], u["gold"], u["chunk_hash"], torch.ones(len(cd), dtype=torch.bool))
        o = ctx_loss(qd, ql.float(), u["q"]["late_mask"], cd, cl, cm, u["lab"], u["chunk_doc"], u["q_doc"],
                     forbid, a, u["tq"] @ u["tc"].T)
        o["total"].backward()
        losses.append(float(o["total"]))
        for n, p in params.items():
            if p.grad is not None:
                ref[n] += p.grad / (W // G)
    num = sum(float((got["grads"][n] - ref[n]).norm() ** 2) for n in got["grads"])
    den = sum(float(ref[n].norm() ** 2) for n in got["grads"])
    assert abs(got["loss"] - losses[0]) < 1e-4 * abs(losses[0]), (got["loss"], losses)
    per = {n: float((got["grads"][n] - ref[n]).norm() / (ref[n].norm() + 1e-12)) for n in got["grads"]}
    worst = sorted(per, key=per.get)[-3:]
    print("rel", (num / den) ** 0.5, {n: per[n] for n in worst})
    # ~2e-4 overall, ~1e-3 on the late head: the bf16 cast of query late reps in train_s1.encode (2e-6 without it)
    assert den > 0 and (num / den) ** 0.5 < 1e-3, (num / den) ** 0.5
