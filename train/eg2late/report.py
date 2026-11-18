"""Base vs trained tables (markdown) from two train-eval output dirs: python -m eg2late.report BASE_DIR TRAINED_DIR

Dense head: flashembed numbers (production runtime) where present, torch numbers alongside as a parity check.
Late head: torch only (the base late head is the untrained SVD-init projection). Paired bootstrap 95% CIs of
trained - base use the per-query metrics in the torch JSONs.
"""
import json
import sys
from pathlib import Path

import numpy as np

from .evaluate import CONTEB, NANO


def load(p: Path):
    return json.loads(p.read_text()) if p.exists() else None


def ci(a, b, n=2000):
    d = np.asarray(b) - np.asarray(a)
    rng = np.random.default_rng(0)
    bs = [d[rng.integers(0, len(d), len(d))].mean() for _ in range(n)]
    return d.mean(), np.percentile(bs, 2.5), np.percentile(bs, 97.5)


def fmt_ci(t):
    return f"{t[0]:+.3f} ({t[1]:+.3f}, {t[2]:+.3f})"


def main():
    B, T = Path(sys.argv[1]), Path(sys.argv[2])
    out = []
    nb, nt = load(B / "nanobeir.json"), load(T / "nanobeir.json")
    if nb and nt:
        out.append("NanoBEIR nDCG@10 (torch, bf16)\n")
        out.append("| Set | Dense base | Dense S1 | Δ (95% CI) | Late base (SVD init) | Late S1 | Δ (95% CI) |")
        out.append("|---|---|---|---|---|---|---|")
        for ds in NANO + ["_average"]:
            rb, rt = nb["results"][ds], nt["results"][ds]
            cells = []
            for h in ("dense", "late"):
                if ds == "_average":
                    xb = np.concatenate([[np.mean(nb["per_query"][f"{d}/{h}"])] for d in NANO])
                    xt = np.concatenate([[np.mean(nt["per_query"][f"{d}/{h}"])] for d in NANO])
                    c = ci(xb, xt)  # bootstrap over the 13 sets
                else:
                    c = ci(nb["per_query"][f"{ds}/{h}"], nt["per_query"][f"{ds}/{h}"])
                cells += [f"{rb[h]:.3f}", f"{rt[h]:.3f}", fmt_ci(c)]
            name = "**Average (13)**" if ds == "_average" else ds.replace("Nano", "")
            out.append(f"| {name} | " + " | ".join(cells) + " |")
    cb, ct = load(B / "conteb-torch.json"), load(T / "conteb-torch.json")
    fb, ft = load(B / "conteb-fe.json"), load(T / "conteb-fe.json")
    if cb and ct:
        out.append("\nConTEB nDCG@10 (8 sets). fe = flashembed-gpu-late1 dense; torch = this repo's evaluator\n")
        keys = [("naive/dense", "naive"), ("late/dense", "late"), ("naive/late", None), ("late/late", None)]
        out.append("| Set | " + " | ".join(f"{k} base | {k} S1" for k, _ in keys) + " | fe naive base | fe naive S1 | fe late base | fe late S1 |")
        out.append("|---|" + "---|" * (2 * len(keys) + 4))
        for ds in CONTEB + ["_average"]:
            if ds not in cb["results"]:
                continue
            row = []
            for k, _ in keys:
                row += [f"{cb['results'][ds][k]:.3f}", f"{ct['results'][ds][k]:.3f}"]
            for v in ("naive", "late"):
                for f in (fb, ft):
                    row.append(f"{f[ds][v]:.3f}" if f and ds in f and v in f[ds] else "–")
            out.append(f"| {ds} | " + " | ".join(row) + " |")
    pb, pt = load(B / "pmc-torch.json"), load(T / "pmc-torch.json")
    eb, et = load(B / "pmc-fe/results/results.json"), load(T / "pmc-fe/results/results.json")
    if pb and pt:
        out.append("\n98k PMC corpus, article-level MRR@10 (3,000 queries per task, variant C units)\n")
        out.append("| Task | Head | Base | S1 | Δ (95% CI) | fe base | fe S1 |")
        out.append("|---|---|---|---|---|---|---|")
        for t in ("abstract->body", "title->article"):
            for h in ("dense", "late"):
                c = ci(pb["per_query"][f"{t}/{h}"], pt["per_query"][f"{t}/{h}"])
                fe = [f"{e['tasks'][t]['C']['MRR@10']:.3f}" if e and h == "dense" else "–" for e in (eb, et)]
                out.append(f"| {t} | {h} | {pb['results'][t][h]['MRR@10']:.3f} | {pt['results'][t][h]['MRR@10']:.3f} | "
                           f"{fmt_ci(c)} | {fe[0]} | {fe[1]} |")
    print("\n".join(out))


if __name__ == "__main__":
    main()
