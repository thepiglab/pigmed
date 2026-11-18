"""GradCache vs direct gradient diagnostic (GPU, a few minutes): `python -m tests.grad_diag --base SNAP` from train/.

check_wrapper's GradCache check showed a large GradCache-vs-direct gap on RTX PRO 6000 and a NaN on B200. This
isolates the cause: every path (direct padded, GradCache padded, GradCache packed, direct packed) under each SDPA
backend (math, efficient) and with autocast on (bf16) or off (fp32), all compared with an fp32 + math direct reference.
"""
import argparse
import contextlib
import json
from types import SimpleNamespace

import torch
from torch.nn.attention import SDPBackend, sdpa_kernel

from eg2late.model import DOC_PROMPT, QUERY_PROMPT, EG2Dual, Tok, pack
from eg2late import train_s1

_AUTOCAST = torch.autocast


def rel(g1, g2):
    nonfin = sorted({n for g in (g1, g2) for n in g if not torch.isfinite(g[n].float()).all()})
    ok = [n for n in g2 if n in g1 and n not in nonfin]
    num = sum(float((g1[n].float() - g2[n].float()).norm() ** 2) for n in ok)
    den = sum(float(g2[n].float().norm() ** 2) for n in ok)
    per = {n: float((g1[n].float() - g2[n].float()).norm() / (g2[n].float().norm() + 1e-12))
           for n in ok if g2[n].float().norm() > 1e-8 * den ** 0.5}
    top = sorted(per, key=per.get, reverse=True)[:3]
    return {"global_rel": round((num / den) ** 0.5, 5), "top": {n: round(per[n], 4) for n in top},
            "n_nonfinite": len(nonfin), "nonfinite": nonfin[:5]}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", required=True)
    ap.add_argument("--n-docs", type=int, default=6)
    a = ap.parse_args()
    torch.backends.cuda.matmul.allow_tf32 = False
    torch.backends.cudnn.allow_tf32 = False
    tok = Tok(a.base)
    m = EG2Dual.load(a.base, attn="sdpa").cuda().train()
    texts = ["The mitochondrion is the powerhouse of the cell.",
             "Aspirin irreversibly inhibits cyclooxygenase-1 in platelets, which blocks thromboxane A2 synthesis "
             "for the life of the platelet; this is why low-dose aspirin is used for secondary prevention.",
             "Paris is the capital of France and its largest city.",
             "Metformin lowers hepatic glucose production and is first-line therapy for type 2 diabetes.",
             "The Treaty of Westphalia ended the Thirty Years' War in 1648.",
             "Beta blockers reduce heart rate and myocardial oxygen demand in stable angina."][: a.n_docs]
    docs = [DOC_PROMPT + t for t in texts]
    qs = [QUERY_PROMPT + t for t in ["what inhibits platelet COX-1", "capital of France", "first-line drug for T2D"]]
    y = tok(qs, len(QUERY_PROMPT), 64)
    d = tok(docs, len(DOC_PROMPT), 512)
    y.pop("offsets", None)
    d.pop("offsets", None)
    nq, nd = len(qs), len(docs)
    gold_doc = [1, 2, 3]
    hq = torch.tensor([[train_s1.sid(f"t:{gold_doc[i]}")] + [-1] * 7 for i in range(nq)])
    hd = torch.tensor([train_s1.sid(f"t:{i}") for i in range(nd)])
    g = torch.Generator().manual_seed(0)
    tq, td = torch.randn(nq, 768, generator=g) * 0.1, torch.randn(nd, 768, generator=g) * 0.1
    args = SimpleNamespace(mb_q=1, mb_d=2, tau_d=0.05, tau_l=0.02, tau_t=0.05, w_dense=1.0, w_late=1.0,
                           w_align=0.1, w_kd_dense=0.5, w_kd_late=0.5)

    def grads():
        return {n: p.grad.detach().clone() for n, p in m.named_parameters() if p.grad is not None}

    def direct():
        m.zero_grad()
        qd, ql = train_s1.encode(m, y, 64, grad=True)
        dd, dl = train_s1.encode(m, d, 64, grad=True)
        forbid, lab, teacher = train_s1.targets(hq.cuda(), hd.cuda(), nq, nd, tq.cuda(), td.cuda(), 1)
        out = train_s1.loss_fn(qd, ql, y["late_mask"].cuda(), dd, dl, d["late_mask"].cuda(), forbid, teacher, lab, args)
        out["total"].backward()
        return grads(), float(out["total"])

    def direct_packed():
        m.zero_grad()
        qp = [train_s1.to_dev(p, "cuda") for p in pack(y, 256)]
        dp = [train_s1.to_dev(p, "cuda") for p in pack(d, 1024)]
        qd, ql = train_s1.encode_packed(m, y, qp, grad=True)
        dd, dl = train_s1.encode_packed(m, d, dp, grad=True)
        forbid, lab, teacher = train_s1.targets(hq.cuda(), hd.cuda(), nq, nd, tq.cuda(), td.cuda(), 1)
        out = train_s1.loss_fn(qd, ql, y["late_mask"].cuda(), dd, dl, d["late_mask"].cuda(), forbid, teacher, lab, args)
        out["total"].backward()
        return grads(), float(out["total"])

    def gc(packed):
        m.zero_grad()
        batch = {"q": y, "d": d, "gold": hq, "doc_hash": hd, "tq": tq, "td": td,
                 "qgroup": torch.full((nq,), -1), "dgroup": torch.full((nd,), -1)}
        if packed:
            batch["qp"], batch["dp"] = pack(y, 256), pack(d, 1024)
        out, _ = train_s1.train_step(m, batch, args)
        return grads(), out["total"]

    def run(fn, precision, backend, attn):
        m.backbone.config._attn_implementation = attn
        ac = (lambda *x, **k: contextlib.nullcontext()) if precision == "fp32" else _AUTOCAST
        torch.autocast = ac
        try:
            with sdpa_kernel([backend]):
                return fn()
        finally:
            torch.autocast = _AUTOCAST
            m.backbone.config._attn_implementation = "sdpa"

    M, E = SDPBackend.MATH, SDPBackend.EFFICIENT_ATTENTION
    rep = {"gpu": torch.cuda.get_device_name()}
    ref, ref_loss = run(direct, "fp32", M, "sdpa")
    rep["ref_loss_fp32_math"] = ref_loss
    ref2, _ = run(direct, "fp32", M, "sdpa")
    rep["noise_ref_vs_ref"] = rel(ref2, ref)["global_rel"]
    cases = {
        "direct_pad_fp32_eff": (direct, "fp32", E, "sdpa"),
        "direct_pad_bf16_math": (direct, "bf16", M, "sdpa"),
        "direct_pad_bf16_eff": (direct, "bf16", E, "sdpa"),
        "gc_pad_fp32_math": (lambda: gc(False), "fp32", M, "sdpa"),
        "gc_pad_fp32_eff": (lambda: gc(False), "fp32", E, "sdpa"),
        "gc_pad_bf16_math": (lambda: gc(False), "bf16", M, "sdpa"),
        "gc_pad_bf16_eff": (lambda: gc(False), "bf16", E, "sdpa"),
        "direct_packed_fp32": (direct_packed, "fp32", M, "flex_attention"),
        "direct_packed_bf16": (direct_packed, "bf16", M, "flex_attention"),
        "gc_packed_fp32": (lambda: gc(True), "fp32", M, "flex_attention"),
        "gc_packed_bf16": (lambda: gc(True), "bf16", M, "flex_attention"),
    }
    for k, c in cases.items():
        try:
            gk, lk = run(*c)
            rep[k] = {"loss": round(lk, 5), **rel(gk, ref)}
        except Exception as e:  # noqa: BLE001
            rep[k] = f"{type(e).__name__}: {str(e)[:300]}"
        print(json.dumps({k: rep[k]}), flush=True)
    print(json.dumps(rep, indent=1))


if __name__ == "__main__":
    main()
