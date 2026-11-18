"""Real expert relevance judgments on full-text PMC articles, for chunking research (evaluation only, no redistribution).

Sets:
  cds14 / cds15  TREC Clinical Decision Support 2014 / 2015: case-report topics (<summary>), graded qrels by PMCID.
  covid          TREC-COVID round 5 (complete) qrels; CORD-19 2020-07-16 cord_uid -> PMCID via metadata.csv.
                 Two query forms: covid-q (<question>) and covid-kw (<query>).

  fetch  --out DIR      download topics/qrels/metadata into DIR/raw (skips files already there)
  build  --out DIR      DIR/queries.json [{qid, task, text}], DIR/qrels.json {qid: {"PMC<n>": grade}},
                        DIR/pmcids.txt (every judged PMCID), DIR/coverage.json
  plan   --out DIR --per-part 1300 [--concurrency 64]
                        latest version of each judged PMCID from the public pmc-oa-opendata bucket (one ListObjectsV2 per
                        PMCID under metadata/), written as pigmed-rs plan partitions DIR/plan/part-*.jsonl
"""
import argparse
import concurrent.futures as cf
import csv
import gzip
import io
import json
import re
import sys
import time
import urllib.parse
import urllib.request
from collections import defaultdict
from pathlib import Path

SRC = {
    "topics2014.xml": "https://www.trec-cds.org/topics2014.xml",
    "qrels-treceval-2014.txt": "https://trec.nist.gov/data/clinical/qrels-treceval-2014.txt",
    "topics-2015-A.xml": "https://trec.nist.gov/data/clinical/topics-2015-A.xml",
    "qrels-treceval-2015.txt": "https://trec.nist.gov/data/clinical/qrels-treceval-2015.txt",
    "topics-rnd5.xml": "https://ir.nist.gov/trec-covid/data/topics-rnd5.xml",
    "qrels-covid_d5_j0.5-5.txt": "https://ir.nist.gov/trec-covid/data/qrels-covid_d5_j0.5-5.txt",
    "cord19-2020-07-16-metadata.csv": "https://ai2-semanticscholar-cord-19.s3-us-west-2.amazonaws.com/2020-07-16/metadata.csv",
}
BUCKET = "https://pmc-oa-opendata.s3.amazonaws.com/"
UA = {"User-Agent": "pigmed-eval/0.1"}


def get(url, tries=4):
    for t in range(tries):
        try:
            with urllib.request.urlopen(urllib.request.Request(url, headers=UA), timeout=120) as r:
                return r.read()
        except Exception:
            if t == tries - 1:
                raise
            time.sleep(5 * (t + 1))


def cmd_fetch(a):
    raw = a.out / "raw"
    raw.mkdir(parents=True, exist_ok=True)
    for name, url in SRC.items():
        p = raw / name
        if p.exists() and p.stat().st_size > 0:
            continue
        data = get(url)
        p.write_bytes(data)
        print(name, len(data), flush=True)


def topics(xml, fields):
    out = {}
    for m in re.finditer(r"<topic\s+number=\"(\d+)\"[^>]*>(.*?)</topic>", xml, re.S):
        n, body = m.group(1), m.group(2)
        out[n] = {f: re.sub(r"\s+", " ", (re.search(fr"<{f}>(.*?)</{f}>", body, re.S) or [None, ""])[1]).strip() for f in fields}
    return out


def cmd_build(a):
    raw = a.out / "raw"
    qs, qrels = [], defaultdict(dict)
    for year, tf, qf in (("cds14", "topics2014.xml", "qrels-treceval-2014.txt"), ("cds15", "topics-2015-A.xml", "qrels-treceval-2015.txt")):
        t = topics((raw / tf).read_text(errors="replace"), ("summary", "description"))
        for n, v in t.items():
            qs.append({"qid": f"{year}-{n}", "task": year, "text": v["summary"] or v["description"]})
        for line in open(raw / qf):
            p = line.split()
            if len(p) >= 4:
                qrels[f"{year}-{p[0]}"][f"PMC{p[2].removeprefix('PMC')}"] = int(p[3])
    # TREC-COVID: cord_uid -> pmcid (CORD-19 2020-07-16 metadata.csv)
    cmap = {}
    with open(raw / "cord19-2020-07-16-metadata.csv", newline="", encoding="utf-8", errors="replace") as f:
        for r in csv.DictReader(f):
            pm = (r.get("pmcid") or "").strip()
            if pm.startswith("PMC"):
                cmap.setdefault(r["cord_uid"], pm)
    t = topics((raw / "topics-rnd5.xml").read_text(errors="replace"), ("query", "question", "narrative"))
    for n, v in t.items():
        qs.append({"qid": f"covid-q-{n}", "task": "covid-q", "text": v["question"]})
        qs.append({"qid": f"covid-kw-{n}", "task": "covid-kw", "text": v["query"]})
    cov = defaultdict(lambda: [0, 0, 0, 0])  # judged, judged with pmcid, relevant, relevant with pmcid
    for line in open(raw / "qrels-covid_d5_j0.5-5.txt"):
        p = line.split()
        if len(p) < 4:
            continue
        n, uid, g = p[0], p[2], int(p[3])
        c = cov["covid"]
        c[0] += 1
        c[2] += g > 0
        pm = cmap.get(uid)
        if pm:
            c[1] += 1
            c[3] += g > 0
            for form in ("q", "kw"):  # a cord doc judged twice in different rounds: keep the max grade
                k = f"covid-{form}-{n}"
                qrels[k][pm] = max(g, qrels[k].get(pm, 0))
    pmcids = sorted({d for q in qrels.values() for d in q})
    qs = [q for q in qs if q["qid"] in qrels and any(g > 0 for g in qrels[q["qid"]].values())]
    a.out.mkdir(parents=True, exist_ok=True)
    (a.out / "queries.json").write_text(json.dumps(qs, indent=0))
    (a.out / "qrels.json").write_text(json.dumps(qrels))
    (a.out / "pmcids.txt").write_text("\n".join(pmcids) + "\n")
    summ = {"queries": {k: sum(q["task"] == k for q in qs) for k in sorted({q["task"] for q in qs})}, "judged_pmcids": len(pmcids),
            "covid_judged_docs/with_pmcid/relevant/relevant_with_pmcid": cov["covid"],
            "relevant_per_set": {k: sum(1 for q, d in qrels.items() if q.startswith(k) for g in d.values() if g > 0) for k in ("cds14", "cds15", "covid-q")}}
    (a.out / "coverage.json").write_text(json.dumps(summ, indent=1))
    print(json.dumps(summ))


def latest(pmcid):
    """-> plan row for the newest version of PMCID under metadata/, or None if it is not in PMC OA now."""
    url = BUCKET + "?list-type=2&prefix=" + urllib.parse.quote(f"metadata/{pmcid}.")
    best = None
    for t in range(4):
        try:
            xml = get(url).decode()
            break
        except Exception:
            if t == 3:
                return {"pmcid": pmcid, "error": "list failed"}
            time.sleep(3)
    for m in re.finditer(r"<Contents>(.*?)</Contents>", xml, re.S):
        c = m.group(1)
        key = re.search(r"<Key>([^<]+)</Key>", c).group(1)
        km = re.match(r"metadata/(PMC\d+)\.(\d+)\.json$", key)
        if not km or km.group(1) != pmcid:
            continue
        v = int(km.group(2))
        if best is None or v > best["version"]:
            best = {"pmcid": pmcid, "version": v, "key": key,
                    "etag": re.search(r"<ETag>([^<]+)</ETag>", c).group(1).replace("&quot;", "").strip('"'),
                    "modified": re.search(r"<LastModified>([^<]+)</LastModified>", c).group(1), "source": "pmc",
                    "snapshot": "listobjects:" + time.strftime("%Y-%m-%d")}
    return best


def cmd_plan(a):
    ids = [x.strip() for x in open(a.out / "pmcids.txt") if x.strip()]
    rows, missing, err = [], 0, 0
    with cf.ThreadPoolExecutor(a.concurrency) as ex:
        for r in ex.map(latest, ids):
            if r is None:
                missing += 1
            elif "error" in r:
                err += 1
            else:
                rows.append(r)
    rows.sort(key=lambda r: r["pmcid"])
    p = a.out / "plan"
    p.mkdir(parents=True, exist_ok=True)
    for i in range(0, len(rows), a.per_part):
        with open(p / f"part-{i // a.per_part:05d}.jsonl", "w") as f:
            for r in rows[i:i + a.per_part]:
                f.write(json.dumps(r) + "\n")
    out = {"judged_pmcids": len(ids), "in_pmc_oa_now": len(rows), "not_in_oa": missing, "list_errors": err,
           "parts": (len(rows) + a.per_part - 1) // a.per_part}
    (p / "plan.json").write_text(json.dumps(out))
    print(json.dumps(out))


def main():
    ap = argparse.ArgumentParser()
    sp = ap.add_subparsers(dest="cmd", required=True)
    for n in ("fetch", "build", "plan"):
        s = sp.add_parser(n)
        s.add_argument("--out", type=Path, required=True)
        if n == "plan":
            s.add_argument("--per-part", type=int, default=1300)
            s.add_argument("--concurrency", type=int, default=64)
    a = ap.parse_args()
    {"fetch": cmd_fetch, "build": cmd_build, "plan": cmd_plan}[a.cmd](a)


if __name__ == "__main__":
    sys.exit(main())
