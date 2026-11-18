"""Hugging Face upload/streaming benchmark for choosing the bundle (commit) size.

Sub-commands (all need an authenticated hub client; the repo must be private unless --allow-public):
  sweep        upload bundle-shaped commits of several sizes (random bytes: no Xet dedup, so this is
               raw throughput), verify each by full readback, and time tiny commits for per-commit overhead.
  stream-read  stream published `items.jsonl.gz` rows back with `datasets` (time to first row, rows/s).

Writes one JSON result to --out. Nothing here deletes remote data.
"""
from __future__ import annotations
import argparse
import hashlib
import json
import os
import tempfile
import time
from pathlib import Path


def api_for(repo: str, create: bool, allow_public: bool):
    from huggingface_hub import HfApi
    api = HfApi()
    if create:
        api.create_repo(repo, repo_type="dataset", private=True, exist_ok=True)
    if not api.repo_info(repo, repo_type="dataset").private and not allow_public:
        raise SystemExit("refusing: repository is public")
    return api


def write_random(path: Path, size: int):
    with path.open("wb") as f:
        left = size
        while left:
            n = min(left, 64 << 20)
            f.write(os.urandom(n))
            left -= n


def readback(api, repo: str, path_in_repo: str, revision: str, expect_sha: str, expect_size: int) -> float:
    import requests
    from huggingface_hub import get_token, hf_hub_url
    url = hf_hub_url(repo, path_in_repo, repo_type="dataset", revision=revision)
    t = time.perf_counter()
    h, size = hashlib.sha256(), 0
    with requests.get(url, headers={"Authorization": f"Bearer {get_token()}"}, stream=True, timeout=(15, 600)) as r:
        r.raise_for_status()
        for chunk in r.iter_content(8 << 20):
            h.update(chunk)
            size += len(chunk)
    if size != expect_size or h.hexdigest() != expect_sha:
        raise RuntimeError(f"readback mismatch for {path_in_repo}")
    return time.perf_counter() - t


def sha(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for b in iter(lambda: f.read(8 << 20), b""):
            h.update(b)
    return h.hexdigest()


def sweep(a):
    from huggingface_hub import CommitOperationAdd
    api = api_for(a.repo, a.create_private, a.allow_public)
    tag = time.strftime("%Y%m%dT%H%M%S")
    results = []
    scratch = Path(a.scratch or tempfile.mkdtemp(prefix="pigmed-hfbench-"))
    # Per-commit overhead: tiny bundle-shaped commits (3 files, ~1 MiB).
    for i in range(a.tiny_commits):
        d = scratch / f"tiny{i}"
        d.mkdir(parents=True, exist_ok=True)
        write_random(d / "assets.tar", 1 << 20)
        (d / "manifest.json").write_text(json.dumps({"i": i, "tag": tag}))
        write_random(d / "items.jsonl.gz", 64 << 10)
        ops = [CommitOperationAdd(path_in_repo=f"bench/{tag}/tiny/{i}/{p.name}", path_or_fileobj=str(p)) for p in sorted(d.iterdir())]
        t = time.perf_counter()
        api.create_commit(repo_id=a.repo, repo_type="dataset", operations=ops, commit_message=f"bench tiny {i}")
        results.append({"kind": "tiny", "bytes": sum(p.stat().st_size for p in d.iterdir()), "upload_s": time.perf_counter() - t})
        for p in d.iterdir():
            p.unlink()
    for mib in a.sizes:
        for r in range(a.repeats if mib < a.single_above_mib else 1):
            d = scratch / f"s{mib}-{r}"
            d.mkdir(parents=True, exist_ok=True)
            total = mib << 20
            write_random(d / "items.jsonl.gz", total // 10)
            write_random(d / "assets.tar", total - total // 10)
            (d / "manifest.json").write_text(json.dumps({"mib": mib, "r": r, "tag": tag}))
            files = sorted(d.iterdir())
            digests = {p.name: (sha(p), p.stat().st_size) for p in files}
            prefix = f"bench/{tag}/size-{mib:05d}MiB/r{r}"
            ops = [CommitOperationAdd(path_in_repo=f"{prefix}/{p.name}", path_or_fileobj=str(p)) for p in files]
            t = time.perf_counter()
            commit = api.create_commit(repo_id=a.repo, repo_type="dataset", operations=ops, commit_message=f"bench {mib} MiB r{r}")
            up = time.perf_counter() - t
            ver = sum(readback(api, a.repo, f"{prefix}/{n}", commit.oid, s, z) for n, (s, z) in digests.items())
            row = {"kind": "sweep", "mib": mib, "repeat": r, "bytes": total, "upload_s": up, "verify_s": ver,
                   "upload_MBps": total / up / 1e6, "verify_MBps": total / ver / 1e6,
                   "end_to_end_MBps": total / (up + ver) / 1e6}
            print(json.dumps(row), flush=True)
            results.append(row)
            for p in files:
                p.unlink()
    tiny = [x["upload_s"] for x in results if x["kind"] == "tiny"]
    out = {"repo": a.repo, "tag": tag, "rows": results,
           "tiny_commit_median_s": sorted(tiny)[len(tiny) // 2] if tiny else None}
    Path(a.out).write_text(json.dumps(out, indent=2))
    print(json.dumps({k: v for k, v in out.items() if k != "rows"}))


def stream_read(a):
    from datasets import load_dataset
    t0 = time.perf_counter()
    ds = load_dataset("json", data_files=f"hf://datasets/{a.repo}/{a.glob}", split="train", streaming=True)
    first, n, kinds = None, 0, {}
    for row in ds:
        if first is None:
            first = time.perf_counter() - t0
        n += 1
        kinds[row.get("kind")] = kinds.get(row.get("kind"), 0) + 1
        if n >= a.rows:
            break
    el = time.perf_counter() - t0
    out = {"repo": a.repo, "glob": a.glob, "rows": n, "first_row_s": first, "elapsed_s": el,
           "rows_per_s": n / max(el - (first or 0), 1e-9), "kinds": kinds}
    Path(a.out).write_text(json.dumps(out, indent=2))
    print(json.dumps(out))


def main():
    p = argparse.ArgumentParser()
    sub = p.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("sweep")
    s.add_argument("--repo", required=True)
    s.add_argument("--sizes", type=int, nargs="+", default=[8, 32, 128, 512, 2048])
    s.add_argument("--repeats", type=int, default=2)
    s.add_argument("--single-above-mib", type=int, default=1024)
    s.add_argument("--tiny-commits", type=int, default=8)
    s.add_argument("--scratch")
    s.add_argument("--create-private", action="store_true")
    s.add_argument("--allow-public", action="store_true")
    s.add_argument("--out", required=True)
    r = sub.add_parser("stream-read")
    r.add_argument("--repo", required=True)
    r.add_argument("--glob", default="data/**/items.jsonl.gz")
    r.add_argument("--rows", type=int, default=50000)
    r.add_argument("--out", required=True)
    a = p.parse_args()
    sweep(a) if a.cmd == "sweep" else stream_read(a)


if __name__ == "__main__":
    main()
