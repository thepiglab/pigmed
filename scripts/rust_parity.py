"""Compare a Python-ingest root and a Rust-ingest root built from the same plan partition.

Usage: python -I scripts/rust_parity.py PY_ROOT RS_ROOT [--pixels N]
Reports unit-ID overlap per kind, field differences on shared units, and pixel deltas for a
sample of shared figure/page views. Text units are EXPECTED to differ where the Rust parser
fixes audited Python defects (figs/tables inside <p>, LaTeX preambles, glued words, back matter).
"""
import argparse, collections, glob, gzip, io, json, sys, tarfile


def load(root):
    rows, tars = {}, {}
    for items in glob.glob(f"{root}/runs/*/ready/*/items.jsonl.gz"):
        bundle = items.rsplit("/", 1)[0]
        for r in map(json.loads, gzip.open(items, "rt")):
            rows[r["unit_id"]] = r
            tars[r["unit_id"]] = bundle + "/assets.tar"
    return rows, tars


def pixels(tar_path, member):
    from PIL import Image
    import numpy as np
    with tarfile.open(tar_path) as t:
        return np.asarray(Image.open(io.BytesIO(t.extractfile(member).read())).convert("RGB"), dtype=np.int16)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("py"); ap.add_argument("rs"); ap.add_argument("--pixels", type=int, default=20)
    a = ap.parse_args()
    py, pyt = load(a.py)
    rs, rst = load(a.rs)
    kinds = sorted({r["kind"] for r in py.values()} | {r["kind"] for r in rs.values()})
    report = {"python_units": len(py), "rust_units": len(rs), "by_kind": {}}
    for k in kinds:
        p = {u for u, r in py.items() if r["kind"] == k}
        s = {u for u, r in rs.items() if r["kind"] == k}
        report["by_kind"][k] = {"python": len(p), "rust": len(s), "shared": len(p & s)}
    diffs = collections.Counter()
    ignore = {"asset_member", "embedding_status"}
    for u in py.keys() & rs.keys():
        for f in (py[u].keys() | rs[u].keys()) - ignore:
            if py[u].get(f) != rs[u].get(f):
                diffs[f"{py[u]['kind']}.{f}"] += 1
    report["field_diffs_on_shared_units"] = dict(diffs)
    deltas = []
    shared_media = [u for u in py.keys() & rs.keys() if py[u].get("asset_member") and rs[u].get("asset_member")]
    for u in shared_media[: a.pixels]:
        x, y = pixels(pyt[u], py[u]["asset_member"]), pixels(rst[u], rs[u]["asset_member"])
        if x.shape != y.shape:
            deltas.append({"unit": u, "kind": py[u]["kind"], "shape_py": list(x.shape), "shape_rs": list(y.shape)})
        else:
            deltas.append({"unit": u, "kind": py[u]["kind"], "mean_abs": float(abs(x - y).mean()), "max_abs": int(abs(x - y).max())})
    report["pixel_samples"] = deltas
    errs = [r for r in rs.values() if r.get("status") == "ingest-error"]
    report["rust_content_errors"] = [{"article": r["article_id"], "error": r["error"][:200]} for r in errs]
    # Text-quality probes on Rust vs Python text units.
    def glued(rows):
        t = [r["text"] for r in rows.values() if r["kind"] in ("text", "table_text")]
        return {"units": len(t), "latex_preamble": sum("\\documentclass" in x for x in t),
                "chars": sum(len(x) for x in t)}
    report["text_probe"] = {"python": glued(py), "rust": glued(rs)}
    json.dump(report, sys.stdout, indent=1)
    print()


if __name__ == "__main__":
    main()
