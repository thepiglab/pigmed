from __future__ import annotations
import argparse
import dataclasses
import glob
import importlib.metadata
import importlib.util
import json
import os
from pathlib import Path
import signal
import sys
import time
from .common import Backpressure, atomic_json, read_jsonl, write_jsonl
from .embeddings import MODELS, build_encoder
from .net import Http
from .pipeline import RunConfig, run


def expand(patterns):
    result = []
    for pattern in patterns:
        matches = glob.glob(pattern, recursive=True)
        result.extend(Path(p) for p in (matches or [pattern]))
    return sorted(set(result))


def model_args(parser):
    parser.add_argument("--backend", choices=["neomme", "gemma2", "mock"], required=True)
    parser.add_argument("--model-id")
    parser.add_argument("--revision")
    parser.add_argument("--device", default="cpu")
    parser.add_argument("--dimension", type=int)
    parser.add_argument("--modalities", choices=["text", "vision", "all"], default="vision")


def encoder_from(args):
    return build_encoder(args.backend, args.model_id, args.revision, args.device, args.dimension, args.modalities)


def parser():
    p = argparse.ArgumentParser(prog="pigmed", description="Bounded biomedical ingestion and retrieval research")
    sub = p.add_subparsers(dest="command", required=True)
    d = sub.add_parser("doctor")
    d.add_argument("--root", type=Path, default=Path("."))
    lock = sub.add_parser("lock-model")
    lock.add_argument("model_id")
    lock.add_argument("--out", type=Path, required=True)
    lock.add_argument("--download", action="store_true")
    for name in ("plan-pmc", "plan-pubmed"):
        q = sub.add_parser(name)
        q.add_argument("--out", type=Path, required=True)
        q.add_argument("--limit", type=int)
        q.add_argument("--contact", default="")
        if name == "plan-pmc":
            q.add_argument("--inventory-manifest")
            q.add_argument("--per-part", type=int, default=2000)
            q.add_argument("--sample-mod", type=int, default=1)
            q.add_argument("--sample-rem", type=int, default=0)
            q.add_argument("--versions", choices=["latest", "all"], default="latest")
        else:
            q.add_argument("--updates", action="store_true")
    ingest = sub.add_parser("ingest")
    ingest.add_argument("--manifest", type=Path, required=True)
    ingest.add_argument("--root", type=Path, required=True)
    ingest.add_argument("--config", type=Path)
    defaults = RunConfig()
    for field in dataclasses.fields(RunConfig):
        default = getattr(defaults, field.name)
        kwargs = {"default": None}
        if isinstance(default, bool):
            kwargs["action"] = argparse.BooleanOptionalAction
        else:
            kwargs["type"] = int if field.name == "dimension" else type(default) if default is not None else str
        ingest.add_argument("--" + field.name.replace("_", "-"), **kwargs)
    pub = sub.add_parser("publish")
    pub.add_argument("--root", type=Path, required=True)
    dest = pub.add_mutually_exclusive_group(required=True)
    dest.add_argument("--repo")
    dest.add_argument("--local-destination", type=Path)
    pub.add_argument("--create-private", action="store_true")
    pub.add_argument("--allow-public", action="store_true")
    pub.add_argument("--allow-mock", action="store_true")
    pub.add_argument("--reviewed-pubmed-terms", action="store_true")
    pub.add_argument("--delete-after-verify", action="store_true")
    pub.add_argument("--limit", type=int)
    pub.add_argument("--vectors-root", type=Path, help="embed_bundles.py output; with --require-vectors")
    pub.add_argument("--require-vectors", default="", help="comma-separated models; publish a bundle only after all "
                     "of them embedded it, uploading their dense vectors in the same commit")
    pub.add_argument("--watch-seconds", type=int, default=0)
    pub.add_argument("--poll-seconds", type=int, default=60)
    audit = sub.add_parser("audit-media")
    audit.add_argument("manifests", nargs="+")
    audit.add_argument("--sample-size", type=int, default=10000)
    audit.add_argument("--contact", default="")
    audit.add_argument("--out", type=Path, required=True)
    bench = sub.add_parser("benchmark")
    model_args(bench)
    bench.add_argument("--inputs", type=Path, required=True)
    bench.add_argument("--out", type=Path, required=True)
    bench.add_argument("--batch-size", type=int, default=1)
    bench.add_argument("--repeats", type=int, default=3)
    re = sub.add_parser("reembed")
    model_args(re)
    re.add_argument("--bundle", type=Path, required=True)
    re.add_argument("--root", type=Path, required=True)
    re.add_argument("--batch-size", type=int, default=1)
    re.add_argument("--rechunk", action=argparse.BooleanOptionalAction, default=True)
    re.add_argument("--chunk-tokens", type=int, default=512)
    re.add_argument("--overlap-tokens", type=int, default=64)
    se = sub.add_parser("search")
    model_args(se)
    se.add_argument("--bundles", nargs="+", required=True)
    se.add_argument("--text", default="")
    se.add_argument("--image", type=Path)
    se.add_argument("--allow-experimental-image-query", action="store_true")
    se.add_argument("--candidates", type=int, default=100)
    se.add_argument("--k", type=int, default=10)
    se.add_argument("--no-rerank", action="store_true")
    ev = sub.add_parser("evaluate")
    ev.add_argument("--qrels", type=Path, required=True, help="JSON: query ID -> {document ID: relevance}")
    ev.add_argument("--rankings", type=Path, required=True, help="JSON: query ID -> [ranked document IDs]")
    ev.add_argument("--k", type=int, default=10)
    split = sub.add_parser("split")
    split.add_argument("--input", type=Path, required=True)
    split.add_argument("--out", type=Path, required=True)
    split.add_argument("--seed", default="pigmed-v1")
    pq = sub.add_parser("to-parquet")
    pq.add_argument("--bundle", type=Path, required=True)
    pq.add_argument("--out", type=Path, required=True)
    verify = sub.add_parser("verify")
    verify.add_argument("bundle", type=Path)
    snap = sub.add_parser("materialize-citations")
    snap.add_argument("inputs", nargs="+")
    snap.add_argument("--database", type=Path, required=True)
    snap.add_argument("--out", type=Path, required=True)
    train = sub.add_parser("train-neomme")
    train.add_argument("--input", type=Path, required=True)
    train.add_argument("--out", type=Path, required=True)
    train.add_argument("--model-id", default=MODELS["neomme"])
    train.add_argument("--revision", required=True)
    train.add_argument("--device", default="cuda")
    train.add_argument("--batch-size", type=int, default=2)
    train.add_argument("--epochs", type=int, default=1)
    train.add_argument("--lr", type=float, default=2e-5)
    train.add_argument("--accumulation", type=int, default=8)
    train.add_argument("--max-steps", type=int, default=1000)
    train.add_argument("--blocklist", type=Path)
    return p


def execute(a):
    if a.command == "doctor":
        packages = {}
        for name in ("numpy", "requests", "lxml", "pillow", "torch", "transformers", "sentence-transformers", "pypdfium2", "huggingface_hub", "pyarrow"):
            try:
                packages[name] = importlib.metadata.version(name)
            except importlib.metadata.PackageNotFoundError:
                packages[name] = None
        supported = {}
        if importlib.util.find_spec("transformers"):
            import transformers
            supported["NeoMMEForRetrieval"] = hasattr(transformers, "NeoMMEForRetrieval")
            supported["NeoMMEProcessor"] = hasattr(transformers, "NeoMMEProcessor")
        import shutil
        return {"packages": packages, "model_interfaces": supported, "ffmpeg": shutil.which("ffmpeg"),
                "ffprobe": shutil.which("ffprobe"), "tmpdir": os.getenv("TMPDIR"),
                "filesystem_free_bytes_NOT_group_quota": shutil.disk_usage(a.root).free,
                "notice": "No model inference or multi-node filesystem lock test performed by doctor."}
    if a.command == "lock-model":
        from huggingface_hub import HfApi, snapshot_download
        info = HfApi().model_info(a.model_id)
        result = {"model_id": a.model_id, "revision": info.sha}
        if a.download:
            result["cache_path"] = snapshot_download(a.model_id, revision=info.sha)
        atomic_json(a.out, result)
        return result
    if a.command == "plan-pmc":
        from .sources import plan_pmc
        return plan_pmc(a.out, Http(a.contact), manifest_url=a.inventory_manifest, per_part=a.per_part,
                        sample_mod=a.sample_mod, sample_rem=a.sample_rem, limit=a.limit, versions=a.versions)
    if a.command == "plan-pubmed":
        from .sources import plan_pubmed
        return plan_pubmed(a.out, Http(a.contact), a.updates, a.limit)
    if a.command == "ingest":
        config = json.loads(a.config.read_text()) if a.config else {}
        for field in dataclasses.fields(RunConfig):
            value = getattr(a, field.name)
            if value is not None:
                config[field.name] = value
        return run(a.manifest, a.root, RunConfig(**config))
    if a.command == "publish":
        from .publish import HubSink, LocalSink, publish
        if a.poll_seconds < 10:
            raise ValueError("Use a modest publisher polling interval (at least 10 seconds)")
        sink = LocalSink(a.local_destination) if a.local_destination else HubSink(a.repo, a.allow_public, a.create_private)
        deadline = time.monotonic() + a.watch_seconds
        results = []
        while True:
            result = publish(a.root, sink, a.delete_after_verify, a.allow_mock, a.reviewed_pubmed_terms, a.limit,
                             a.vectors_root, [m for m in a.require_vectors.split(",") if m])
            results.append(result)
            if time.monotonic() >= deadline:
                return {"passes": len(results), "published_or_verified": sum(r["published_or_verified"] for r in results)}
            time.sleep(min(a.poll_seconds, max(0, deadline-time.monotonic())))
    if a.command == "audit-media":
        from .research import audit_media
        result = audit_media(expand(a.manifests), Http(a.contact), a.sample_size)
        atomic_json(a.out, result)
        return result
    if a.command == "benchmark":
        from .research import benchmark
        result = benchmark(encoder_from(a), list(read_jsonl(a.inputs)), a.batch_size, repeats=a.repeats)
        atomic_json(a.out, result)
        return result
    if a.command == "reembed":
        from .reembed import reembed
        return reembed(a.bundle, a.root, encoder_from(a), a.batch_size, a.rechunk, a.chunk_tokens, a.overlap_tokens)
    if a.command == "search":
        from .research import search
        if not a.text and not a.image:
            raise ValueError("Supply --text and/or --image")
        if a.image and a.backend == "neomme" and not a.allow_experimental_image_query:
            raise ValueError("NeoMME camera/image-query behavior needs separate validation; pass --allow-experimental-image-query for experiments")
        query = {"kind": "figure" if a.image else "text", "text": a.text, "media_path": str(a.image) if a.image else None}
        return search(expand(a.bundles), encoder_from(a), query, a.candidates, a.k, not a.no_rerank)
    if a.command == "evaluate":
        from .evaluation import evaluate
        return evaluate(json.loads(a.qrels.read_text()), json.loads(a.rankings.read_text()), a.k)
    if a.command == "split":
        from .evaluation import article_group, assign_split
        def rows():
            for row in read_jsonl(a.input):
                yield {**row, "group_id": article_group(row), "split": assign_split(row, a.seed)}
        write_jsonl(a.out, rows())
        return {"out": str(a.out), "notice": "Cluster duplicates/DOI aliases before this step; indexing still includes all splits."}
    if a.command == "to-parquet":
        from .shards import to_parquet
        to_parquet(a.bundle, a.out)
        return {"out": str(a.out)}
    if a.command == "verify":
        from .shards import verify_bundle
        return verify_bundle(a.bundle)
    if a.command == "materialize-citations":
        from .citations import materialize
        return materialize(expand(a.inputs), a.database, a.out)
    if a.command == "train-neomme":
        from .training import finetune_neomme
        return finetune_neomme(a.input, a.out, a.model_id, a.revision, a.device, a.batch_size,
                              a.epochs, a.lr, accumulation=a.accumulation, max_steps=a.max_steps, blocklist=a.blocklist)
    raise ValueError(a.command)


def main():
    def terminate(signum, frame):
        raise SystemExit(128 + signum)
    signal.signal(signal.SIGTERM, terminate)
    try:
        result = execute(parser().parse_args())
        print(json.dumps(result, ensure_ascii=False, indent=2))
    except Backpressure as e:
        print(str(e), file=sys.stderr)
        raise SystemExit(75)
    except Exception as e:
        print(f"{type(e).__name__}: {e}", file=sys.stderr)
        raise SystemExit(1)


if __name__ == "__main__":
    main()
