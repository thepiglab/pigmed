from __future__ import annotations
import dataclasses
import json
import os
import re
import shutil
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import urlsplit
from .budget import Budget
from .common import allowed_license, atomic_json, digest_file, exclusive_file, read_jsonl, stable_id, tree_bytes
from .documents import TokenCounter, parse_jats, resolve_asset, text_units, iter_pubmed
from .embeddings import build_encoder
from .media import image_view, media_kind, render_pdf, probe_media, time_windows, make_clip
from .net import Http, s3_url
from .shards import BundleWriter


@dataclass
class RunConfig:
    backend: str = "none"
    model_id: str | None = None
    revision: str | None = None
    device: str = "cpu"
    dimension: int | None = None
    modalities: str = "vision"
    batch_size: int = 1
    chunk_tokens: int = 512
    overlap_tokens: int = 64
    tokenizer_id: str | None = None
    tokenizer_revision: str | None = None
    render_pages: bool = False
    figures: bool = True
    image_caption: bool = False
    video: bool = False
    audio: bool = False
    archive_media: bool = True
    longest_side: int = 2048
    max_pages: int = 200
    video_seconds: float = 20
    audio_seconds: float = 30
    media_overlap: float = 2
    max_asset_bytes: int = 128 << 20
    max_article_bytes: int = 2 << 30
    target_shard_bytes: int = 256 << 20
    articles_per_shard: int = 100
    outbox_limit_bytes: int = 200 << 30
    contact: str = ""
    # "redistributable" = CC0/CC-BY only (v0.1); "all" = ingest every licence, filter downstream on row licence.
    license_policy: str = "redistributable"

    def validate(self):
        if self.batch_size < 1 or self.articles_per_shard < 1:
            raise ValueError("batch_size and articles_per_shard must be positive")
        if not 0 <= self.overlap_tokens < self.chunk_tokens:
            raise ValueError("Invalid text overlap")
        if self.license_policy not in {"redistributable", "all"}:
            raise ValueError("license_policy must be 'redistributable' or 'all'")
        if self.target_shard_bytes + self.max_article_bytes >= self.outbox_limit_bytes:
            raise ValueError("Outbox must hold at least one full shard reservation")
        if self.max_asset_bytes > self.max_article_bytes:
            raise ValueError("An asset cannot exceed the article workspace budget")
        if self.video and self.video_seconds > 32:
            raise ValueError("v0.1 video windows limited to 32 seconds to avoid default frame-budget truncation")


def make_views(metadata: dict, scratch: Path, config: RunConfig, counter: TokenCounter, http: Http):
    xml_path = scratch / "article.xml"
    xml_info = http.download(metadata["xml_url"], xml_path, config.max_asset_bytes)
    article = parse_jats(xml_path, metadata)
    article["xml_sha256"] = xml_info["sha256"]
    article["media_audit"] = []
    urls = metadata.get("media_urls") or []
    if not all(isinstance(u, str) for u in urls):
        raise ValueError("PMC media_urls schema changed; refusing to guess")
    article["media_inventory"] = [{"url": u, "kind": media_kind(u)} for u in urls]
    units = list(text_units(article, counter, config.chunk_tokens, config.overlap_tokens))
    media_files = {}

    def workspace_guard():
        if tree_bytes(scratch) > config.max_article_bytes:
            raise ValueError("Article workspace budget exceeded; adjust limits explicitly or quarantine this article")

    if config.figures:
        for fig in article["figures"]:
            if fig["rights_review_required"]:
                article["media_audit"].append({"figure": fig["figure_id"], "status": "rights-review-required"})
                continue
            for href in fig["hrefs"]:
                url = resolve_asset(href, urls)
                if not url or media_kind(url) != "image":
                    article["media_audit"].append({"figure": fig["figure_id"], "href": href, "status": "unresolved-or-unsupported"})
                    continue
                key = stable_id(url)
                original = scratch / (key + Path(urlsplit(url).path).suffix)
                info = http.download(url, original, config.max_asset_bytes)
                view = scratch / (key + ".png")
                transform = image_view(original, view, config.longest_side)
                media_files[key] = view
                units.append({"unit_id": stable_id(article["article_id"], "figure", fig["figure_id"], key, transform),
                              "kind": "figure", "text": fig["caption"] if config.image_caption else "",
                              "caption": fig["caption"], "media_path": str(view), "media_key": key,
                              "source_url": url, "source_sha256": info["sha256"],
                              "locator": {"figure_id": fig["figure_id"], "href": href}, "transform": transform})
                workspace_guard()
    if config.render_pages and metadata.get("pdf_url"):
        pdf = scratch / "article.pdf"
        info = http.download(metadata["pdf_url"], pdf, config.max_asset_bytes)
        for view, locator in render_pdf(pdf, scratch / "pages", config.longest_side, config.max_pages):
            key = stable_id(info["sha256"], locator)
            media_files[key] = view
            units.append({"unit_id": stable_id(article["article_id"], "page", key), "kind": "page", "text": "",
                          "media_path": str(view), "media_key": key, "source_url": metadata["pdf_url"],
                          "source_sha256": info["sha256"], "locator": locator})
            workspace_guard()
    for url in urls:
        kind = media_kind(url)
        if not ((kind == "video" and (config.video or config.audio)) or (kind == "audio" and config.audio)):
            continue
        original = scratch / (stable_id(url) + Path(urlsplit(url).path).suffix)
        info = http.download(url, original, config.max_asset_bytes)
        probe = probe_media(original)
        article["media_audit"].append({"url": url, **probe})
        modes = (["video"] if kind == "video" and config.video else []) + (["audio"] if config.audio and probe["has_audio"] else [])
        for mode in modes:
            seconds = config.video_seconds if mode == "video" else config.audio_seconds
            for start, end in time_windows(probe["duration"], seconds, config.media_overlap):
                key = stable_id(url, mode, start, end)
                view = scratch / (key + (".mp4" if mode == "video" else ".wav"))
                make_clip(original, view, start, end, mode)
                media_files[key] = view
                units.append({"unit_id": stable_id(article["article_id"], mode, key), "kind": mode, "text": "",
                              "media_path": str(view), "media_key": key, "source_url": url,
                              "source_sha256": info["sha256"], "locator": {"start_seconds": start, "end_seconds": end},
                              "has_soundtrack": probe["has_audio"], "audio_included": mode == "audio"})
                workspace_guard()
    return article, units, media_files, xml_path


def recover_cursor(worker: Path, root: Path, run_id: str, manifest_hash: str):
    candidates = []
    state = worker / "state.json"
    if state.exists():
        candidates.append(json.loads(state.read_text()))
    for path in (worker / "ready").glob("*/manifest.json"):
        candidates.append(json.loads(path.read_text())["checkpoint"])
    for path in (root / "receipts" / run_id).glob("*.json"):
        candidates.append(json.loads(path.read_text())["checkpoint"])
    candidates = [c for c in candidates if c.get("manifest_sha256") == manifest_hash and c.get("run_id") == run_id]
    return max((c["next_index"] for c in candidates), default=0)


def run(manifest_path: Path, root: Path, config: RunConfig, http: Http | None = None):
    config.validate()
    root.mkdir(parents=True, exist_ok=True)
    config_dict = dataclasses.asdict(config)
    # Device/batch size can change without changing semantic representation identity.
    semantic = {k: v for k, v in config_dict.items() if k not in {"device", "batch_size", "contact", "outbox_limit_bytes"}}
    manifest_hash = digest_file(manifest_path)
    run_id = stable_id(manifest_hash, semantic)
    worker = root / "runs" / run_id
    worker.mkdir(parents=True, exist_ok=True)
    ready = worker / "ready"
    ready.mkdir(exist_ok=True)
    budget = Budget(root, config.outbox_limit_bytes)
    http = http or Http(config.contact)
    with exclusive_file(worker / "worker.lock"):
        for partial in ready.glob("*.partial"):
            # Lock ensures no live worker shares this run; unfinished source units will be replayed.
            budget.release(partial.name.removesuffix(".partial"))
            shutil.rmtree(partial)
        cursor = recover_cursor(worker, root, run_id, manifest_hash)
        encoder = build_encoder(config.backend, config.model_id, config.revision, config.device, config.dimension, config.modalities)
        if config.tokenizer_id:
            from transformers import AutoTokenizer
            if not config.tokenizer_revision:
                raise ValueError("Pin --tokenizer-revision alongside --tokenizer-id")
            tokenizer = AutoTokenizer.from_pretrained(config.tokenizer_id, revision=config.tokenizer_revision, use_fast=True)
            counter = TokenCounter(tokenizer, config.tokenizer_id + "@" + config.tokenizer_revision)
        elif encoder is not None and encoder.tokenizer is not None:
            counter = TokenCounter(encoder.tokenizer, encoder.name + "@" + encoder.revision)
        else:
            counter = TokenCounter()
        identity = encoder.identity if encoder else None
        atomic_json(worker / "run.json", {"run_id": run_id, "config": config_dict, "embedding": identity,
                                          "manifest_sha256": manifest_hash, "tokenizer": counter.name})
        writer = None
        start_cursor = cursor
        processed = 0
        start_time = time.monotonic()
        source_rows = list(read_jsonl(manifest_path))
        if source_rows and source_rows[0]["source"] == "pubmed":
            return run_pubmed(source_rows, worker, root, run_id, manifest_hash, cursor, config, budget, http)

        def finish(next_index):
            nonlocal writer, start_cursor
            if writer is None:
                return
            checkpoint = {"run_id": run_id, "manifest_sha256": manifest_hash, "next_index": next_index,
                          "first_index": start_cursor, "bundle": writer.name}
            writer.finish(checkpoint)
            atomic_json(worker / "state.json", checkpoint)
            writer = None
            start_cursor = next_index

        try:
            for index, source in enumerate(source_rows):
                if index < cursor:
                    continue
                if writer is None:
                    name = f"{run_id}-{index:09d}"
                    budget.reserve(name, config.target_shard_bytes + config.max_article_bytes)
                    writer = BundleWriter(ready, name, identity)
                with tempfile.TemporaryDirectory(prefix="pigmed-article-") as temp:
                    scratch = Path(temp)
                    meta_path = scratch / "metadata.json"
                    etag = source.get("etag", "")
                    http.download(s3_url(source["key"]), meta_path, 16 << 20,
                                  md5=etag if re.fullmatch(r"[0-9a-fA-F]{32}", etag) else None)
                    metadata = json.loads(meta_path.read_text())
                    actual_pmcid = str(metadata.get("pmcid", ""))
                    if not actual_pmcid.startswith("PMC"):
                        actual_pmcid = "PMC" + actual_pmcid
                    if actual_pmcid != source["pmcid"] or int(metadata.get("version", 1)) != int(source["version"]):
                        raise ValueError("Metadata identity does not match immutable inventory plan")
                    redistributable = allowed_license(metadata.get("license_code", "UNKNOWN"))
                    if config.license_policy == "redistributable" and not redistributable:
                        # No full text/media fetched. This operational audit is not published as content.
                        writer.add({"unit_id": stable_id(source, "license-skip"), "kind": "audit",
                                    "article_id": f"{source['pmcid']}.{source['version']}",
                                    "status": "excluded-by-license-policy", "license": metadata.get("license_code"),
                                    "export_approved": True, "contains_article_content": False})
                    else:
                        article, units, media_files, xml = make_views(metadata, scratch, config, counter, http)
                        base = {k: article[k] for k in ("article_id", "pmcid", "pmid", "doi", "version", "title", "license", "is_retracted")}
                        base.update({"license_code": metadata.get("license_code"), "redistributable": redistributable,
                                     "export_approved": True, "source_etag": etag, "snapshot": source.get("snapshot"),
                                     "xml_sha256": article["xml_sha256"]})
                        canonical_name = writer.add_bytes(f"{article['article_id']}/article.json", json.dumps(article, ensure_ascii=False).encode())
                        xml_member = writer.add_file(f"{article['article_id']}/source.xml", xml)
                        writer.add({**base, "unit_id": stable_id(article["article_id"], article["xml_sha256"], "canonical"),
                                    "kind": "article", "canonical_member": canonical_name, "xml_member": xml_member,
                                    "media_counts": {k: sum(x["kind"] == k for x in article["media_inventory"])
                                                     for k in ("image", "video", "audio", "other")}})
                        for start in range(0, len(units), config.batch_size):
                            batch = units[start:start + config.batch_size]
                            active = [u for u in batch if encoder and u["kind"] in encoder.supported]
                            for u in active:
                                u["title"] = article["title"]
                            encoded = encoder.encode(active) if active else []
                            vectors = {u["unit_id"]: e for u, e in zip(active, encoded, strict=True)}
                            for unit in batch:
                                row = {**base, **unit}
                                if unit.get("media_key") and config.archive_media:
                                    p = media_files[unit["media_key"]]
                                    row["asset_member"] = writer.add_file(f"{article['article_id']}/media/{unit['media_key']}{p.suffix}", p)
                                if encoder and unit["kind"] not in encoder.supported:
                                    row["embedding_status"] = "unsupported-modality-retained-for-other-backends"
                                writer.add(row, vectors.get(unit["unit_id"]))
                    # Temp source files are removed after their durable bundle representation has been written.
                    # A crash before bundle commit re-fetches this article from checksum-addressed sources.
                processed += 1
                current_size = writer.size()
                if current_size > config.target_shard_bytes + config.max_article_bytes:
                    raise ValueError("Bundle reservation exceeded; increase max_article_bytes or reduce views")
                if index + 1 - start_cursor >= config.articles_per_shard or current_size >= config.target_shard_bytes:
                    finish(index + 1)
            finish(len(source_rows))
            result = {"complete": True, "processed_this_run": processed, "input_records": len(source_rows),
                      "elapsed_seconds": time.monotonic() - start_time, "run_id": run_id}
            atomic_json(worker / "complete.json", result)
            return result
        except BaseException:
            if writer is not None:
                key = writer.name
                writer.abort()
                if not writer.final.exists():
                    budget.release(key)
            raise


def run_pubmed(sources, worker, root, run_id, manifest_hash, cursor, config, budget, http):
    if config.backend != "none":
        raise ValueError("Citation ingest is model-agnostic; abstract encoding is a separate view, not full text")
    # One input baseline/update archive per plan partition, so restart offsets are unambiguous.
    if len(sources) != 1:
        raise ValueError("PubMed plans require one source archive per partition")
    source = sources[0]
    md5text = http.get(source["md5_url"]).decode()
    match = re.search(r"\b[0-9a-fA-F]{32}\b", md5text)
    if not match:
        raise ValueError("Cannot parse official PubMed MD5 file")
    writer, count, first = None, 0, cursor
    try:
        with tempfile.TemporaryDirectory(prefix="pigmed-pubmed-") as tmp:
            path = Path(tmp) / source["name"]
            http.download(source["url"], path, 2 << 30, md5=match[0])
            for index, row in enumerate(iter_pubmed(path, source)):
                count = index + 1
                if index < cursor:
                    continue
                if writer is None:
                    name = f"{run_id}-{index:09d}"
                    budget.reserve(name, config.target_shard_bytes + config.max_article_bytes)
                    writer = BundleWriter(worker / "ready", name)
                writer.add({**row, "unit_id": stable_id(source["name"], index, row["pmid"]), "kind": "citation"})
                if count - first >= 10000 or writer.size() >= config.target_shard_bytes:
                    checkpoint = {"run_id": run_id, "manifest_sha256": manifest_hash,
                                  "next_index": count, "first_index": first, "bundle": writer.name}
                    writer.finish(checkpoint)
                    atomic_json(worker / "state.json", checkpoint)
                    writer, first = None, count
            if writer is not None:
                checkpoint = {"run_id": run_id, "manifest_sha256": manifest_hash,
                              "next_index": count, "first_index": first, "bundle": writer.name}
                writer.finish(checkpoint)
                atomic_json(worker / "state.json", checkpoint)
                writer = None
        result = {"complete": True, "source": source["name"], "records": count,
                  "export_policy": "PubMed abstract/citation redistribution requires explicit review", "run_id": run_id}
        atomic_json(worker / "complete.json", result)
        return result
    except BaseException:
        if writer:
            key = writer.name
            writer.abort()
            if not writer.final.exists():
                budget.release(key)
        raise
