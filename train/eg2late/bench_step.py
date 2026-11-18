"""Throughput of one S1 training step (GradCache fwd no-grad + fwd/bwd) under different attention set-ups, on real
batches. Also: which SDPA backends accept EG2's head dims (256 sliding / 512 full), and packed-vs-padded parity.

python -m eg2late.bench_step --base SNAP --data DATA --source hotpotqa [--steps 5]
Prints one JSON line per variant: real tokens/s, s/step, peak GB.
"""
import argparse
import json
import time

import torch
import torch.nn.functional as F

from .model import EG2Dual, Tok
from .train_s1 import Batches, Source, encode, encode_packed, make_plan, to_dev, train_step


def attn_backends():
    from torch.nn.attention import SDPBackend, sdpa_kernel
    res = {}
    for D in (256, 512):
        for masked in (False, True):
            B, H, L = 128, 4, 320
            q, k, v = (torch.randn(B, H, L, D, device="cuda", dtype=torch.bfloat16, requires_grad=True)
                       for _ in range(3))
            m = torch.ones(B, 1, L, L, device="cuda", dtype=torch.bool) if masked else None
            if masked:
                m[: B // 2, :, :, L // 2:] = False
            for be in (SDPBackend.FLASH_ATTENTION, SDPBackend.EFFICIENT_ATTENTION, SDPBackend.CUDNN_ATTENTION,
                       SDPBackend.MATH):
                key = f"D{D}/{'mask' if masked else 'nomask'}/{be.name}"
                try:
                    with sdpa_kernel(be):
                        for it in range(4):
                            if it == 1:
                                torch.cuda.synchronize()
                                t0 = time.time()
                            o = F.scaled_dot_product_attention(q, k, v, attn_mask=m, scale=1.0)
                            o.sum().backward()
                        torch.cuda.synchronize()
                    res[key] = round((time.time() - t0) / 3 * 1000, 2)  # ms fwd+bwd
                except Exception as e:  # noqa: BLE001
                    res[key] = f"unsupported: {str(e).splitlines()[0][:80]}"
            del q, k, v
    return res


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", required=True)
    ap.add_argument("--data", required=True)
    ap.add_argument("--source", default="hotpotqa")
    ap.add_argument("--steps", type=int, default=5)
    ap.add_argument("--warmup", type=int, default=2)
    ap.add_argument("--variants", default="sdpa,pack32k,pack64k_compile")
    a0 = ap.parse_args()
    torch.backends.cuda.matmul.allow_tf32 = True
    print(json.dumps({"gpu": torch.cuda.get_device_name(), "torch": torch.__version__}), flush=True)
    print(json.dumps({"sdpa_ms_fwd_bwd_B128_L320": attn_backends()}), flush=True)

    from pathlib import Path
    tok = Tok(a0.base)
    model = EG2Dual.load(a0.base).cuda().train()
    src = Source(Path(a0.data) / a0.source, None, 0)
    n = a0.warmup + a0.steps

    class A:  # train_s1 defaults
        batch, n_neg, neg_pool, mb_q, mb_d, q_len, d_len = 128, 7, 15, 128, 128, 64, 320
        tau_d, tau_l, tau_t, w_dense, w_late, w_align, w_kd = 0.05, 0.02, 0.05, 1.0, 1.0, 0.1, 0.5
        w_kd_dense, w_kd_late = 0.5, 0.5
        pack_tokens, pack_tokens_q = 0, 8192

    plan = make_plan([src], A.batch, 0)[:n]
    batches_pad = [Batches([src], plan, tok, A)[i] for i in range(n)]
    real_tok = sum(int(b["q"]["attention_mask"].sum() + b["d"]["attention_mask"].sum()) for b in batches_pad) / n
    pad_tok = sum(b["q"]["input_ids"].numel() + b["d"]["input_ids"].numel() for b in batches_pad) / n
    print(json.dumps({"real_tokens_per_step": round(real_tok), "padded_tokens_per_step": round(pad_tok)}), flush=True)

    def packed(T):
        A.pack_tokens = T
        bs = [Batches([src], plan, tok, A)[i] for i in range(n)]
        A.pack_tokens = 0
        return bs

    # parity: padded SDPA vs packed flex on the first batch's documents (no grad); falls back to small flex tiles
    from .model import FLEX_SMALL

    def parity():
        with torch.no_grad():
            model.backbone.config._attn_implementation = "sdpa"
            b = batches_pad[0]
            d0, l0 = encode(model, b["d"], 128)
            model.backbone.config._attn_implementation = "flex_attention"
            bp = packed(32768)[0]
            d1, l1 = encode_packed(model, bp["d"], [to_dev(p) for p in bp["dp"]])
            m = b["d"]["late_mask"].cuda()
            cos_d = (d0 * d1).sum(-1)
            cos_l = (l0.float() * l1.float()).sum(-1)[m]
            return {"dense_cos_min": round(float(cos_d.min()), 5), "dense_cos_mean": round(float(cos_d.mean()), 6),
                    "late_cos_min": round(float(cos_l.min()), 5), "late_cos_mean": round(float(cos_l.mean()), 6)}

    model.flex_kernel_options = FLEX_SMALL  # default tiles: shared-memory overflow (RTX), misaligned address (B200)
    try:
        print(json.dumps({"parity_packed_vs_padded": parity()}), flush=True)
    except Exception as e:  # noqa: BLE001
        print(json.dumps({"parity_error": f"{type(e).__name__}: {str(e)[-800:]}"}), flush=True)

    from torch.nn.attention import SDPBackend, sdpa_kernel
    import contextlib
    for v in a0.variants.split(","):
        ctx = contextlib.nullcontext()
        if v == "sdpa":
            model.backbone.config._attn_implementation, bs = "sdpa", batches_pad
        elif v == "sdpa_eff":
            model.backbone.config._attn_implementation, bs = "sdpa", batches_pad
            ctx = sdpa_kernel([SDPBackend.EFFICIENT_ATTENTION, SDPBackend.CUDNN_ATTENTION])
        elif v == "flex":
            model.backbone.config._attn_implementation, bs = "flex_attention", batches_pad
        elif v.startswith("pack"):
            T = int(v[4:].split("_")[0].rstrip("k")) * 1024
            model.backbone.config._attn_implementation, bs = "flex_attention", packed(T)
            model.flex_kernel_options = None if "_default" in v else FLEX_SMALL
            if v.endswith("_compile"):
                for layer in model.backbone.layers:
                    layer.compile(dynamic=False)
        else:
            raise SystemExit(f"unknown variant {v}")
        torch.cuda.reset_peak_memory_stats()
        try:
            with ctx:
                for i, b in enumerate(bs):
                    if i == a0.warmup:
                        torch.cuda.synchronize()
                        t0 = time.time()
                    out, _ = train_step(model, b, A)
                    model.zero_grad(set_to_none=True)
                torch.cuda.synchronize()
            dt = (time.time() - t0) / a0.steps
            print(json.dumps({"variant": v, "s_per_step": round(dt, 3), "real_tok_s": round(real_tok / dt),
                              "peak_gb": round(torch.cuda.max_memory_allocated() / 2**30, 1),
                              "loss": round(out["total"], 4)}), flush=True)
        except Exception as e:  # noqa: BLE001
            print(json.dumps({"variant": v, "error": f"{type(e).__name__}: {str(e)[:300]}"}), flush=True)
            torch.cuda.empty_cache()


if __name__ == "__main__":
    main()
