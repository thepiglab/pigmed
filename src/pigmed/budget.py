from __future__ import annotations
import contextlib
import fcntl
import json
from pathlib import Path
from .common import Backpressure, atomic_json


class Budget:
    """Cross-process outbox reservations, not a prediction of group filesystem quota.

    Uses POSIX flock on a tiny ledger lock, not SQLite/WAL on a parallel filesystem.
    Verify flock support on the actual shared filesystem before a multi-node run.
    Reservations survive crashes and are released only by cleanup/publish.
    """
    def __init__(self, root: Path, limit: int):
        self.root, self.limit = root / "budget", limit
        self.root.mkdir(parents=True, exist_ok=True)

    @contextlib.contextmanager
    def lock(self):
        with (self.root / "ledger.lock").open("a+") as f:
            fcntl.flock(f, fcntl.LOCK_EX)
            try:
                yield
            finally:
                fcntl.flock(f, fcntl.LOCK_UN)

    def reserve(self, key: str, size: int):
        with self.lock():
            record = self.root / (key + ".json")
            if record.exists():
                return
            used = sum(json.loads(p.read_text())["bytes"] for p in self.root.glob("*.json"))
            if used + size > self.limit:
                raise Backpressure(f"Outbox reserved {used / 2**30:.1f} GiB; cannot reserve {size / 2**30:.1f} GiB "
                                   f"within {self.limit / 2**30:.1f} GiB. Publish/drain and resume.")
            atomic_json(record, {"key": key, "bytes": size})

    def release(self, key: str):
        with self.lock():
            (self.root / (key + ".json")).unlink(missing_ok=True)
