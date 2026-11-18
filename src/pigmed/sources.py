from __future__ import annotations
import csv
import gzip
import json
import re
import sqlite3
import tempfile
from pathlib import Path
from urllib.parse import unquote, urljoin
from .common import atomic_json, stable_id, write_jsonl
from .net import Http, s3_url

INVENTORY = "inventory-reports/pmc-oa-opendata/metadata/"
KEY_RE = re.compile(r"^metadata/(PMC\d+)\.(\d+)\.json$")


def latest_inventory(http: Http) -> str:
    prefixes = [row["prefix"] for row in http.list_s3(INVENTORY, "/")
                if "prefix" in row and re.search(r"/\d{4}-\d\d-\d\dT[^/]+/$", row["prefix"])]
    if not prefixes:
        raise RuntimeError("No PMC inventory manifest found; check current PMC README")
    # A new day's directory can exist before a complete manifest is published.
    for prefix in sorted(prefixes, reverse=True):
        try:
            http.json(s3_url(prefix + "manifest.json"))
            return s3_url(prefix + "manifest.json")
        except Exception as exc:
            import requests
            if not isinstance(exc, requests.HTTPError) or exc.response.status_code != 404:
                raise
    raise RuntimeError("No completed inventory found")


def inventory_rows(http: Http, manifest_url: str, scratch: Path):
    manifest = http.json(manifest_url)
    fields = [re.sub(r"[^a-z]", "", x.lower()) for x in manifest["fileSchema"].split(",")]
    required = {"bucket", "key", "etag", "lastmodifieddate"}
    if not required.issubset(fields):
        raise ValueError(f"Unexpected inventory schema: {manifest['fileSchema']}")
    for i, part in enumerate(manifest["files"]):
        path = scratch / f"inventory-{i}.csv.gz"
        http.download(s3_url(part["key"]), path, max_bytes=8 << 30,
                      md5=part.get("MD5checksum") or part.get("md5Checksum"))
        try:
            with gzip.open(path, "rt", encoding="utf-8", newline="") as f:
                for values in csv.reader(f):
                    row = dict(zip(fields, values, strict=True))
                    if row["bucket"] != "pmc-oa-opendata":
                        raise ValueError("Unexpected inventory bucket")
                    key = unquote(row["key"])
                    m = KEY_RE.match(key)
                    if m:
                        yield {"pmcid": m[1], "version": int(m[2]), "key": key,
                               "etag": row["etag"].strip('"'), "modified": row["lastmodifieddate"]}
        finally:
            path.unlink(missing_ok=True)


def plan_pmc(out: Path, http: Http, *, manifest_url: str | None = None,
             per_part: int = 2000, sample_mod: int = 1, sample_rem: int = 0,
             limit: int | None = None, versions: str = "latest") -> dict:
    if per_part < 1 or sample_mod < 1 or not 0 <= sample_rem < sample_mod:
        raise ValueError("Invalid partition/sample setting")
    if out.exists():
        raise FileExistsError(f"Plan directories are immutable: {out}")
    out.parent.mkdir(parents=True, exist_ok=True)
    temporary = out.with_name(out.name + ".partial")
    temporary.mkdir()
    manifest_url = manifest_url or latest_inventory(http)
    count = scanned = 0
    try:
        with tempfile.TemporaryDirectory(prefix="pigmed-inventory-") as tmp:
            scratch = Path(tmp)
            db = sqlite3.connect(scratch / "latest.sqlite")  # node-local, ONE writer
            db.execute("CREATE TABLE inventory (id TEXT PRIMARY KEY, pmcid TEXT, version INTEGER, payload TEXT)")
            for row in inventory_rows(http, manifest_url, scratch):
                scanned += 1
                if int(stable_id(row["pmcid"]), 16) % sample_mod != sample_rem:
                    continue
                key = row["pmcid"] if versions == "latest" else row["key"]
                db.execute("INSERT INTO inventory VALUES (?,?,?,?) ON CONFLICT(id) DO UPDATE SET "
                           "version=excluded.version,payload=excluded.payload WHERE excluded.version>=inventory.version",
                           (key, row["pmcid"], row["version"], json.dumps(row)))
                if scanned % 10000 == 0:
                    db.commit()
            db.commit()
            rows = db.execute("SELECT payload FROM inventory ORDER BY pmcid,version")
            batch, n_parts = [], 0
            for (payload,) in rows:
                if limit is not None and count >= limit:
                    break
                row = json.loads(payload)
                row["source"] = "pmc"
                row["snapshot"] = manifest_url
                batch.append(row)
                count += 1
                if len(batch) == per_part:
                    write_jsonl(temporary / f"part-{n_parts:05d}.jsonl", batch)
                    n_parts += 1
                    batch = []
            if batch:
                write_jsonl(temporary / f"part-{n_parts:05d}.jsonl", batch)
                n_parts += 1
            db.close()
        result = {"source": "pmc", "manifest_url": manifest_url, "rows": count, "parts": n_parts,
                  "scanned_versions": scanned, "versions": versions, "sample_mod": sample_mod,
                  "sample_rem": sample_rem, "limit": limit}
        atomic_json(temporary / "plan.json", result)
        temporary.rename(out)
        return result
    except BaseException:
        # An incomplete plan is intentionally not usable by workers.
        raise


def pubmed_files(http: Http, updates: bool = False):
    base = "https://ftp.ncbi.nlm.nih.gov/pubmed/" + ("updatefiles/" if updates else "baseline/")
    html = http.get(base).decode("utf-8")
    names = sorted(set(re.findall(r'href=["\'](pubmed\d+n\d+\.xml\.gz)["\']', html)))
    if not names:
        raise RuntimeError("No PubMed XML.gz files found; directory format may have changed")
    for name in names:
        sequence = int(re.search(r"n(\d+)\.xml", name)[1])
        yield {"source": "pubmed", "url": urljoin(base, name), "md5_url": urljoin(base, name + ".md5"),
               "sequence": sequence, "phase": "update" if updates else "baseline", "name": name}


def plan_pubmed(out: Path, http: Http, updates: bool = False, limit: int | None = None):
    if out.exists():
        raise FileExistsError(out)
    out.mkdir(parents=True)
    n = 0
    for row in pubmed_files(http, updates):
        if limit is not None and n >= limit:
            break
        write_jsonl(out / f"part-{n:05d}.jsonl", [row])
        n += 1
    atomic_json(out / "plan.json", {"source": "pubmed", "parts": n,
                                   "phase": "update" if updates else "baseline"})
    return {"parts": n}
