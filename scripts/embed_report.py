"""Summarise an embed_bundles.py run: items, items/s per worker and modality, aggregate throughput, skips.

usage: python -I scripts/embed_report.py VECTORS [--cpus-per-worker 16]
"""
import argparse
import json
from pathlib import Path

ap = argparse.ArgumentParser()
ap.add_argument("vectors", type=Path)
ap.add_argument("--cpus-per-worker", type=int, default=16)
a = ap.parse_args()
out = {}
for mdir in sorted(p for p in a.vectors.iterdir() if p.is_dir() and p.name not in ("units",)):
    metas = [json.loads(m.read_text()) for m in mdir.glob("*/meta.json")]
    if not metas:
        continue
    mods, skipped = {}, {}
    for m in metas:
        for k, v in m["modalities"].items():
            d = mods.setdefault(k, {"items": 0, "seconds": 0.0, "errors": 0})
            d["items"] += v.get("items", 0)
            d["seconds"] += v.get("seconds", 0.0)
            d["errors"] += "error" in v
        for k, v in m["skipped_media"].items():
            skipped[k] = skipped.get(k, 0) + v
    for d in mods.values():
        d["items_per_s_per_worker"] = round(d["items"] / max(d["seconds"], 1e-9), 3)
        d["items_per_cpu_hour"] = round(d["items"] / max(d["seconds"], 1e-9) * 3600 / a.cpus_per_worker, 1)
    t0 = min(m["finished_at"] - m["seconds_total"] for m in metas)
    t1 = max(m["finished_at"] for m in metas)
    worker_s = sum(m["seconds_total"] for m in metas)
    items = sum(d["items"] for d in mods.values())
    out[mdir.name] = {"bundles": len(metas), "items": items, "wall_s": round(t1 - t0), "worker_hours": round(worker_s / 3600, 2),
                      "aggregate_items_per_s": round(items / max(t1 - t0, 1e-9), 2),
                      "fetch_s_share": round(sum(m["seconds_inputs"] for m in metas) / max(worker_s, 1e-9), 3),
                      "hosts": len({m["host"] for m in metas}), "late_bundles": sum(m["late"] for m in metas),
                      "modalities": mods, "skipped_media": skipped,
                      "failed_bundles": len(list(mdir.glob("*.failed")))}
print(json.dumps(out, indent=2))
