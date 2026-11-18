from __future__ import annotations
import heapq
import json
import os
import resource
import time
from pathlib import Path
import numpy as np
from .common import read_jsonl, stable_id, allowed_license
from .embeddings import normalized, mean_maxsim
from .evaluation import wilson
from .media import media_kind
from .net import Http, s3_url
from .shards import read_dense, read_late


def audit_media(manifests: list[Path], http: Http, max_articles: int = 10000):
    """Uniform hash-ranked sample, invariant to inventory order. Metadata-only audit.

    Caller must use a latest-version plan. Files in ZIP archives and video
    soundtracks cannot be inferred from filename metadata and remain unknown.
    """
    if max_articles < 1:
        raise ValueError("sample size must be positive")
    sample = []
    for manifest in manifests:
        for row in read_jsonl(manifest):
            priority = int(stable_id("media-audit-v1", row["pmcid"]), 16)
            entry = (-priority, row["key"], row)
            if len(sample) < max_articles:
                heapq.heappush(sample, entry)
            elif priority < -sample[0][0]:
                heapq.heapreplace(sample, entry)
    counts = {"articles": 0, "permissive_articles": 0, "video_articles": 0, "audio_articles": 0,
              "image_articles": 0, "video_files": 0, "audio_files": 0, "metadata_failures": 0}
    for _, _, row in sorted(sample):
        try:
            meta = http.json(s3_url(row["key"]))
        except Exception:
            counts["metadata_failures"] += 1
            continue
        kinds = [media_kind(u) for u in (meta.get("media_urls") or [])]
        counts["articles"] += 1
        counts["permissive_articles"] += allowed_license(meta.get("license_code", ""))
        for kind in ("video", "audio", "image"):
            counts[kind + "_articles"] += int(kind in kinds)
        counts["video_files"] += kinds.count("video")
        counts["audio_files"] += kinds.count("audio")
    return {**counts, "denominator": "successfully fetched latest-version PMC metadata records",
            "video_fraction": counts["video_articles"]/counts["articles"] if counts["articles"] else None,
            "video_fraction_95pct_wilson": wilson(counts["video_articles"], counts["articles"]),
            "audio_fraction_95pct_wilson": wilson(counts["audio_articles"], counts["articles"]),
            "limitations": ["Not all PubMed; only the input PMC inventory/plan", "ZIP contents not inspected",
                            "Video soundtracks not inspected", "Report metadata failures; they can bias prevalence"]}


def benchmark(encoder, units: list[dict], batch_size=1, warmup=2, repeats=3):
    if not units or batch_size < 1 or repeats < 1:
        raise ValueError("Nonempty units and positive batch/repeat counts required")
    batches = [units[i:i+batch_size] for i in range(0, len(units), batch_size)]
    import torch
    cuda = getattr(encoder, "device", "cpu").startswith("cuda")
    for i in range(warmup):
        encoder.encode(batches[i % len(batches)])
    if cuda:
        torch.cuda.synchronize()
        torch.cuda.reset_peak_memory_stats()
    elapsed, items = [], 0
    for _ in range(repeats):
        start = time.perf_counter()
        for batch in batches:
            encoder.encode(batch)
            items += len(batch)
        if cuda:
            torch.cuda.synchronize()
        elapsed.append(time.perf_counter() - start)
    return {"embedding": encoder.identity, "batch_size": batch_size, "examples": len(units),
            "repeats": repeats, "wall_seconds": elapsed, "items_per_second": items/sum(elapsed),
            "allocated_cpu_cores": int(os.getenv("SLURM_CPUS_PER_TASK", os.cpu_count() or 1)),
            "max_rss_kib_linux": resource.getrusage(resource.RUSAGE_SELF).ru_maxrss,
            "peak_cuda_allocated_bytes": torch.cuda.max_memory_allocated() if cuda else None,
            "includes_preprocessing": True, "includes_download_parse_upload": False}


def search(bundles: list[Path], encoder, query: dict, candidates=100, k=10, rerank=True):
    """Exact reference search for the pilot, NOT a scalable ANN service."""
    if candidates < 1 or k < 1:
        raise ValueError("candidates and k must be positive")
    q = encoder.encode([query], task="query")[0]
    identity = stable_id(encoder.identity)
    heap, sequence, scanned = [], 0, 0
    for bundle in bundles:
        manifest = json.loads((bundle / "manifest.json").read_text())
        if stable_id(manifest.get("embedding")) != identity:
            raise ValueError("Cannot compare embedding spaces/revisions; query with the exact indexed model configuration")
        for row in read_jsonl(bundle / "items.jsonl.gz"):
            if "dense" not in row or row.get("is_retracted"):
                continue
            vector = normalized(read_dense(bundle, row["dense"]))
            score = float(q.dense @ vector)
            item = (score, sequence, str(bundle), row)
            sequence += 1
            scanned += 1
            if len(heap) < candidates:
                heapq.heappush(heap, item)
            elif score > heap[0][0]:
                heapq.heapreplace(heap, item)
    result = []
    for score, _, path, row in heap:
        if rerank and q.late is not None and "late" in row:
            score = mean_maxsim(q.late, read_late(Path(path), row["late"]))
        result.append({"score": score, "bundle": path, **row})
    result.sort(key=lambda x: x["score"], reverse=True)
    return {"scanned": scanned, "candidates": len(heap), "hits": result[:k],
            "notice": "Exact pilot search. Deploy a separately provisioned ANN/lexical service at corpus scale."}
