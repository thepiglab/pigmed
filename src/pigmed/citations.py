from __future__ import annotations
import json
import sqlite3
from pathlib import Path
from .common import read_jsonl, write_jsonl


def materialize(inputs: list[Path], database: Path, output: Path):
    """Single-writer current-citation snapshot, including retained delete tombstones.

    Different baseline years must use different databases. Numeric source order
    makes replays idempotent even when Slurm tasks complete out of order.
    """
    database.parent.mkdir(parents=True, exist_ok=True)
    db = sqlite3.connect(database)
    db.execute("CREATE TABLE IF NOT EXISTS citations (pmid TEXT PRIMARY KEY, seq INTEGER, op TEXT, payload TEXT)")
    n = 0
    try:
        for path in inputs:
            for row in read_jsonl(path):
                if row.get("kind") != "citation":
                    continue
                sequence = (int(row["phase"] == "update") * 10**15 + int(row["source_sequence"]) * 10**8
                            + int(row["within_file"]))
                db.execute("INSERT INTO citations VALUES (?,?,?,?) ON CONFLICT(pmid) DO UPDATE SET "
                           "seq=excluded.seq,op=excluded.op,payload=excluded.payload WHERE excluded.seq>=citations.seq",
                           (row["pmid"], sequence, row["op"], json.dumps(row)))
                n += 1
                if n % 10000 == 0:
                    db.commit()
        db.commit()
        write_jsonl(output, (json.loads(x[0]) for x in db.execute("SELECT payload FROM citations WHERE op='upsert' ORDER BY pmid")))
        return {"events_read": n, "active_citations": db.execute("SELECT count(*) FROM citations WHERE op='upsert'").fetchone()[0],
                "tombstones": db.execute("SELECT count(*) FROM citations WHERE op='delete'").fetchone()[0]}
    finally:
        db.close()
