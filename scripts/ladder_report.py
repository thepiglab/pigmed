"""Summarise a pigmed ingest root: throughput, licences, unit kinds, media prevalence, errors, bytes.

Usage: python -I scripts/ladder_report.py RUN_ROOT [LOG_GLOB]
"""
import collections, glob, gzip, json, math, os, re, sys


def wilson(k, n, z=1.96):
    if n == 0:
        return [0.0, 0.0]
    p = k / n
    d = 1 + z * z / n
    c = p + z * z / (2 * n)
    a = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n))
    return [round((c - a) / d, 4), round((c + a) / d, 4)]


root = sys.argv[1]
kinds, lic, status, errors = collections.Counter(), collections.Counter(), collections.Counter(), collections.Counter()
media = collections.Counter()
articles = 0
for items in glob.glob(f"{root}/runs/*/ready/*/items.jsonl.gz"):
    for r in map(json.loads, gzip.open(items, "rt")):
        kinds[r["kind"]] += 1
        if r["kind"] == "article":
            articles += 1
            lic[r.get("license")] += 1
            mc = r.get("media_counts", {})
            for k in ("image", "video", "audio", "other"):
                media[k] += bool(mc.get(k))
        elif r["kind"] == "audit":
            status[r.get("status")] += 1
            lic[r.get("license")] += 1
            if r.get("status") == "ingest-error":
                errors[re.sub(r"[0-9a-f]{8,}|PMC\d+[.\d]*", "*", r.get("error", ""))[:120]] += 1
total_bytes = sum(os.path.getsize(p) for p in glob.glob(f"{root}/runs/*/ready/*/*"))
completes = [json.load(open(p)) for p in glob.glob(f"{root}/runs/*/complete.json")]
out = {
    "partitions_complete": len(completes),
    "records": sum(c.get("input_records", 0) for c in completes),
    "ingested_articles": articles,
    "license_skips": status["excluded-by-license-policy"],
    "content_errors": status["ingest-error"],
    "error_kinds": errors.most_common(8),
    "licenses_top": lic.most_common(12),
    "units_by_kind": dict(kinds),
    "units_per_article": {k: round(v / max(articles, 1), 2) for k, v in kinds.items() if k not in ("article", "audit")},
    "articles_with_media": {k: [v, wilson(v, articles)] for k, v in media.items()},
    "bundle_bytes": total_bytes,
    "bytes_per_ingested_article": round(total_bytes / max(articles, 1)),
    "mean_task_articles_per_second": round(sum(c.get("articles_per_second", 0) for c in completes) / max(len(completes), 1), 2),
}
if len(sys.argv) > 2:
    walls, rss = [], []
    for log in glob.glob(sys.argv[2]):
        m = re.search(r"wall=([\d.]+) s rss=(\d+) KB", open(log).read())
        if m:
            walls.append(float(m[1])); rss.append(int(m[2]))
    out["task_wall_seconds"] = {"n": len(walls), "max": max(walls, default=0), "mean": round(sum(walls) / max(len(walls), 1), 1)}
    out["task_peak_rss_mb"] = round(max(rss, default=0) / 1024)
print(json.dumps(out, indent=1))
