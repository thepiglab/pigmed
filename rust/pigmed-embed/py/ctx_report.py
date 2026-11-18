"""Markdown table for a context-length sweep dir (examples/slurm/embed-rs-ctx.sbatch output).

  python ctx_report.py OUT_DIR
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).parent))
from compare import load_vecs, parity  # noqa: E402


def main():
    root = Path(sys.argv[1])
    d = root / "rep0" if (root / "rep0").exists() else root  # vectors live in rep0
    allr = {}
    for p in list(root.glob("*-t*-b*.json")) + list(root.glob("rep*/*-t*-b*.json")):
        if p.name.endswith(".ids.json"):
            continue
        r = json.load(open(p))
        allr.setdefault(r["tag"], []).append(r)
    rows = {}
    for tag, rs_ in allr.items():  # median items/s, max RSS over reps
        r = dict(rs_[0])
        r["items_per_s"] = float(np.median([x["items_per_s"] for x in rs_]))
        r["peak_rss_mb"] = max(x["peak_rss_mb"] for x in rs_)
        r["reps"] = len(rs_)
        rows[tag] = r
    lscpu = (root / "lscpu.txt").read_text() if (root / "lscpu.txt").exists() else ""
    cpu = next((l.split(":", 1)[1].strip() for l in lscpu.splitlines() if l.startswith("Model name")), "?")
    any_r = next(iter(rows.values()))
    model, t = any_r["model"], any_r["threads"]
    print(f"### {model} text, context sweep — {cpu}, {t} threads, batch 1, job {any_r.get('job')}, "
          f"median of {any_r.get('reps', 1)} reps (runtime order alternated), peak RSS = max over reps\n")
    print("| input | tokens/item | Python items/s | Rust items/s | Rust / Python | Python s/item | Rust s/item | "
          "Python peak RSS MB | Rust peak RSS MB | dense min cos | late min cos | flag |")
    print("|---|---|---|---|---|---|---|---|---|---|---|---|")
    sfxs = sorted({tag.split("_", 1)[1] for tag in rows if "_" in tag},
                  key=lambda x: (not x.startswith("L"), int(x[1:]) if x.startswith("L") and x[1:].isdigit() else 0, x))
    for sfx in sfxs:
        b = "b4" if sfx == "mixed" else "b1"
        for bb in (["b1", "b4"] if sfx == "mixed" else [b]):
            py, rs = rows.get(f"py-{model}-text-t{t}-{bb}_{sfx}"), rows.get(f"rs-{model}-text-t{t}-{bb}_{sfx}")
            if not py and not rs:
                continue
            src = rs if rs and rs.get("tokens_total") else py
            ntok = src["tokens_total"] / src["n"] if src and src.get("tokens_total") else None
            par, flag = {}, []
            if py and rs:
                try:
                    par = parity(load_vecs(d, py["tag"]), load_vecs(d, rs["tag"]))
                except Exception as e:  # missing vectors (e.g. a run died)
                    par = {"error": str(e)}
            f = lambda v, fmt="{:.3f}": fmt.format(v) if v is not None else "FAILED"
            ipy, irs = (py or {}).get("items_per_s"), (rs or {}).get("items_per_s")
            ratio = irs / ipy if ipy and irs else None
            if ratio is not None and ratio < 0.95:
                flag.append("Rust slower")
            if py and rs and rs["peak_rss_mb"] > py["peak_rss_mb"]:
                flag.append("Rust heavier")
            if par.get("dense_min_cos", 1) < 0.999 or par.get("late_min_cos", 1) < 0.999:
                flag.append("PARITY < 0.999")
            if not rs:
                flag.append("Rust failed")
            lc = par.get("late_min_cos")
            print(f"| {sfx} {bb} | {f(ntok, '{:.0f}')} | {f(ipy)} | {f(irs)} | {f(ratio, '{:.2f}x')} | "
                  f"{f(1 / ipy if ipy else None, '{:.2f}')} | {f(1 / irs if irs else None, '{:.2f}')} | "
                  f"{f((py or {}).get('peak_rss_mb'), '{:.0f}')} | {f((rs or {}).get('peak_rss_mb'), '{:.0f}')} | "
                  f"{f(par.get('dense_min_cos'), '{:.7f}')} | {f(lc, '{:.7f}') if lc is not None else 'n/a'} | {', '.join(flag)} |")
    # batching invariance on the mixed-length batch
    for a, b in ((f"py-{model}-text-t{t}-b1_mixed", f"py-{model}-text-t{t}-b4_mixed"),
                 (f"py-{model}-text-t{t}-b1_mixed", f"rs-{model}-text-t{t}-b4_mixed")):
        try:
            p = parity(load_vecs(d, a), load_vecs(d, b))
            print(f"\nmixed-length batch: {b} vs {a}: dense min cos {p['dense_min_cos']:.7f}"
                  + (f", late min cos {p['late_min_cos']:.7f}" if "late_min_cos" in p else ""))
        except Exception as e:
            print(f"\nmixed-length batch {b} vs {a}: n/a ({e})")
    for rt in ("py", "rs"):
        r = rows.get(f"{rt}-{model}-text-t{t}-b1_over")
        if r:
            tok = f"{r['tokens_total'] / r['n']:.0f} tokens/item after truncation" if r.get("tokens_total") else \
                "token count not exposed (identical vectors to the Rust run)"
            print(f"over-max input ({rt}): {tok}"
                  + (f"; sentence-transformers default max_seq_length = {r.get('st_default_max_seq_length')}"
                     if r.get("st_default_max_seq_length") else ""))


if __name__ == "__main__":
    main()
