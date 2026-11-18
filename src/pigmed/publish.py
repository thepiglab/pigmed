from __future__ import annotations
import hashlib
import json
import shutil
import time
from pathlib import Path
from .budget import Budget
from .common import atomic_json, digest_file, exclusive_file, read_jsonl
from .shards import verify_bundle


def _files(bundle: Path, remote_prefix: str, extra=()):
    """(local file, path in repo): the bundle's files, then extra files (e.g. its vector shards)."""
    out = [(p, f"{remote_prefix}/{p.name}") for p in sorted(bundle.iterdir()) if p.is_file()]
    return out + [(Path(p), rel) for p, rel in extra]


def vector_files(bundle: Path, vectors_root: Path, models, run_prefix: str):
    """Vector shard files for every required model, or None if any model has not finished this bundle."""
    extra = []
    for m in models:
        d = vectors_root / m / bundle.name
        if not (d / "meta.json").exists():
            return None
        if any("error" in v for v in json.loads((d / "meta.json").read_text()).get("modalities", {}).values()):
            return None  # a media modality still awaits repair
        extra += [(p, f"vectors/{m}/{run_prefix}/{bundle.name}/{p.name}") for p in sorted(d.iterdir())
                  if p.is_file() and not p.name.startswith("late")]  # late vectors stay local (eval sample)
    return extra


class LocalSink:
    """Offline integration-test sink with the same verify-before-delete contract."""
    def __init__(self, destination: Path):
        self.destination = destination

    def put(self, bundle: Path, remote_prefix: str, extra=()) -> str:
        for p, rel in _files(bundle, remote_prefix, extra):
            (self.destination / rel).parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(p, self.destination / rel)
        return "local:" + digest_file(bundle / "manifest.json")

    def verify(self, bundle: Path, remote_prefix: str, revision: str, extra=()):
        for p, rel in _files(bundle, remote_prefix, extra):
            if digest_file(p) != digest_file(self.destination / rel):
                raise ValueError("Local sink verification failed")


class HubSink:
    def __init__(self, repo_id: str, allow_public: bool = False, create: bool = False):
        from huggingface_hub import HfApi, get_token
        self.api, self.repo_id, self.token = HfApi(), repo_id, get_token()
        if not self.token:
            raise RuntimeError("Authenticate with `hf auth login` or HF_TOKEN; do not put credentials in scripts")
        if create:
            self.api.create_repo(repo_id, repo_type="dataset", private=True, exist_ok=True)
        info = self.api.repo_info(repo_id, repo_type="dataset")
        if not info.private and not allow_public:
            raise ValueError("Public repository requires --allow-public after a rights/storage review")

    def put(self, bundle: Path, remote_prefix: str, extra=()) -> str:
        from huggingface_hub import CommitOperationAdd
        ops = [CommitOperationAdd(path_in_repo=rel, path_or_fileobj=str(p)) for p, rel in _files(bundle, remote_prefix, extra)]
        result = self.api.create_commit(repo_id=self.repo_id, repo_type="dataset", operations=ops,
                                       commit_message=f"Add immutable shard {bundle.name}")
        return result.oid

    def verify(self, bundle: Path, remote_prefix: str, revision: str, extra=()):
        """Read at the committed revision and hash every byte, without a duplicate disk cache.

        This costs a round-trip download. It is deliberately conservative; an
        object-store checksum-aware verifier can replace it after validation.
        """
        import requests
        from huggingface_hub import hf_hub_url
        for p, rel in _files(bundle, remote_prefix, extra):
            url = hf_hub_url(self.repo_id, rel, repo_type="dataset", revision=revision)
            h, size = hashlib.sha256(), 0
            with requests.get(url, headers={"Authorization": f"Bearer {self.token}"}, stream=True,
                              timeout=(15, 180)) as r:
                r.raise_for_status()
                for data in r.iter_content(4 << 20):
                    h.update(data)
                    size += len(data)
            if size != p.stat().st_size or h.hexdigest() != digest_file(p):
                raise ValueError(f"Remote checksum verification failed: {p.name}")


def publish(root: Path, sink, delete: bool = False, allow_mock: bool = False,
            reviewed_pubmed_terms: bool = False, limit: int | None = None,
            vectors_root: Path | None = None, require_vectors=()):
    count = 0
    budget = Budget(root, 0)  # only releasing existing reservations
    with exclusive_file(root / "publisher.lock"):
        for manifest_file in sorted((root / "runs").glob("*/ready/*/manifest.json")):
            if limit is not None and count >= limit:
                break
            bundle = manifest_file.parent
            if bundle.name.endswith(".partial"):
                # The manifest is written before the commit rename; a live worker still owns it.
                continue
            manifest = verify_bundle(bundle)
            if not manifest["export_approved"] and reviewed_pubmed_terms:
                unapproved = (row for row in read_jsonl(bundle / "items.jsonl.gz") if not row.get("export_approved"))
                if any(row.get("kind") != "citation" or row.get("license") != "PUBMED-CITATION-TERMS" for row in unapproved):
                    raise ValueError("PubMed-terms override cannot authorize other restricted content")
            if not manifest["export_approved"] and not reviewed_pubmed_terms:
                raise ValueError("Shard contains records not cleared for export. Review PubMed terms separately; "
                                 "do not assume that private hosting grants additional rights.")
            if (manifest.get("embedding") or {}).get("backend") == "mock" and not allow_mock:
                raise ValueError("Refusing to publish mock embeddings")
            run_id = manifest["checkpoint"]["run_id"]
            prefix = f"data/{run_id[:2]}/{bundle.name}"
            extra = []
            if require_vectors:
                extra = vector_files(bundle, vectors_root, require_vectors, run_id[:2])
                if extra is None:
                    continue  # not embedded by every model yet; keep it local

            receipt = root / "receipts" / run_id / (bundle.name + ".json")
            local_hash = digest_file(manifest_file)
            if receipt.exists():
                old = json.loads(receipt.read_text())
                if old["manifest_sha256"] != local_hash:
                    raise ValueError("Receipt conflicts with local immutable bundle")
                # Re-verify the remote copy in case it was removed or changed since the receipt.
                sink.verify(bundle, prefix, old["commit"], extra)
            else:
                t0 = time.time()
                commit = sink.put(bundle, prefix, extra)
                t1 = time.time()
                sink.verify(bundle, prefix, commit, extra)
                t2 = time.time()
                atomic_json(receipt, {"commit": commit, "remote_prefix": prefix,
                                      "manifest_sha256": local_hash, "checkpoint": manifest["checkpoint"],
                                      "files": manifest["files"], "verified": True,
                                      "bytes": sum(p.stat().st_size for p in bundle.iterdir() if p.is_file()),
                                      "vector_files": [rel for _, rel in extra],
                                      "vector_bytes": sum(Path(p).stat().st_size for p, _ in extra),
                                      "committed_at": t1, "upload_seconds": t1 - t0, "verify_seconds": t2 - t1,
                                      "bundle_ready_at": manifest_file.stat().st_mtime})
            if delete:
                shutil.rmtree(bundle)
                budget.release(manifest["bundle"])
            count += 1
    return {"published_or_verified": count, "deleted_local": delete}
