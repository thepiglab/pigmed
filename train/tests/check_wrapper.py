"""Wrapper checks (CPU or GPU, a minute): run `python -m tests.check_wrapper --base SNAP --tmp DIR` from train/.

1. Dense parity: EG2Dual dense == sentence-transformers' EG2 encode (prompts included) to cosine >= 0.9999.
2. late_mask: covers exactly the non-prompt text tokens.
3. MaxSim: ours == a naive loop, and == PyLate's colbert_scores when PyLate imports (it pins older ST/transformers,
   so it is installed --no-deps and may not import; reported either way).
4. GradCache: one train_step's gradients == a direct (non-cached) backward of the same loss.
5. Export round trip: export_snapshot -> EG2Dual.load gives identical dense and late outputs; base tensors untouched.
"""
import argparse
import json
import tempfile
from pathlib import Path
from types import SimpleNamespace

import torch
import torch.nn.functional as F

from eg2late.model import DOC_PROMPT, QUERY_PROMPT, EG2Dual, Tok, maxsim, pack
from eg2late import train_s1


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", required=True)
    ap.add_argument("--tmp", type=Path, default=Path(tempfile.gettempdir()))
    a = ap.parse_args()
    dev = "cuda" if torch.cuda.is_available() else "cpu"
    rep = {}
    tok = Tok(a.base)
    m = EG2Dual.load(a.base).to(dev).eval()
    docs = [DOC_PROMPT + t for t in ["The mitochondrion is the powerhouse of the cell.",
                                     "Aspirin irreversibly inhibits cyclooxygenase-1 in platelets.",
                                     "Paris is the capital of France and its largest city."]]
    qs = [QUERY_PROMPT + t for t in ["what inhibits platelet COX-1", "capital of France"]]

    # 1. parity with sentence-transformers
    from sentence_transformers import SentenceTransformer
    st = SentenceTransformer(a.base, device=dev)
    ref = st.encode([t[len(DOC_PROMPT):] for t in docs], prompt=DOC_PROMPT, convert_to_tensor=True, normalize_embeddings=True).float()
    x = tok(docs, len(DOC_PROMPT), 512)
    with torch.no_grad():
        dn, lt = m(x["input_ids"].to(dev), x["attention_mask"].to(dev), x["late_mask"].to(dev))
    cos = F.cosine_similarity(dn, ref.to(dev)).min().item()
    rep["dense_parity_min_cos"] = cos
    del st

    # 2. late mask
    ids = x["input_ids"][0][x["late_mask"][0]]
    rep["late_mask_text"] = tok.tk.decode(ids)
    rep["late_mask_ok"] = rep["late_mask_text"].strip().startswith("The mitochondrion") and "title" not in rep["late_mask_text"]

    # 3. MaxSim
    y = tok(qs, len(QUERY_PROMPT), 64)
    with torch.no_grad():
        qd, ql = m(y["input_ids"].to(dev), y["attention_mask"].to(dev), y["late_mask"].to(dev))
    qm, dm = y["late_mask"].to(dev), x["late_mask"].to(dev)
    s = maxsim(ql.float(), qm, lt.float(), dm)
    naive = torch.zeros_like(s)
    for i in range(len(qs)):
        for j in range(len(docs)):
            qq, dd = ql[i][qm[i]].float(), lt[j][dm[j]].float()
            naive[i, j] = (qq @ dd.T).amax(1).mean()
    rep["maxsim_max_abs_err"] = (s - naive).abs().max().item()
    try:
        from pylate import scores as pls
        ref_pl = pls.colbert_scores(ql.float() * qm[..., None], lt.float(), dm.float())
        rep["pylate_colbert_scores_err"] = (ref_pl / qm.sum(1, keepdim=True) - s).abs().max().item()
    except Exception as e:  # noqa: BLE001
        rep["pylate_import"] = f"{type(e).__name__}: {str(e)[:200]}"
    rep["dense_scores"] = (qd @ dn.T).tolist()
    rep["late_scores"] = s.tolist()

    # 4. GradCache == direct (padded SDPA path, then packed flex path)
    if dev == "cuda":
        args = SimpleNamespace(mb_q=1, mb_d=2, tau_d=0.05, tau_l=0.02, tau_t=0.05, w_dense=1.0, w_late=1.0,
                               w_align=0.1, w_kd_dense=0.5, w_kd_late=0.5)
        d = tok(docs[:2] + docs[2:], len(DOC_PROMPT), 512)
        y.pop("offsets", None)
        d.pop("offsets", None)
        hq = torch.tensor([[train_s1.sid(f"t:{i}")] + [-1] * 7 for i in range(2)])
        hd = torch.tensor([train_s1.sid(f"t:{i}") for i in range(3)])
        tq, td = torch.randn(2, 768) * 0.1, torch.randn(3, 768) * 0.1

        def direct():  # reference: padded SDPA (same math; padded flex has no small-tile path in transformers)
            m.zero_grad()
            impl = m.backbone.config._attn_implementation
            m.backbone.config._attn_implementation = "sdpa"
            qd2, ql2 = train_s1.encode(m, y, 8, grad=True)
            dd2, dl2 = train_s1.encode(m, d, 8, grad=True)
            forbid, lab, teacher = train_s1.targets(hq.cuda(), hd.cuda(), 2, 3, tq.cuda(), td.cuda(), 1)
            out = train_s1.loss_fn(qd2, ql2, y["late_mask"].cuda(), dd2, dl2, d["late_mask"].cuda(),
                                   forbid, teacher, lab, args)
            out["total"].backward()
            m.backbone.config._attn_implementation = impl
            return {n: p.grad.clone() for n, p in m.named_parameters() if p.grad is not None}

        def rel(g1, g2):
            """Global relative error over all parameters, plus the worst parameter (name), the share of
            gradient mass in parameters with > 5% relative error, and any parameter whose grad is not finite."""
            nonfin = sorted({n for g in (g1, g2) for n in g if not torch.isfinite(g[n].float()).all()})
            ok = [n for n in g2 if n in g1 and n not in nonfin]
            num = sum(float((g1[n].float() - g2[n].float()).norm() ** 2) for n in ok)
            den = sum(float(g2[n].float().norm() ** 2) for n in ok)
            per = {n: float((g1[n].float() - g2[n].float()).norm() / (g2[n].float().norm() + 1e-12))
                   for n in ok if g2[n].float().norm() > 0}
            worst = max(per, key=per.get)
            bad = sum(float(g2[n].float().norm() ** 2) for n in per if per[n] > 0.05) / den
            return {"global_rel": round((num / den) ** 0.5, 5), "worst": worst, "worst_rel": round(per[worst], 4),
                    "grad_mass_over_5pct": round(bad, 5), "nonfinite": nonfin[:10], "n_nonfinite": len(nonfin)}

        # fp32 (autocast off) + SDPA math: GradCache must then equal direct to ~1e-5 (tests/grad_diag.py). Under bf16
        # the comparison measures rounding on this tiny batch (4-13% vs fp32, up to 21% for RTX efficient-attn).
        import contextlib
        from torch.nn.attention import SDPBackend, sdpa_kernel
        _ac = torch.autocast
        torch.autocast = lambda *x, **k: contextlib.nullcontext()
        _kern = sdpa_kernel([SDPBackend.MATH])
        _kern.__enter__()
        m.train()
        for attn, packed in (("sdpa", False), ("flex_attention", True)):
            m.backbone.config._attn_implementation = attn
            batch = {"q": y, "d": d, "gold": hq, "doc_hash": hd, "tq": tq, "td": td,
                     "qgroup": torch.full((2,), -1), "dgroup": torch.full((3,), -1)}
            if packed:
                batch["qp"], batch["dp"] = pack(y, 256), pack(d, 1024)
            m.zero_grad()
            key = f"gradcache_fp32_rel_err_{attn}{'_packed' if packed else ''}"
            try:  # fp32 flex tiles exceed RTX PRO 6000 shared memory (131 KB > 101 KB); B200 runs it
                train_s1.train_step(m, batch, args)  # packed rows use the small flex tiles (model default)
            except Exception as e:  # noqa: BLE001
                rep[key] = f"skipped: {type(e).__name__}: {str(e)[:160]}"
                continue
            g1 = {n: p.grad.clone() for n, p in m.named_parameters() if p.grad is not None}
            rep[key] = rel(g1, direct())
        m.backbone.config._attn_implementation = "sdpa"
        rep["gradcache_noise_floor_direct_vs_direct"] = rel(direct(), direct())
        _kern.__exit__(None, None, None)
        torch.autocast = _ac
        m.zero_grad()
        m.eval()

    # 5. export round trip
    out = m.export_snapshot(a.tmp / "eg2late-export-check", {"check": True})
    m2 = EG2Dual.load(out).to(dev).eval()
    with torch.no_grad():
        dn2, lt2 = m2(x["input_ids"].to(dev), x["attention_mask"].to(dev), x["late_mask"].to(dev))
    rep["export_dense_max_err"] = (dn2 - dn).abs().max().item()  # bf16 export of fp32 weights
    rep["export_late_max_err"] = (lt2.float() - lt.float()).abs().max().item()
    from safetensors import safe_open
    with safe_open(Path(a.base) / "model.safetensors", "pt") as f0, safe_open(out / "model.safetensors", "pt") as f1:
        k0, k1 = set(f0.keys()), set(f1.keys())
        rep["export_same_keys"] = k0 == k1
        tower = [k for k in k0 if not k.startswith("language_model.")][:3]
        rep["export_towers_identical"] = all(torch.equal(f0.get_tensor(k), f1.get_tensor(k)) for k in tower)
        lm = [k for k in k0 if k.startswith("language_model.")][:5]
        rep["export_lm_identical_untrained"] = all(torch.equal(f0.get_tensor(k), f1.get_tensor(k)) for k in lm)
    print(json.dumps(rep, indent=1))


if __name__ == "__main__":
    main()
