"""Build the markdown tables for docs/RUST_EMBED.md from finished bench dirs (each already run through compare.py).

  python report.py OUT_DIR [OUT_DIR ...] > report.md
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

import numpy as np


def main() -> None:
    dirs = [Path(d) for d in sys.argv[1:]]
    print("### Parity (worst case over all thread/batch configs; reference = Python card runtime at the same config)\n")
    print("| model | modality | runtime | configs | dense min cos | dense max (1-cos) | late min cos (per token) | "
          "late max (1-cos) | token layout identical |")
    print("|---|---|---|---|---|---|---|---|---|")
    for d in dirs:
        par = json.load(open(d / "parity.json"))
        groups: dict = {}
        for tag, v in par.items():
            if "~" in tag:
                continue
            rt, model, modality = tag.split("-")[:3]
            groups.setdefault((model, modality, rt), []).append(v)
        for (model, modality, rt), vs in sorted(groups.items()):
            name = {"rs": "Rust (ort)", "pyort": "Python (ort)", "rsc": "Rust (candle)"}.get(rt, rt)
            dmin = min(v["dense_min_cos"] for v in vs)
            dmean = float(np.mean([v["dense_mean_cos"] for v in vs]))
            if "late_rows_equal" in vs[0]:
                same = all(v["late_rows_equal"] for v in vs)
                lm = min(v['late_min_cos'] for v in vs) if same else None
                lmin = f"{lm:.7f}" if same else "-"
                lq = f"{1 - lm:.1e}" if same else "-"
            else:
                same, lmin, lq = None, "n/a", "n/a"
            print(f"| {model} | {modality} | {name} | {len(vs)} | {dmin:.7f} | {1 - dmin:.1e} | {lmin} | {lq} | "
                  f"{'yes' if same else ('n/a' if same is None else 'NO')} |")
    print()
    for d in dirs:
        s = json.load(open(d / "summary.json"))
        lscpu = (d / "lscpu.txt").read_text() if (d / "lscpu.txt").exists() else ""
        cpu = next((l.split(":", 1)[1].strip() for l in lscpu.splitlines() if l.startswith("Model name")), "?")
        jobs = sorted({json.load(open(p)).get("job") for p in d.glob("rep0/*-t*-b*.json") if not p.name.endswith("ids.json")} - {None})
        m, mod = s[0]["model"], s[0]["modality"]
        print(f"### {m} {mod} — {cpu}, job {', '.join(map(str, jobs))}\n")
        print("| threads | batch | Python torch items/s | Rust ort items/s | Rust / torch | Python ort items/s | "
              "Rust / Python-ort | torch peak RSS MB | Rust peak RSS MB | Python-ort peak RSS MB |")
        print("|---|---|---|---|---|---|---|---|---|---|")
        for r in s:
            f = lambda v, fmt="{:.2f}": fmt.format(v) if v is not None else "-"
            rr = r["rs_items_s"] / r["py_items_s"] if r["rs_items_s"] and r["py_items_s"] else None
            ro = r["rs_items_s"] / r["pyort_items_s"] if r["rs_items_s"] and r["pyort_items_s"] else None
            print(f"| {r['threads']} | {r['batch']} | {f(r['py_items_s'])} | {f(r['rs_items_s'])} | "
                  f"**{f(rr, '{:.2f}x')}** | {f(r['pyort_items_s'])} | {f(ro, '{:.2f}x')} | "
                  f"{f(r['py_rss_mb'], '{:.0f}')} | {f(r['rs_rss_mb'], '{:.0f}')} | {f(r['pyort_rss_mb'], '{:.0f}')} |")
        ratios = [r["rs_items_s"] / r["py_items_s"] for r in s if r["rs_items_s"] and r["py_items_s"]]
        spread = [max(x) / min(x) for r in s for x in (r["py_items_s_all"], r["rs_items_s_all"]) if len(x) > 1]
        print(f"\nRust/torch over the {len(ratios)} configs: min {min(ratios):.2f}x, median {np.median(ratios):.2f}x, "
              f"max {max(ratios):.2f}x. Run-to-run spread (max/min over reps of one config): median "
              f"{np.median(spread):.2f}x, worst {max(spread):.2f}x.\n")


if __name__ == "__main__":
    main()
