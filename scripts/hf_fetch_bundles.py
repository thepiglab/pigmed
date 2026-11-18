"""Re-read published bundles from the Hugging Face dataset into a local ingest-style root (read-back test).

For every publish receipt under RECEIPTS (root/receipts/<run>/<bundle>.json) it downloads the bundle's manifest.json
and items.jsonl.gz at the receipt's commit (assets.tar is skipped: embedding needs only the rows; figures, video
and audio are fetched from their source URLs), checks both against the receipt's sha256s, and writes them to
OUT/runs/<run>/ready/<bundle>/. Already-fetched bundles are skipped, so it can be rerun as more bundles publish.

usage: python -I scripts/hf_fetch_bundles.py --repo OWNER/NAME --receipts ROOT/receipts --out DIR [--workers 8]
"""
import argparse
import concurrent.futures as cf
import hashlib
import json
import os
import shutil
from pathlib import Path


def sha(p: Path) -> str:
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for b in iter(lambda: f.read(1 << 20), b""):
            h.update(b)
    return h.hexdigest()


def fetch(repo, rec: Path, out: Path):
    from huggingface_hub import hf_hub_download
    r = json.loads(rec.read_text())
    run, bundle = rec.parent.name, rec.stem
    dst = out / "runs" / run / "ready" / bundle
    if (dst / "manifest.json").exists():
        return "skip"
    tmp = dst.with_name(bundle + ".partial")
    shutil.rmtree(tmp, ignore_errors=True)
    tmp.mkdir(parents=True)
    want = {f["path"]: f["sha256"] for f in r["files"]}
    for name in ("items.jsonl.gz", "manifest.json"):
        p = Path(hf_hub_download(repo, f"{r['remote_prefix']}/{name}", repo_type="dataset", revision=r["commit"],
                                 local_dir=tmp / ".dl"))
        got = sha(p)
        exp = r["manifest_sha256"] if name == "manifest.json" else want[name]
        if got != exp:
            raise ValueError(f"{bundle}/{name}: sha256 {got} != receipt {exp}")
        shutil.move(str(p), tmp / name)
    shutil.rmtree(tmp / ".dl", ignore_errors=True)
    os.replace(tmp, dst)
    return "ok"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", required=True)
    ap.add_argument("--receipts", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--workers", type=int, default=8)
    a = ap.parse_args()
    recs = sorted(a.receipts.glob("*/*.json"))
    res = {"ok": 0, "skip": 0, "failed": 0}
    with cf.ThreadPoolExecutor(a.workers) as ex:
        futs = {ex.submit(fetch, a.repo, r, a.out): r for r in recs}
        for f in cf.as_completed(futs):
            try:
                res[f.result()] += 1
            except Exception as e:
                res["failed"] += 1
                print(json.dumps({"receipt": str(futs[f]), "error": str(e)[-500:]}), flush=True)
    print(json.dumps({"receipts": len(recs), **res}))


if __name__ == "__main__":
    main()
