"""Summarize a bench output dir: throughput / RSS table and vector parity against the Python (torch) reference.

  python compare.py --out DIR [--ref DIR_WITH_py-*.npy] [--md table.md]

Parity per item: cosine(dense_rs, dense_py); late vectors: per-token cosine over the item's valid rows
(requires identical token counts, which also checks the preprocessing/token layout).
"""

from __future__ import annotations

import argparse
import glob
import json
import os
from pathlib import Path

import numpy as np


def cos(a, b):
    return (a * b).sum(-1) / (np.linalg.norm(a, axis=-1) * np.linalg.norm(b, axis=-1) + 1e-12)


def load_vecs(d: Path, tag: str):
    p = d / f"{tag}.dense.npy"
    if not p.exists():
        return None
    v = {"dense": np.load(p), "ids": json.load(open(d / f"{tag}.ids.json"))}
    if (d / f"{tag}.late.npy").exists():
        v["late"] = np.load(d / f"{tag}.late.npy")
        v["offs"] = np.load(d / f"{tag}.late_offsets.npy")
    return v


def parity(ref, got):
    assert ref["ids"] == got["ids"], "different inputs"
    r = {"n": len(ref["ids"])}
    c = cos(ref["dense"].astype(np.float64), got["dense"].astype(np.float64))
    r.update(dense_min_cos=float(c.min()), dense_mean_cos=float(c.mean()))
    if "late" in ref and "late" in got:
        lr, lg = np.diff(ref["offs"]), np.diff(got["offs"])
        r["late_rows_equal"] = bool((lr == lg).all())
        if r["late_rows_equal"]:
            tc = cos(ref["late"].astype(np.float64), got["late"].astype(np.float64))
            r.update(late_min_cos=float(tc.min()), late_mean_cos=float(tc.mean()),
                     late_p001_cos=float(np.quantile(tc, 0.001)))
        else:
            bad = int((lr != lg).sum())
            r["late_rows_mismatch_items"] = bad
    return r


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--ref", type=Path, default=None)
    ap.add_argument("--md", type=Path, default=None)
    args = ap.parse_args()
    files = sorted(glob.glob(str(args.out / "*.json")) + glob.glob(str(args.out / "rep*" / "*.json")))
    rows = [json.load(open(p)) for p in files if not p.endswith(("ids.json", "parity.json", "summary.json"))]
    rows = [r for r in rows if "items_per_s" in r]
    by: dict = {}
    for r in rows:
        by.setdefault((r["model"], r["modality"], r["threads"], r["batch"], r["runtime"]), []).append(r)

    def med(rs, k):
        return float(np.median([r[k] for r in rs])) if rs else None

    summary = []
    lines = ["| model | modality | threads | batch | py items/s | rs items/s | rs/py | py-ort items/s | rs/py-ort | "
             "py RSS MB | rs RSS MB | py-ort RSS MB | reps |",
             "|---|---|---|---|---|---|---|---|---|---|---|---|---|"]
    for key in sorted({k[:4] for k in by}):
        py, rs, po = (by.get((*key, rt), []) for rt in ("python", "rust-ort", "python-ort"))
        ipy, irs, ipo = med(py, "items_per_s"), med(rs, "items_per_s"), med(po, "items_per_s")
        mpy, mrs, mpo = (max((r["peak_rss_mb"] for r in x), default=None) for x in (py, rs, po))
        f = lambda v, fmt="{:.2f}": fmt.format(v) if v is not None else "-"
        lines.append(f"| {key[0]} | {key[1]} | {key[2]} | {key[3]} | {f(ipy)} | {f(irs)} | "
                     f"{f(irs / ipy if ipy and irs else None, '{:.2f}x')} | {f(ipo)} | "
                     f"{f(irs / ipo if ipo and irs else None, '{:.2f}x')} | {f(mpy, '{:.0f}')} | {f(mrs, '{:.0f}')} | "
                     f"{f(mpo, '{:.0f}')} | {len(py)}/{len(rs)}/{len(po)} |")
        summary.append({"model": key[0], "modality": key[1], "threads": key[2], "batch": key[3],
                        "py_items_s": ipy, "rs_items_s": irs, "pyort_items_s": ipo,
                        "py_items_s_all": [r["items_per_s"] for r in py], "rs_items_s_all": [r["items_per_s"] for r in rs],
                        "pyort_items_s_all": [r["items_per_s"] for r in po],
                        "py_rss_mb": mpy, "rs_rss_mb": mrs, "pyort_rss_mb": mpo})
    cpus = sorted({r.get("cpu", "?") for r in rows})
    jobs = sorted({str(r.get("job")) for r in rows})
    hosts = sorted({str(r.get("host")) for r in rows})
    table = "\n".join(lines) + (f"\n\nmedian items/s over reps (runtime order rotated per rep); RSS = max over reps. "
                                f"CPU: {', '.join(cpus)}; hosts: {', '.join(hosts)}; jobs: {', '.join(jobs)}\n")
    print(table)
    (args.out / "summary.json").write_text(json.dumps(summary, indent=1))

    par = {}
    vecdir = args.out / "rep0" if (args.out / "rep0").exists() else args.out
    refdir = args.ref or vecdir
    for p in sorted(glob.glob(str(vecdir / "*.dense.npy"))):
        tag = os.path.basename(p)[: -len(".dense.npy")]
        if tag.startswith("py-"):
            continue
        rt, model, modality, t, b = tag.split("-")
        cands = sorted(glob.glob(str(refdir / f"py-{model}-{modality}-*.dense.npy")))
        same = refdir / f"py-{model}-{modality}-{t}-{b}.dense.npy"
        reft = (same if same.exists() else Path(cands[0]) if cands else None)
        if reft is None:
            continue
        rtag = os.path.basename(str(reft))[: -len(".dense.npy")]
        par[tag] = {"ref": rtag, **parity(load_vecs(refdir, rtag), load_vecs(vecdir, tag))}
        print(tag, "vs", rtag, json.dumps({k: (round(v, 7) if isinstance(v, float) else v) for k, v in par[tag].items()}))
    # also py vs py across batch sizes (padding / batching invariance of the reference itself)
    pys = sorted(glob.glob(str(refdir / "py-*.dense.npy")))
    for p in pys[1:]:
        tag = os.path.basename(p)[: -len(".dense.npy")]
        base = os.path.basename(pys[0])[: -len(".dense.npy")]
        if tag.split("-")[1:3] == base.split("-")[1:3]:
            par[f"{tag}~{base}"] = {"ref": base, **parity(load_vecs(refdir, base), load_vecs(refdir, tag))}
    if par:
        (args.out / "parity.json").write_text(json.dumps(par, indent=1))
    if args.md:
        args.md.write_text(table)


if __name__ == "__main__":
    main()
