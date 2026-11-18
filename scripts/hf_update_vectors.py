"""Re-upload changed per-bundle vector shards to the HF dataset (same paths the publisher used), then verify.

For each bundle name in LIST: uploads VECTORS/<model>/<bundle>/{dense.npy,ids.json,meta.json} to
vectors/<model>/<bundle[:2]>/<bundle>/ in batched commits, reads every file back at the new revision and checks its
sha256, and writes a receipt per batch under RECEIPTS.

usage: python -I scripts/hf_update_vectors.py --repo OWNER/NAME --vectors V --model gemma --list FILE --receipts DIR
"""
import argparse
import hashlib
import json
import time
from pathlib import Path

FILES = ("dense.npy", "ids.json", "meta.json")


def sha(p):
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for b in iter(lambda: f.read(1 << 20), b""):
            h.update(b)
    return h.hexdigest()


def main():
    from huggingface_hub import CommitOperationAdd, HfApi, get_token, hf_hub_url
    import requests
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", required=True)
    ap.add_argument("--vectors", type=Path, required=True)
    ap.add_argument("--model", required=True)
    ap.add_argument("--list", type=Path, required=True)
    ap.add_argument("--receipts", type=Path, required=True)
    ap.add_argument("--batch", type=int, default=60)
    ap.add_argument("--message", default="Update re-embedded vectors")
    a = ap.parse_args()
    api, tok = HfApi(), get_token()
    if not api.repo_info(a.repo, repo_type="dataset").private:
        raise SystemExit("refusing: repo is not private")
    names = [l.strip() for l in a.list.read_text().splitlines() if l.strip()]
    a.receipts.mkdir(parents=True, exist_ok=True)
    done = 0
    for k in range(0, len(names), a.batch):
        chunk = names[k:k + a.batch]
        ops, files = [], []
        for b in chunk:
            for f in FILES:
                p = a.vectors / a.model / b / f
                rel = f"vectors/{a.model}/{b[:2]}/{b}/{f}"
                ops.append(CommitOperationAdd(path_in_repo=rel, path_or_fileobj=str(p)))
                files.append((p, rel))
        t0 = time.time()
        c = api.create_commit(repo_id=a.repo, repo_type="dataset", operations=ops,
                              commit_message=f"{a.message} ({len(chunk)} bundles)")
        for p, rel in files:
            url = hf_hub_url(a.repo, rel, repo_type="dataset", revision=c.oid)
            h = hashlib.sha256()
            with requests.get(url, headers={"Authorization": f"Bearer {tok}"}, stream=True, timeout=(15, 180)) as r:
                r.raise_for_status()
                for d in r.iter_content(4 << 20):
                    h.update(d)
            if h.hexdigest() != sha(p):
                raise SystemExit(f"verify failed: {rel}")
        (a.receipts / f"batch-{k // a.batch:04d}.json").write_text(json.dumps(
            {"commit": c.oid, "bundles": chunk, "files": len(files), "verified": True, "seconds": time.time() - t0}))
        done += len(chunk)
        print(json.dumps({"batch": k // a.batch, "bundles": len(chunk), "commit": c.oid}), flush=True)
    print(json.dumps({"updated_bundles": done, "verified": True}))


if __name__ == "__main__":
    main()
