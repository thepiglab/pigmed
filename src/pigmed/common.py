from __future__ import annotations
import contextlib
import gzip
import hashlib
import json
import os
import re
import time
from pathlib import Path
from typing import Iterable, Iterator, Any


def digest_bytes(data: bytes, algorithm: str = "sha256") -> str:
    return hashlib.new(algorithm, data).hexdigest()


def digest_file(path: Path, algorithm: str = "sha256") -> str:
    h = hashlib.new(algorithm)
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(4 * 1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def stable_id(*parts: Any) -> str:
    return digest_bytes(json.dumps(parts, sort_keys=True, ensure_ascii=False,
                                   separators=(",", ":")).encode())[:32]


def atomic_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp")
    with temporary.open("w", encoding="utf-8") as f:
        json.dump(value, f, ensure_ascii=False, sort_keys=True, indent=2)
        f.flush()
        os.fsync(f.fileno())
    os.replace(temporary, path)


def read_jsonl(path: Path) -> Iterator[dict]:
    opener = gzip.open if path.name.endswith(".gz") else open
    with opener(path, "rt", encoding="utf-8") as f:
        for line in f:
            if line.strip():
                yield json.loads(line)


def write_jsonl(path: Path, rows: Iterable[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    opener = gzip.open if path.name.endswith(".gz") else open
    with opener(path, "wt", encoding="utf-8") as f:
        for row in rows:
            f.write(json.dumps(row, ensure_ascii=False, separators=(",", ":")) + "\n")


def safe_member(name: str) -> str:
    """Tar/relative file names; never extract paths provided by an article."""
    p = Path(name)
    if p.is_absolute() or ".." in p.parts or "\\" in name or not name:
        raise ValueError(f"Unsafe relative name: {name!r}")
    return p.as_posix()


def tree_bytes(path: Path) -> int:
    return sum(p.stat().st_size for p in path.rglob("*") if p.is_file()) if path.exists() else 0


class Backpressure(RuntimeError):
    """Safe to retry after the publisher drains the outbox (exit code 75)."""


@contextlib.contextmanager
def exclusive_file(path: Path):
    """Single-writer lock held with flock(2) on `path`.

    A live holder blocks any other writer (on any node of a cluster filesystem mounted with flock). A holder that
    dies (kill -9, preemption, node loss) loses the lock with its file descriptor, so a restart is never blocked by a
    stale file and nothing needs manual cleanup. The file is left in place on release: unlinking a flock file races
    with a concurrent opener.
    """
    import fcntl
    path.parent.mkdir(parents=True, exist_ok=True)
    fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o664)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        holder = os.pread(fd, 4096, 0).decode(errors="replace")
        os.close(fd)
        raise RuntimeError(f"Lock held by a live process: {path} ({holder})") from None
    try:
        os.ftruncate(fd, 0)
        os.pwrite(fd, json.dumps({"pid": os.getpid(), "host": os.uname().nodename,
                                  "job": os.getenv("SLURM_JOB_ID"), "time": time.time()}).encode(), 0)
        yield
    finally:
        fcntl.flock(fd, fcntl.LOCK_UN)
        os.close(fd)


def normal_license(value: str | None) -> str:
    value = (value or "UNKNOWN").strip().upper().replace("_", "-")
    value = re.sub(r"\s+", "-", value)
    value = re.sub(r"-(?:[1-4](?:\.0)?)$", "", value)
    if value in {"CC-0", "CC0", "PUBLIC-DOMAIN"}:
        return "CC0"
    if value == "BY":
        return "CC-BY"
    return value


def allowed_license(value: str, allow: set[str] | None = None) -> bool:
    return normal_license(value) in (allow or {"CC0", "CC-BY"})
