"""Collect the final benchmark matrix (batch jobs fanned out per CPU family) into markdown for docs/RUST_EMBED.md.

  python final_report.py $EMBED_ROOT/out/final > final.md

Layout: <root>/<family>/{bench-<model>-<modality>-t<16|32>, ctx-<model>-<len>-t<T>, ctx-<model>-extras-t16, mm-<model>, ops}
Every number is a median over interleaved reps (Python and Rust alternate), RSS = max over reps; parity is the
worst case over all items / configs against the Python card runtime on the same inputs.
"""

from __future__ import annotations

import glob
import json
import sys
from collections import defaultdict
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).parent))
from compare import load_vecs, parity  # noqa: E402

FAM = {"rome": "EPYC 7702 (Zen 2, 2 sockets)", "milan": "EPYC 75F3 (Zen 3, 2 sockets)", "turin": "EPYC 9655P (Zen 5, 1 socket, AVX-512)"}


def rows_in(d: Path):
    out = defaultdict(list)
    for p in glob.glob(str(d / "**" / "*.json"), recursive=True):
        if p.endswith((".ids.json", "parity.json", "summary.json")) or "/lists/" in p:
            continue
        try:
            r = json.load(open(p))
        except Exception:
            continue
        if "items_per_s" in r:
            out[r["tag"]].append(r)
    return out


def med(rs, k="items_per_s"):
    return float(np.median([r[k] for r in rs])) if rs else None


def vec_dir(d: Path):
    return d / "rep0" if (d / "rep0").exists() else d


def par_of(d: Path, pytag: str, rstag: str):
    try:
        return parity(load_vecs(vec_dir(d), pytag), load_vecs(vec_dir(d), rstag))
    except Exception as e:
        return {"error": str(e)[:80]}


def f(v, fmt="{:.2f}"):
    return fmt.format(v) if v is not None else "-"


def worst(ps, k):
    vals = [p[k] for p in ps if k in p]
    return min(vals) if vals else None


def grid(root: Path, fam: str, out: list, stats: list):
    for d in sorted(root.glob("bench-*")):
        allr = rows_in(d)
        if not allr:
            continue
        keys = sorted({(r[0]["model"], r[0]["modality"], r[0]["threads"], r[0]["batch"]) for r in allr.values()})
        out.append(f"\n**{d.name}** ({FAM[fam]})\n")
        out.append("| threads | batch | Python items/s | Rust items/s | Rust / Python | Python RSS MB | Rust RSS MB | Rust / Python RSS | parity (dense min cos / late min cos) |")
        out.append("|---|---|---|---|---|---|---|---|---|")
        for m, mod, t, b in keys:
            py, rs = allr.get(f"py-{m}-{mod}-t{t}-b{b}", []), allr.get(f"rs-{m}-{mod}-t{t}-b{b}", [])
            ipy, irs = med(py), med(rs)
            mpy = max((r["peak_rss_mb"] for r in py), default=None)
            mrs = max((r["peak_rss_mb"] for r in rs), default=None)
            p = par_of(d, f"py-{m}-{mod}-t{t}-b{b}", f"rs-{m}-{mod}-t{t}-b{b}")
            sp = irs / ipy if ipy and irs else None
            mr = mrs / mpy if mpy and mrs else None
            stats.append({"fam": fam, "kind": "grid", "case": f"{m} {mod} t{t} b{b}", "model": m, "mod": mod, "speed": sp, "mem": mr,
                          "dcos": p.get("dense_min_cos"), "lcos": p.get("late_min_cos")})
            lc = f"{p['late_min_cos']:.7f}" if "late_min_cos" in p else "n/a"
            out.append(f"| {t} | {b} | {f(ipy)} | {f(irs)} | **{f(sp, '{:.2f}x')}** | {f(mpy, '{:.0f}')} | {f(mrs, '{:.0f}')} | "
                       f"{f(mr, '{:.2f}')} | {f(p.get('dense_min_cos'), '{:.7f}')} / {lc} |")


def ctx(root: Path, fam: str, out: list, stats: list):
    by = defaultdict(list)  # (model, T) -> rows
    for d in sorted(root.glob("ctx-*-t*")):
        parts = d.name.split("-")  # ctx-<model>-<len>-t<T>
        model, length, t = parts[1], parts[2], parts[3]
        allr = rows_in(d)
        for tag, rs_ in allr.items():
            if not tag.startswith("py-"):
                continue
            rtag = "rs" + tag[2:]
            rs = allr.get(rtag, [])
            sfx = tag.split("_", 1)[1] if "_" in tag else length
            p = par_of(d, tag, rtag)
            src = rs or rs_
            ntok = (src[0].get("tokens_total") or 0) / max(1, src[0]["n"]) if src else None
            by[(model, t)].append((sfx, tag.split("-")[4].split("_")[0], ntok, rs_, rs, p))
    order = lambda s: (0, int(s[1:])) if s.startswith("L") and s[1:].isdigit() else (1, s)
    for (model, t), items in sorted(by.items()):
        out.append(f"\n**{model} text, {t} threads** ({FAM[fam]}; batch 1 unless noted)\n")
        out.append("| input | batch | tokens/item | Python items/s | Rust items/s | Rust / Python | Python s/item | Rust s/item | "
                   "Python RSS MB | Rust RSS MB | dense min cos | late min cos |")
        out.append("|---|---|---|---|---|---|---|---|---|---|---|---|")
        for sfx, b, ntok, py, rs, p in sorted(items, key=lambda x: (order(x[0]), x[1])):
            ipy, irs = med(py), med(rs)
            mpy = max((r["peak_rss_mb"] for r in py), default=None)
            mrs = max((r["peak_rss_mb"] for r in rs), default=None)
            sp = irs / ipy if ipy and irs else None
            stats.append({"fam": fam, "kind": "ctx", "case": f"{model} {sfx} {b} t{t[1:]}", "model": model, "mod": "text", "speed": sp,
                          "mem": mrs / mpy if mpy and mrs else None, "dcos": p.get("dense_min_cos"), "lcos": p.get("late_min_cos")})
            lc = f"{p['late_min_cos']:.7f}" if "late_min_cos" in p else "n/a"
            out.append(f"| {sfx} | {b} | {f(ntok, '{:.0f}')} | {f(ipy, '{:.3f}')} | {f(irs, '{:.3f}')} | **{f(sp, '{:.2f}x')}** | "
                       f"{f(1 / ipy if ipy else None)} | {f(1 / irs if irs else None)} | {f(mpy, '{:.0f}')} | {f(mrs, '{:.0f}')} | "
                       f"{f(p.get('dense_min_cos'), '{:.7f}')} | {lc} |")


def mm(root: Path, fam: str, out: list, stats: list):
    for d in sorted(root.glob("mm-*")):
        allr = rows_in(d)
        model = d.name.split("-", 1)[1]
        logs = {Path(p).stem: open(p).read() for p in glob.glob(str(d / "**" / "*.log"), recursive=True)}
        tags = sorted({t.split("_", 1)[1] for t in allr if "_" in t} | {Path(k).stem.split("_", 1)[1] for k in logs if "_" in k})
        out.append(f"\n**{model}: modalities, file types, sizes** ({FAM[fam]}, 16 threads, batch 1)\n")
        out.append("| input group | modality | items | Python | Rust | Rust / Python | Python RSS MB | Rust RSS MB | dense min cos | late min cos |")
        out.append("|---|---|---|---|---|---|---|---|---|---|")
        for sfx in tags:
            py = [r for t, rs_ in allr.items() if t.startswith("py-") and t.endswith("_" + sfx) for r in rs_]
            rs = [r for t, rs_ in allr.items() if t.startswith("rs-") and t.endswith("_" + sfx) for r in rs_]
            mod = (py or rs)[0]["modality"] if (py or rs) else "?"
            n = (py or rs)[0]["n"] if (py or rs) else 0

            def status(rt, rows):
                if rows:
                    return f"{med(rows):.2f} items/s"
                lg = logs.get(f"{rt}_{sfx}", "")
                line = next((l for l in lg.splitlines() if "Error" in l or "error" in l), "")
                return "REJECTED: " + line.strip()[:90] if lg else "-"
            p = par_of(d, py[0]["tag"], rs[0]["tag"]) if py and rs else {}
            sp = med(rs) / med(py) if py and rs else None
            mpy = max((r["peak_rss_mb"] for r in py), default=None)
            mrs = max((r["peak_rss_mb"] for r in rs), default=None)
            stats.append({"fam": fam, "kind": "mm", "case": f"{model} {sfx}", "model": model, "mod": mod, "speed": sp,
                          "mem": mrs / mpy if mpy and mrs else None, "dcos": p.get("dense_min_cos"), "lcos": p.get("late_min_cos")})
            lc = f"{p['late_min_cos']:.7f}" if "late_min_cos" in p else "n/a"
            out.append(f"| {sfx} | {mod} | {n} | {status('py', py)} | {status('rs', rs)} | **{f(sp, '{:.2f}x')}** | "
                       f"{f(mpy, '{:.0f}')} | {f(mrs, '{:.0f}')} | {f(p.get('dense_min_cos'), '{:.7f}')} | {lc} |")


def ops(root: Path, fam: str, out: list):
    files = sorted(glob.glob(str(root / "ops" / "ops-*.json")))
    if not files:
        return
    out.append(f"\n**Per-op time share** ({FAM[fam]}, 16 threads, batch 1): torch eager vs Rust runner (ORT + Rust kernels)\n")
    out.append("| case | tokens | runtime | total s | GEMM attn-proj/other | GEMM MLP | attention core | norms | RoPE | embed / patch | other (elementwise, framework) | pre/post-processing |")
    out.append("|---|---|---|---|---|---|---|---|---|---|---|---|")
    for p in files:
        r = json.load(open(p))
        for rt in ("torch", "rust"):
            x = r.get(rt)
            if not x or "error" in x:
                continue
            tot = x["total_s"]
            pc = lambda k: f"{100 * x.get(k, 0) / tot:.0f}%" if tot else "-"
            other = x.get("other", x.get("other_ort", 0))
            prep = r.get("prep_s", 0) if rt == "torch" else x.get("rust_prep_post", 0)
            out.append(f"| {r['case']} | {r['len']} | {rt} | {tot:.3f} | {pc('gemm_proj')} | {pc('gemm_mlp')} | {pc('attn_core')} | "
                       f"{pc('norm')} | {pc('rope')} | {pc('embed')} | {100 * other / tot:.0f}% | {prep * 1000:.0f} ms |")


def summary(stats):
    sp = [s for s in stats if s["speed"]]
    lose = [s for s in sp if s["speed"] < 1.0]
    heavy = [s for s in stats if s["mem"] and s["mem"] > 1.0]
    bad = [s for s in stats if (s["dcos"] is not None and s["dcos"] < 0.999) or (s["lcos"] is not None and s["lcos"] < 0.999)]
    head = [f"\n### Summary over {len(stats)} measured cases\n",
            f"- Rust / Python speed: min {min(s['speed'] for s in sp):.2f}x, median {np.median([s['speed'] for s in sp]):.2f}x, "
            f"max {max(s['speed'] for s in sp):.2f}x; Rust slower in {len(lose)} case(s)." if sp else "- no speed data",
            f"- Rust / Python peak RSS: median {np.median([s['mem'] for s in stats if s['mem']]):.2f}; Rust heavier in {len(heavy)} case(s)."
            if any(s["mem"] for s in stats) else "",
            f"- Parity: worst dense cosine {min((s['dcos'] for s in stats if s['dcos'] is not None), default=float('nan')):.7f}, "
            f"worst late cosine {min((s['lcos'] for s in stats if s['lcos'] is not None), default=float('nan')):.7f}; below 0.999 in {len(bad)} case(s)."]
    for name, lst in (("Rust slower", lose), ("Rust heavier", heavy), ("parity < 0.999", bad)):
        for s in lst:
            head.append(f"  - {name}: {s['fam']} {s['kind']} {s['case']} (speed {f(s['speed'])}x, RSS ratio {f(s['mem'])}, "
                        f"cos {f(s['dcos'], '{:.5f}')})")
    return head


def by_family_table(stats):
    """median / min speed and median RSS ratio per CPU family x benchmark kind."""
    out = ["| CPU | benchmark | cases | Rust / Python speed: median (min - max) | Rust slower in | peak RSS Rust / Python: median (max) |",
           "|---|---|---|---|---|---|"]
    kinds = {"grid": "text + image, thread x batch grid", "ctx": "context length 128 ... max", "mm": "modalities / file types / sizes"}
    for fam in ("rome", "milan", "turin"):
        for k, kn in kinds.items():
            xs = [s for s in stats if s["fam"] == fam and s["kind"] == k and s["speed"]]
            if not xs:
                continue
            sp = [s["speed"] for s in xs]
            mm_ = [s["mem"] for s in xs if s["mem"]]
            out.append(f"| {FAM[fam]} | {kn} | {len(xs)} | {np.median(sp):.2f}x ({min(sp):.2f} - {max(sp):.2f}) | "
                       f"{sum(v < 1 for v in sp)} | {np.median(mm_):.2f} ({max(mm_):.2f}) |")
    return out


def parity_table(stats):
    out = ["| model | modality | cases (all CPUs, configs, file types) | worst dense cosine | worst late cosine (NeoMME) |",
           "|---|---|---|---|---|"]
    g = defaultdict(list)
    for s in stats:
        if s.get("model") and s["dcos"] is not None:
            g[(s["model"], s["mod"])].append(s)
    for (m, mod), xs in sorted(g.items()):
        lc = [s["lcos"] for s in xs if s["lcos"] is not None]
        out.append(f"| {m} | {mod} | {len(xs)} | {min(s['dcos'] for s in xs):.7f} | {f(min(lc) if lc else None, '{:.7f}')} |")
    return out


def jobs_table(root: Path):
    out = ["| CPU | benchmark dir |", "|---|---|"]
    for fam in ("rome", "milan", "turin"):
        for d in sorted((root / fam).glob("*")) if (root / fam).exists() else []:
            if not d.is_dir():
                continue
            jobs = set()
            for p in glob.glob(str(d / "**" / "*.json"), recursive=True):
                if p.endswith(".ids.json") or "/lists/" in p:
                    continue
                try:
                    r = json.load(open(p))
                except Exception:
                    continue
                if isinstance(r, dict) and r.get("job"):
                    jobs.add(str(r["job"]))
            if jobs:
                out.append(f"| {FAM[fam]} | {d.name} |")  # scheduler job ids are deliberately not published
    return out


def main():
    doc = "--doc" in sys.argv
    root = Path([a for a in sys.argv[1:] if not a.startswith("--")][0])
    out, stats = [], []
    sec = {k: [] for k in ("tables", "ctx", "mm", "ops")}
    for fam in ("rome", "milan", "turin"):
        r = root / fam
        if not r.exists():
            continue
        out.append(f"\n### {FAM[fam]}\n")
        parts = {k: [] for k in sec}
        grid(r, fam, parts["tables"], stats)
        ctx(r, fam, parts["ctx"], stats)
        mm(r, fam, parts["mm"], stats)
        ops(r, fam, parts["ops"])
        for k, v in parts.items():
            out.extend(v)
            if v:
                sec[k] += [f"<details><summary><b>{FAM[fam]}</b></summary>\n", *v, "\n</details>\n"]
    head = summary(stats)
    json.dump(stats, open(root / "final_stats.json", "w"), indent=1)
    if doc:
        res = {k: "\n".join(v) for k, v in sec.items()}
        res["summary"] = "\n".join(head)
        res["by_family"] = "\n".join(by_family_table(stats))
        res["parity"] = "\n".join(parity_table(stats))
        res["jobs"] = "\n".join(jobs_table(root))
        json.dump(res, sys.stdout, indent=1)
        return
    print("\n".join(head + out))


if __name__ == "__main__":
    main()
