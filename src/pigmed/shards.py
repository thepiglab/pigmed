from __future__ import annotations
import gzip
import io
import json
import os
import shutil
import tarfile
from pathlib import Path
import numpy as np
from .common import atomic_json, digest_file, safe_member, stable_id, read_jsonl
from .embeddings import Encoded, quantize_int8


class BundleWriter:
    """One process owns one immutable bundle. A manifest is the commit marker.

    JSONL.gz is directly streamable by HF Datasets. Packed vector sidecars avoid
    JSON's 5-10x inflation. Optional Parquet conversion is a separate operation.
    """
    def __init__(self, out: Path, name: str, identity: dict | None = None):
        self.out, self.name = out, name
        self.partial = out / (name + ".partial")
        self.final = out / name
        if self.partial.exists() or self.final.exists():
            raise FileExistsError(f"Bundle already exists: {name}")
        self.partial.mkdir(parents=True)
        self.rows = gzip.open(self.partial / "items.jsonl.gz", "wt", encoding="utf-8")
        self.tar = tarfile.open(self.partial / "assets.tar", "w")
        self.dense = (self.partial / "dense.f16").open("wb")
        self.late = (self.partial / "late.i8").open("wb")
        self.scales = (self.partial / "late-scales.f32").open("wb")
        self.identity, self.count, self.asset_names = identity, 0, set()
        self.all_approved, self.closed = True, False

    def add_bytes(self, member: str, data: bytes):
        member = safe_member(member)
        if member in self.asset_names:
            return member
        info = tarfile.TarInfo(member)
        info.size, info.mtime, info.mode = len(data), 0, 0o644
        self.tar.addfile(info, io.BytesIO(data))
        self.asset_names.add(member)
        return member

    def add_file(self, member: str, path: Path):
        member = safe_member(member)
        if member not in self.asset_names:
            info = tarfile.TarInfo(member)
            info.size, info.mtime, info.mode = path.stat().st_size, 0, 0o644
            with path.open("rb") as f:
                self.tar.addfile(info, f)
            self.asset_names.add(member)
        return member

    def add(self, row: dict, embedding: Encoded | None = None):
        row = dict(row)
        row.pop("media_path", None)  # ephemeral worker paths must never become dataset references
        if embedding is not None:
            dense = np.asarray(embedding.dense, dtype="<f2")
            if not np.isfinite(dense).all():
                raise ValueError("Cannot serialize non-finite vector")
            row["dense"] = {"offset_bytes": self.dense.tell(), "dimensions": dense.size, "dtype": "<f2"}
            self.dense.write(dense.tobytes())
            if embedding.late is not None:
                codes, scales = quantize_int8(embedding.late)
                row["late"] = {"offset_bytes": self.late.tell(), "scale_offset_bytes": self.scales.tell(),
                               "vectors": len(codes), "dimensions": codes.shape[1],
                               "dtype": "int8", "scale_dtype": "<f4", "pool_factor": 1}
                self.late.write(codes.tobytes())
                self.scales.write(scales.tobytes())
            row["embedding_space"] = stable_id(self.identity)
        self.all_approved &= bool(row.get("export_approved", False))
        self.rows.write(json.dumps(row, ensure_ascii=False, separators=(",", ":")) + "\n")
        self.count += 1

    def size(self):
        self.rows.flush()
        for f in (self.dense, self.late, self.scales):
            f.flush()
        return sum(p.stat().st_size for p in self.partial.iterdir() if p.is_file())

    def finish(self, checkpoint: dict):
        if self.closed:
            raise RuntimeError("Already finalized")
        self.rows.close()
        self.tar.close()
        for name in ("items.jsonl.gz", "assets.tar"):
            # gzip/tarfile close() flushes but does not fsync; a node crash could otherwise leave a
            # valid manifest over truncated files.
            with (self.partial / name).open("rb+") as f:
                os.fsync(f.fileno())
        for f in (self.dense, self.late, self.scales):
            f.flush()
            os.fsync(f.fileno())
            f.close()
        files = []
        for p in sorted(self.partial.iterdir()):
            if p.stat().st_size == 0:
                p.unlink()
            else:
                files.append({"path": p.name, "bytes": p.stat().st_size, "sha256": digest_file(p)})
        manifest = {"schema_version": 1, "bundle": self.name, "rows": self.count,
                    "embedding": self.identity, "export_approved": self.all_approved,
                    "checkpoint": checkpoint, "files": files,
                    "late_compression": "per-vector symmetric int8 + float32 scales; no pooling"}
        atomic_json(self.partial / "manifest.json", manifest)
        os.replace(self.partial, self.final)
        dir_fd = os.open(self.out, os.O_RDONLY)
        try:
            os.fsync(dir_fd)  # make the commit rename durable
        finally:
            os.close(dir_fd)
        self.closed = True
        return self.final

    def abort(self):
        if not self.closed:
            for f in (self.rows, self.tar, self.dense, self.late, self.scales):
                f.close()
            self.closed = True
        shutil.rmtree(self.partial, ignore_errors=True)


def verify_bundle(path: Path) -> dict:
    manifest = json.loads((path / "manifest.json").read_text())
    for item in manifest["files"]:
        p = path / safe_member(item["path"])
        if not p.is_file() or p.stat().st_size != item["bytes"] or digest_file(p) != item["sha256"]:
            raise ValueError(f"Bundle checksum/size mismatch: {p}")
    return manifest


def read_dense(bundle: Path, info: dict):
    with (bundle / "dense.f16").open("rb") as f:
        f.seek(info["offset_bytes"])
        return np.frombuffer(f.read(info["dimensions"] * 2), dtype="<f2").astype(np.float32)


def read_late(bundle: Path, info: dict):
    with (bundle / "late.i8").open("rb") as f, (bundle / "late-scales.f32").open("rb") as sf:
        f.seek(info["offset_bytes"])
        sf.seek(info["scale_offset_bytes"])
        codes = np.frombuffer(f.read(info["vectors"] * info["dimensions"]), dtype=np.int8).reshape(info["vectors"], info["dimensions"])
        scales = np.frombuffer(sf.read(info["vectors"] * 4), dtype="<f4")
        return codes.astype(np.float32) * scales[:, None]


def to_parquet(bundle: Path, out: Path, batch_size: int = 1000):
    """Streaming stable-schema metadata export; packed vectors stay in sidecars."""
    import pyarrow as pa
    import pyarrow.parquet as pq
    out.parent.mkdir(parents=True, exist_ok=True)
    schema = pa.schema([("unit_id", pa.string()), ("article_id", pa.string()),
                        ("kind", pa.string()), ("text", pa.string()), ("metadata_json", pa.string())])
    writer = pq.ParquetWriter(out, schema, compression="zstd")
    batch = []
    try:
        for row in read_jsonl(bundle / "items.jsonl.gz"):
            batch.append({"unit_id": row.get("unit_id"), "article_id": row.get("article_id"),
                          "kind": row.get("kind"), "text": row.get("text"), "metadata_json": json.dumps(row)})
            if len(batch) >= batch_size:
                writer.write_table(pa.Table.from_pylist(batch, schema=schema))
                batch = []
        if batch:
            writer.write_table(pa.Table.from_pylist(batch, schema=schema))
    finally:
        writer.close()
