from __future__ import annotations
import json
import shutil
import tarfile
import tempfile
from pathlib import Path
from .budget import Budget
from .common import atomic_json, digest_file, exclusive_file, read_jsonl, safe_member, stable_id
from .documents import TokenCounter, text_units
from .shards import BundleWriter, verify_bundle


def reembed(bundle: Path, root: Path, encoder, batch_size=1, rechunk=True,
            chunk_tokens=512, overlap_tokens=64, reservation_bytes=4 << 30, outbox_limit=200 << 30):
    """Re-encode an archived bundle without re-downloading original PMC articles.

    Media views must have been archived. Missing media fail loudly, not silently
    become caption-only embeddings. Use the original model-agnostic ingest with
    --archive-media for a reusable comparison corpus.
    """
    old = verify_bundle(bundle)
    run_id = stable_id("reembed", digest_file(bundle / "manifest.json"), encoder.identity,
                       rechunk, chunk_tokens, overlap_tokens)
    worker = root / "runs" / run_id
    worker.mkdir(parents=True, exist_ok=True)
    ready = worker / "ready"
    ready.mkdir(exist_ok=True)
    budget = Budget(root, outbox_limit)
    name = run_id + "-000000000"
    with exclusive_file(worker / "worker.lock"):
        if (worker / "complete.json").exists():
            return {"run_id": run_id, "already_complete": True}
        # A ready bundle/verified receipt can survive a crash before complete.json.
        if (ready / name / "manifest.json").exists() or (root / "receipts" / run_id / (name + ".json")).exists():
            atomic_json(worker / "complete.json", {"run_id": run_id, "recovered": True})
            return {"run_id": run_id, "recovered": True}
        partial = ready / (name + ".partial")
        if partial.exists():
            shutil.rmtree(partial)
            budget.release(name)
        budget.reserve(name, reservation_bytes)
        writer = BundleWriter(ready, name, encoder.identity)
        counter = TokenCounter(encoder.tokenizer, encoder.name + "@" + encoder.revision) if encoder.tokenizer else TokenCounter()
        try:
            with tarfile.open(bundle / "assets.tar", "r") as source, tempfile.TemporaryDirectory(prefix="pigmed-reembed-") as tmp:
                tmp = Path(tmp)
                # Reuse asset bytes; do not extract untrusted TAR paths to the filesystem.
                for member in source:
                    safe_member(member.name)
                    if not member.isfile():
                        raise ValueError("Unexpected non-file archive member")
                    with source.extractfile(member) as f:
                        writer.tar.addfile(member, f)
                def rows():
                    for row in read_jsonl(bundle / "items.jsonl.gz"):
                        row = dict(row)
                        for key in ("dense", "late", "embedding_space", "embedding_status"):
                            row.pop(key, None)
                        row["parent_bundle_sha256"] = digest_file(bundle / "manifest.json")
                        if rechunk and row["kind"] == "article":
                            with source.extractfile(safe_member(row["canonical_member"])) as f:
                                article = json.load(f)
                            yield row
                            base = {k: row[k] for k in ("article_id", "pmcid", "pmid", "doi", "version", "title", "license", "is_retracted", "export_approved") if k in row}
                            for unit in text_units(article, counter, chunk_tokens, overlap_tokens):
                                yield {**base, **unit}
                        elif rechunk and row["kind"] in {"text", "caption", "table_text"}:
                            continue
                        else:
                            yield row
                batch = []
                def flush():
                    active = [r for r in batch if r["kind"] in encoder.supported]
                    paths = []
                    for row in active:
                        if row["kind"] in {"figure", "page", "video", "audio"}:
                            member = row.get("asset_member")
                            if not member:
                                raise ValueError("Media bytes not archived; re-ingest or explicitly fetch the checksum-verified source")
                            member = safe_member(member)
                            path = tmp / (stable_id(member) + Path(member).suffix)
                            with source.extractfile(member) as f, path.open("wb") as dst:
                                shutil.copyfileobj(f, dst)
                            row["media_path"] = str(path)
                            paths.append(path)
                    encoded = encoder.encode(active) if active else []
                    by_id = {r["unit_id"]: e for r, e in zip(active, encoded, strict=True)}
                    for row in batch:
                        writer.add(row, by_id.get(row["unit_id"]))
                    for path in paths:
                        path.unlink(missing_ok=True)
                    batch.clear()
                    if writer.size() > reservation_bytes:
                        raise ValueError("Re-embedding exceeded reserved output bytes")
                for row in rows():
                    batch.append(row)
                    if len(batch) >= batch_size:
                        flush()
                if batch:
                    flush()
            checkpoint = {"run_id": run_id, "manifest_sha256": digest_file(bundle / "manifest.json"),
                          "first_index": 0, "next_index": 1, "bundle": name}
            result = writer.finish(checkpoint)
            atomic_json(worker / "complete.json", {"run_id": run_id, "source": str(bundle)})
            return {"bundle": str(result), "run_id": run_id}
        except BaseException:
            writer.abort()
            if not writer.final.exists():
                budget.release(name)
            raise
