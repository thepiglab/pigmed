"""Build chunking-experiment corpora from ingested bundles (no re-download).

Each article's section streams are rebuilt exactly from the stored text units (their char offsets point into the
joined section text), then re-chunked per variant and written as small "bundles" that `embed_bundles.py --serve`
can embed (rows carry `embed_text`, the exact string the model sees).

  A   today's units, "title: none | text: <chunk>"                     (baseline; same as the production run)
  B   today's units, "title: <article title> | text: <section path>\\n<chunk>"
  C   balanced packing: consecutive sections under the same top-level heading are packed to ~TARGET words
      (no chunk under MIN words unless the whole article part is shorter; long sections split evenly with
      overlap), "title: none | text: <chunk>"
  CB  C's chunks with B's context prefix
  D   CB's chunks embedded by late chunking: one `late_doc` row per article (the whole prompted body text, up to
      the model's window) with one char span per chunk; the runner pools each span's token states

Only body text units (kind "text") are re-chunked; tables, captions, figures and supplements are left out of this
experiment (the production run already has them). Abstract units are kept and flagged `is_abstract`, so the eval
can exclude them from the corpus for the abstract-sentence task.

usage: python -I scripts/chunk_variants.py --src ROOT_WITH_BUNDLES --out DIR [--variants A,B,C,CB,D]
          [--target 384 --min 100 --max 512 --overlap 48] [--limit-bundles N]
"""
import argparse
import gzip
import hashlib
import json
import math
import os
import re
import sys
from collections import OrderedDict
from pathlib import Path

WS = re.compile(r"\S+")
SENT = re.compile(r"[.!?][\"')\]]?$")


def split_text(text, budget, overlap):
    """Exact port of rust/pigmed-ingest/src/jats.rs::split_text (word budget, sentence snap in the last quarter)."""
    toks = [(m.start(), m.end()) for m in WS.finditer(text)]
    out, i = [], 0
    while i < len(toks):
        end = min(i + budget, len(toks))
        if end < len(toks):
            lo = max(i + overlap + 1, i + budget * 3 // 4)
            for j in range(end, lo, -1):
                a, b = toks[j - 1]
                if SENT.search(text[a:b]):
                    end = j
                    break
        out.append((text[toks[i][0]:toks[end - 1][1]], toks[i][0], toks[end - 1][1], end - i))
        if end == len(toks):
            break
        i = max(i + 1, end - overlap)
    return out


def uid(*parts):
    return hashlib.sha256(json.dumps(parts, sort_keys=True).encode()).hexdigest()[:32]


def is_abstract(sec_id, path):
    p = " ".join(path or []).lower()
    return (sec_id or "").lower().startswith("abs") or "abstract" in p


def load_articles(bundle: Path):
    """-> list of {article_id, title, sections: [ {sid, path, text, abstract} ] } in document order."""
    arts = OrderedDict()
    secs = {}
    for line in gzip.open(bundle / "items.jsonl.gz", "rt"):
        r = json.loads(line)
        aid = r.get("article_id")
        if r.get("kind") == "article":
            arts.setdefault(aid, {"article_id": aid, "title": r.get("title") or "", "order": []})
            arts[aid]["title"] = r.get("title") or arts[aid]["title"]
        elif r.get("kind") == "text" and r.get("text"):
            loc = r.get("locator") or {}
            sid = loc.get("section_id") or ""
            a = arts.setdefault(aid, {"article_id": aid, "title": r.get("title") or "", "order": []})
            key = (aid, sid)
            if key not in secs:
                secs[key] = {"sid": sid, "path": loc.get("section") or [], "pieces": []}
                a["order"].append(key)
            secs[key]["pieces"].append((loc.get("char_start", 0), loc.get("char_end", 0), r["text"], r["unit_id"]))
    out = []
    for aid, a in arts.items():
        sections = []
        for key in a["order"]:
            s = secs[key]
            n = max(e for _, e, _, _ in s["pieces"])
            buf = [" "] * n
            for cs, ce, t, _ in s["pieces"]:
                if ce - cs == len(t):
                    buf[cs:ce] = list(t)
            text = "".join(buf).strip()
            sections.append({"sid": s["sid"], "path": s["path"], "text": text, "abstract": is_abstract(s["sid"], s["path"]),
                             "units": sorted(s["pieces"])})
        if sections:
            out.append({"article_id": aid, "title": a["title"], "sections": sections})
    return out


def ctx_prefix(title, path):
    t = re.sub(r"\s+", " ", title or "none").strip() or "none"
    p = " > ".join(x for x in (path or []) if x)
    return f"title: {t} | text: " + (f"{p}\n" if p else "")


def pack(sections, target, mn, mx, overlap):
    """Balanced packing. Yields (text, path, member section ids, abstract)."""
    groups, cur = [], []
    for s in sections:  # consecutive sections sharing the top-level heading (and abstract-ness) form a group
        top = (s["path"][0] if s["path"] else s["sid"], s["abstract"])
        if cur and cur[0][0] != top:
            groups.append(cur)
            cur = []
        cur.append((top, s))
    if cur:
        groups.append(cur)
    out = []
    for g in groups:
        secs = [s for _, s in g]
        common = secs[0]["path"][:1] if secs[0]["path"] else []
        # whole group as one stream with sub-headings inline, so packing can cross subsection boundaries
        parts = []
        for s in secs:
            sub = s["path"][-1] if len(s["path"]) > 1 else ""
            parts.append((f"{sub}. " if sub and not s["text"].startswith(sub) else "") + s["text"])
        stream = "\n\n".join(parts)
        words = len(WS.findall(stream))
        if words <= mx:
            out.append((stream, common or secs[0]["path"], [s["sid"] for s in secs], secs[0]["abstract"]))
            continue
        n = math.ceil(words / target)
        budget = min(mx, math.ceil(words / n) + overlap)
        pieces = split_text(stream, budget, overlap)
        if len(pieces) > 1 and len(WS.findall(pieces[-1][0])) - overlap < mn:  # fold a tiny tail into its neighbour
            prev = pieces[-2]
            merged = stream[prev[1]:pieces[-1][2]]
            pieces = pieces[:-2] + [(merged, prev[1], pieces[-1][2], len(WS.findall(merged)))]
        for t, _, _, _ in pieces:
            out.append((t, common or secs[0]["path"], [s["sid"] for s in secs], secs[0]["abstract"]))
    return out


def write_bundle(root: Path, name: str, rows):
    d = root / "runs" / "exp" / "ready" / name
    d.mkdir(parents=True, exist_ok=True)
    with gzip.open(d / "items.jsonl.gz", "wt") as f:
        for r in rows:
            f.write(json.dumps(r) + "\n")
    (d / "manifest.json").write_text(json.dumps({"bundle": name, "rows": len(rows), "experiment": True}))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--src", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--variants", default="A,B,C,CB,D")
    ap.add_argument("--target", type=int, default=384)
    ap.add_argument("--min", type=int, default=100)
    ap.add_argument("--max", type=int, default=512)
    ap.add_argument("--overlap", type=int, default=48)
    ap.add_argument("--limit-bundles", type=int, default=0)
    a = ap.parse_args()
    variants = a.variants.split(",")
    bundles = sorted(p.parent for p in a.src.glob("runs/*/ready/*/items.jsonl.gz"))
    if a.limit_bundles:
        bundles = bundles[: a.limit_bundles]
    stats = {v: {"units": 0, "words": [], "docs": 0} for v in variants}
    for bi, b in enumerate(bundles):
        arts = load_articles(b)
        rows = {v: [] for v in variants}
        for art in arts:
            aid, title = art["article_id"], art["title"]
            base = {"article_id": aid, "title": title, "source_type": "pmc"}
            rows_meta = []
            for s in art["sections"]:  # today's units (A, B)
                for cs, ce, t, u in s["units"]:
                    rows_meta.append((u, t, s["path"], s["abstract"], s["sid"]))
            if "A" in variants or "B" in variants:
                for u, t, path, ab, sid in rows_meta:
                    for v, et in (("A", "title: none | text: " + t), ("B", ctx_prefix(title, path) + t)):
                        if v in variants:
                            rows[v].append({**base, "unit_id": u, "kind": "text", "text": t, "embed_text": et,
                                            "is_abstract": ab, "locator": {"section_id": sid, "section": path}})
            packed = pack(art["sections"], a.target, a.min, a.max, a.overlap) if any(v in variants for v in ("C", "CB", "D")) else []
            doc_text, spans = "", []
            for k, (t, path, sids, ab) in enumerate(packed):
                u = uid(aid, "packed", k, t[:64], a.target, a.min, a.max, a.overlap)
                for v, et in (("C", "title: none | text: " + t), ("CB", ctx_prefix(title, path) + t)):
                    if v in variants:
                        rows[v].append({**base, "unit_id": u, "kind": "text", "text": t, "embed_text": et,
                                        "is_abstract": ab, "locator": {"section_ids": sids, "section": path}})
                if "D" in variants:
                    head = (" > ".join(path) + "\n") if path else ""
                    if doc_text:
                        doc_text += "\n\n"
                    doc_text += head
                    s0 = len(doc_text)
                    doc_text += t
                    spans.append({"id": u, "start": s0, "end": len(doc_text)})
                    rows["D"].append({**base, "unit_id": u, "kind": "text_meta", "text": t, "is_abstract": ab,
                                      "locator": {"section_ids": sids, "section": path}})
            if "D" in variants and spans:
                prefix = ctx_prefix(title, [])
                rows["D"].append({**base, "unit_id": uid(aid, "late_doc"), "kind": "late_doc", "text": prefix + doc_text,
                                  "prefix_chars": len(prefix),
                                  "spans": [{"id": s["id"], "start": s["start"] + len(prefix), "end": s["end"] + len(prefix)} for s in spans]})
        for v in variants:
            write_bundle(a.out / v / "root", b.name, rows[v])
            st = stats[v]
            for r in rows[v]:
                if r["kind"] in ("text", "text_meta"):
                    st["units"] += 1
                    st["words"].append(len(WS.findall(r["text"])))
                elif r["kind"] == "late_doc":
                    st["docs"] += 1
        if bi % 50 == 0:
            print(f"{bi + 1}/{len(bundles)} bundles", flush=True)
    import statistics
    summary = {}
    for v, st in stats.items():
        w = sorted(st["words"]) or [0]
        summary[v] = {"units": st["units"], "late_docs": st["docs"], "median_words": statistics.median(w),
                      "p10_words": w[len(w) // 10], "lt20": round(sum(x < 20 for x in w) / len(w), 4),
                      "lt100": round(sum(x < 100 for x in w) / len(w), 4)}
    (a.out / "variants.json").write_text(json.dumps({"args": vars(a) | {"src": str(a.src), "out": str(a.out)}, "summary": summary}, indent=1, default=str))
    print(json.dumps(summary, indent=1))


if __name__ == "__main__":
    sys.exit(main())
