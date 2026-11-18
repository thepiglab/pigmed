"""S1.5 loss options and the mix config (CPU, no model): reverse loss on symmetric rows, q-q negatives with
false-negative masking, mix-config precedence. Run from train/: python -m pytest -q tests/test_s15.py"""
import json
from types import SimpleNamespace

import pytest
import torch
import torch.nn.functional as F

from eg2late.train_s1 import loss_fn


def _a(**kw):
    a = dict(tau_d=0.05, tau_l=0.05, tau_t=0.05, w_dense=1.0, w_late=0.0, w_align=0.0, w_kd_dense=0.0,
             w_kd_late=0.0, w_rev=0.0, qq_neg=0.0, qq_margin=0.1)
    a.update(kw)
    return SimpleNamespace(**a)


def _reps(B=4, N=8, D=16, L=3, seed=0):
    g = torch.Generator().manual_seed(seed)
    n = lambda *s: F.normalize(torch.randn(*s, generator=g), dim=-1)  # noqa: E731
    qd, dd = n(B, D), n(N, D)
    ql, dl = n(B, L, D), n(N, L, D)
    qm, dm = torch.ones(B, L, dtype=torch.bool), torch.ones(N, L, dtype=torch.bool)
    lab = torch.arange(B)
    forbid = torch.zeros(B, N, dtype=torch.bool)
    return qd, ql, qm, dd, dl, dm, forbid, lab


def test_plain_dense_ce():
    qd, ql, qm, dd, dl, dm, forbid, lab = _reps()
    out = loss_fn(qd, ql, qm, dd, dl, dm, forbid, None, lab, _a())
    ref = F.cross_entropy(qd @ dd.T / 0.05, lab)
    for k in (512, 256, 128):  # MRL terms (no-ops at D=16: slicing beyond D keeps all dims)
        ref = ref + 0.25 * F.cross_entropy(F.normalize(qd[:, :k], dim=-1) @ F.normalize(dd[:, :k], dim=-1).T / 0.05, lab)
    assert torch.allclose(out["dense"], ref, atol=1e-5)


def test_reverse_loss_only_on_symmetric_rows():
    qd, ql, qm, dd, dl, dm, forbid, lab = _reps()
    sym = torch.tensor([True, False, True, False])
    out = loss_fn(qd, ql, qm, dd, dl, dm, forbid, None, lab, _a(w_rev=1.0), sym)
    sd, sl = qd @ dd.T / 0.05, None
    from eg2late.train_s1 import maxsim_train
    sl = maxsim_train(ql, qm, dl, dm) / 0.05
    tgt = torch.tensor([0, 2])
    ref = F.cross_entropy(sd[:, lab].T[sym], tgt) + F.cross_entropy(sl[:, lab].T[sym], tgt)
    assert torch.allclose(out["rev"], ref, atol=1e-5)
    none = loss_fn(qd, ql, qm, dd, dl, dm, forbid, None, lab, _a(w_rev=1.0), torch.zeros(4, dtype=torch.bool))
    assert "rev" not in none


def test_qq_negatives_mask_false_negatives():
    qd, ql, qm, dd, dl, dm, forbid, lab = _reps()
    qd2 = qd.clone()
    qd2[1] = qd2[0]  # query 1 duplicates query 0: must be masked as a false negative for both
    base = loss_fn(qd2, ql, qm, dd, dl, dm, forbid, None, lab, _a())["dense"]
    with_qq = loss_fn(qd2, ql, qm, dd, dl, dm, forbid, None, lab, _a(qq_neg=1.0))["dense"]
    assert with_qq > base  # extra negatives can only raise the CE
    # with an impossible margin every q-q column is masked -> identical to no q-q
    same = loss_fn(qd2, ql, qm, dd, dl, dm, forbid, None, lab, _a(qq_neg=1.0, qq_margin=-100.0))["dense"]
    assert torch.allclose(same, base, atol=1e-5)


def test_mix_config_precedence(tmp_path):
    from eg2late.mixcfg import apply_mix_config
    p = tmp_path / "m.json"
    p.write_text(json.dumps({"steps": 100, "sources": {"a": {"weight": 0.7}, "b": {"weight": 0.3}, "c": 0},
                             "args": {"lr": 1e-5, "l2sp": 0.02}}))
    a = SimpleNamespace(sources="nq", mix="", steps=0, lr=2e-5, l2sp=0.0)
    apply_mix_config(a, p, argv=["--l2sp", "0.05"])
    assert a.sources == "a,b" and a.mix == "a:0.7,b:0.3" and a.steps == 100 and a.lr == 1e-5 and a.l2sp == 0.0
    a = SimpleNamespace(sources="nq", mix="", steps=7, lr=2e-5, l2sp=0.0)
    apply_mix_config(a, p, argv=["--steps", "7"])
    assert a.steps == 7
    p.write_text(json.dumps({"sources": {"a": 1}, "args": {"nope": 1}, "steps": 1}))
    with pytest.raises(ValueError):
        apply_mix_config(SimpleNamespace(sources="", mix="", steps=0), p, argv=[])
