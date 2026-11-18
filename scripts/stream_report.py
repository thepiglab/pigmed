"""Summarise a progressive ingest->publish run from its publish receipts and producer log.

usage: python -I scripts/stream_report.py ROOT
"""
import json
import sys
from pathlib import Path

root = Path(sys.argv[1])
rec = [json.loads(p.read_text()) for p in sorted(root.glob("receipts/*/*.json"))]
rec.sort(key=lambda r: r.get("committed_at", 0))
log = (root / "producer.log").read_text().splitlines() if (root / "producer.log").exists() else []
total = sum(r.get("bytes", 0) for r in rec)
up = sum(r.get("upload_seconds", 0) for r in rec)
ver = sum(r.get("verify_seconds", 0) for r in rec)
lag = [r["committed_at"] - r["bundle_ready_at"] for r in rec if "bundle_ready_at" in r]
span = (rec[-1]["committed_at"] - min(r["bundle_ready_at"] for r in rec)) if rec else 0
left = list(root.glob("runs/*/ready/*/manifest.json"))
print(json.dumps({
    "bundles_published": len(rec), "bytes": total, "mean_bundle_MB": total / max(len(rec), 1) / 1e6,
    "upload_s": up, "verify_s": ver, "upload_MBps": total / max(up, 1e-9) / 1e6, "verify_MBps": total / max(ver, 1e-9) / 1e6,
    "ready_to_committed_lag_s": {"median": sorted(lag)[len(lag) // 2] if lag else None, "max": max(lag) if lag else None},
    "first_ready_to_last_commit_s": span,
    "backpressure_events": sum("BACKPRESSURE" in l for l in log),
    "producer_failures": [l for l in log if "PRODUCER_FAILED" in l],
    "bundles_left_local": len(left), "all_verified": all(r.get("verified") for r in rec)}, indent=2))
