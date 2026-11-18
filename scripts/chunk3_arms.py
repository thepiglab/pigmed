"""Chunking v3 research arms, built from a root of per-section text units (A form: pigmed-rs ingest output) or from a
v2 chunk root. Research prototypes in Python (they need numpy over sentence vectors); a winner gets a Rust port.

Groups are exactly balanced-c's: consecutive sections sharing the top-level heading (and abstract flag), joined into
one stream with sub-headings inline (scripts/chunk_variants.py::pack). All arms keep v2's size bounds (min/max words)
and model input ("title: none | text: <top heading>\\n<chunk>") unless the arm changes exactly that.

  sentences --src A_ROOT --out ROOT        sentence windows (sentence i with i-1 and i+1) for an EG2 pass
  semantic  --src A_ROOT --sentvec VEC --out ROOT [--pct 90]   boundaries where the cosine distance between
                                           consecutive sentence windows exceeds the article's pct-th percentile
  texttile  --src A_ROOT --out ROOT [--block 3]               Hearst TextTiling on lexical cohesion
  prefix    --src V2_ROOT --out ROOT --mode kwd|title|title_kwd (--xml-cache DIR | --assets)
  latewin   --src CHUNK_ROOT --out ROOT [--window article|section] [--no-title]
"""
import argparse
import gzip
import hashlib
import io
import json
import math
import re
import sys
import tarfile
from collections import Counter, OrderedDict
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).parent))
import chunk_variants as cv  # noqa: E402

WS = re.compile(r"\S+")
ABBR = re.compile(r"(?:\b(?:Fig|Figs|Tab|Eq|Ref|Refs|No|Nos|vs|al|approx|ca|cf|e\.g|i\.e|etc|resp|Dr|Mr|Ms|Prof|St|Suppl|Vol|pp|ed|eds)|\b[A-Z])\.$")
SPLIT = re.compile(r"(?<=[.!?])[\"')\]]?\s+(?=[A-Z0-9(\[\"'])|\n\n+")
STOP = set("the of and to in a is for with that by on as are was were be this from at or an which we our these it not have has can between than also its using used been such into may their both other more after all two one three most".split())


def uid(*parts):
    return hashlib.sha256(json.dumps(parts).encode()).hexdigest()[:32]


def bundles(root):
    return sorted(p.parent for p in root.glob("runs/*/ready/*/items.jsonl.gz") if not p.parent.name.endswith(".partial"))


def write_bundle(root, run, name, rows):
    d = root / "runs" / run / "ready" / name
    d.mkdir(parents=True, exist_ok=True)
    with gzip.open(d / "items.jsonl.gz", "wt") as f:
        for r in rows:
            f.write(json.dumps(r) + "\n")
    (d / "manifest.json").write_text(json.dumps({"bundle": name, "rows": len(rows), "experiment": True}))


def base_rows(bundle):
    base = {}
    for line in gzip.open(bundle / "items.jsonl.gz", "rt"):
        r = json.loads(line)
        if r.get("kind") == "text":
            base.setdefault(r["article_id"], {k: r.get(k) for k in ("article_id", "pmcid", "pmid", "doi", "title", "license", "license_code")})
    return base


def groups(bundle):
    """-> [(article_id, [ {stream, path, sids, abstract} ...]) ] in document order (balanced-c grouping)."""
    out = []
    for art in cv.load_articles(bundle):
        gs, cur = [], []
        for s in art["sections"]:
            top = (s["path"][0] if s["path"] else s["sid"], s["abstract"])
            if cur and cur[0][0] != top:
                gs.append(cur)
                cur = []
            cur.append((top, s))
        if cur:
            gs.append(cur)
        res = []
        for g in gs:
            secs = [s for _, s in g]
            parts = []
            for s in secs:
                sub = s["path"][-1] if len(s["path"]) > 1 else ""
                parts.append((f"{sub}. " if sub and not s["text"].startswith(sub) else "") + s["text"])
            res.append({"stream": "\n\n".join(parts), "path": secs[0]["path"][:1], "sids": [s["sid"] for s in secs],
                        "abstract": secs[0]["abstract"]})
        out.append((art["article_id"], res))
    return out


def sentences_of(stream):
    """-> list of (start, end) char spans of sentences; no cut after common abbreviations or single initials."""
    spans, start = [], 0
    for m in SPLIT.finditer(stream):
        piece = stream[start:m.end()].strip()
        if not piece:
            start = m.end()
            continue
        if "\n\n" not in m.group(0) and ABBR.search(piece.rstrip("\"')]")[-14:]):
            continue  # abbreviation: keep accumulating this sentence
        s0 = start + (len(stream[start:m.end()]) - len(stream[start:m.end()].lstrip()))
        spans.append((s0, s0 + len(piece)))
        start = m.end()
    tail = stream[start:].strip()
    if tail:
        s0 = start + (len(stream[start:]) - len(stream[start:].lstrip()))
        spans.append((s0, s0 + len(tail)))
    return spans


def wcount(t):
    return len(WS.findall(t))


def enforce(stream, cuts, mn, mx, overlap=32):
    """Sentence-boundary segments -> chunks within [mn, mx] words: merge short ones forward, split long ones with
    chunk_variants.split_text (balanced budget, sentence snap). cuts = sorted char offsets of segment starts."""
    bounds = [0] + [c for c in cuts if 0 < c < len(stream)] + [len(stream)]
    segs = [(bounds[i], bounds[i + 1]) for i in range(len(bounds) - 1)]
    merged, cur = [], None
    for s, e in segs:
        if cur is None:
            cur = [s, e]
        elif wcount(stream[cur[0]:cur[1]]) < mn:
            cur[1] = e
        else:
            merged.append(cur)
            cur = [s, e]
    if cur:
        if merged and wcount(stream[cur[0]:cur[1]]) < mn:
            merged[-1][1] = cur[1]
        else:
            merged.append(cur)
    out = []
    for s, e in merged:
        seg = stream[s:e].strip()
        if not seg:
            continue
        s = stream.index(seg[0], s)
        w = wcount(seg)
        if w <= mx:
            out.append((seg, s, s + len(seg)))
            continue
        n = math.ceil(w / ((mn + mx) // 2))
        budget = min(mx, math.ceil(w / n) + overlap)
        for t, a0, a1, _ in cv.split_text(seg, budget, overlap):
            out.append((t, s + a0, s + a1))
    return out


def chunk_rows(aid, base, g, pieces, recipe, heading=True):
    rows = []
    for k, (t, s0, s1) in enumerate(pieces):
        r = dict(base)
        sec = g["path"]
        r.update({"unit_id": uid(aid, recipe["name"], tuple(g["sids"]), s0, s1), "kind": "text", "text": t,
                  "is_abstract": g["abstract"], "token_count": wcount(t), "chunk_recipe": recipe,
                  "locator": {"section_ids": g["sids"], "section": sec, "char_start": s0, "char_end": s1, "offsets": "packed-stream"}})
        if heading:
            r["embed_text"] = "title: none | text: " + (f"{sec[0]}\n" if sec and sec[0] else "") + t
        rows.append(r)
    return rows


def cmd_sentences(a):
    n = 0
    for b in bundles(a.src):
        rows = []
        for aid, gs in groups(b):
            for gi, g in enumerate(gs):
                sp = sentences_of(g["stream"])
                for i in range(len(sp)):
                    w = g["stream"][sp[max(0, i - 1)][0]:sp[min(len(sp) - 1, i + 1)][1]]
                    rows.append({"unit_id": f"s|{aid}|{gi}|{i}", "article_id": aid, "kind": "text", "text": w,
                                 "embed_text": "title: none | text: " + w})
        write_bundle(a.out, "sent", b.name, rows)
        n += len(rows)
    print(json.dumps({"sentence_windows": n}))


def load_vecs(vroot, name):
    d = vroot / "gemma" / name
    ids = json.loads((d / "ids.json").read_text())["unit_id"]
    v = np.load(d / "dense.npy").astype(np.float32)
    v /= np.maximum(np.linalg.norm(v, axis=1, keepdims=True), 1e-12)
    return {u: v[i] for i, u in enumerate(ids)}


def cmd_semantic(a):
    rec = {"name": "semantic", "pct": a.pct, "min": a.min, "max": a.max, "unit": "sentence-window cosine"}
    stats = Counter()
    for b in bundles(a.src):
        vec = load_vecs(a.sentvec, b.name)
        base, rows = base_rows(b), []
        for aid, gs in groups(b):
            dists, per = [], []
            for gi, g in enumerate(gs):
                sp = sentences_of(g["stream"])
                vs = [vec.get(f"s|{aid}|{gi}|{i}") for i in range(len(sp))]
                d = [1 - float(vs[i] @ vs[i + 1]) if vs[i] is not None and vs[i + 1] is not None else 0.0 for i in range(len(sp) - 1)]
                per.append((g, sp, d))
                dists += d
            thr = np.percentile(dists, a.pct) if dists else 1.0
            for g, sp, d in per:
                cuts = [sp[i + 1][0] for i, x in enumerate(d) if x > thr]
                pieces = enforce(g["stream"], cuts, a.min, a.max)
                rows += chunk_rows(aid, base.get(aid, {"article_id": aid}), g, pieces, rec)
                stats["chunks"] += len(pieces)
        write_bundle(a.out, f"sem{a.pct}", b.name, rows)
    print(json.dumps(dict(stats)))


def bow(t):
    return Counter(w for w in re.findall(r"[a-z][a-z0-9-]{2,}", t.lower()) if w not in STOP)


def cos_c(x, y):
    if not x or not y:
        return 0.0
    num = sum(v * y.get(k, 0) for k, v in x.items())
    return num / math.sqrt(sum(v * v for v in x.values()) * sum(v * v for v in y.values()))


def cmd_texttile(a):
    rec = {"name": "texttile", "block": a.block, "min": a.min, "max": a.max}
    stats = Counter()
    for b in bundles(a.src):
        base, rows = base_rows(b), []
        for aid, gs in groups(b):
            for g in gs:
                sp = sentences_of(g["stream"])
                bags = [bow(g["stream"][s:e]) for s, e in sp]
                gaps = []
                for i in range(1, len(sp)):
                    left, right = Counter(), Counter()
                    for x in bags[max(0, i - a.block):i]:
                        left.update(x)
                    for x in bags[i:i + a.block]:
                        right.update(x)
                    gaps.append(cos_c(left, right))
                cuts = []
                if len(gaps) >= 3:
                    depth = []
                    for i, s in enumerate(gaps):
                        lp = max(gaps[:i + 1])
                        rp = max(gaps[i:])
                        depth.append((lp - s) + (rp - s))
                    thr = np.mean(depth) - np.std(depth) / 2
                    cuts = [sp[i + 1][0] for i, dv in enumerate(depth) if dv > thr and dv > 0]
                pieces = enforce(g["stream"], cuts, a.min, a.max)
                rows += chunk_rows(aid, base.get(aid, {"article_id": aid}), g, pieces, rec)
                stats["chunks"] += len(pieces)
        write_bundle(a.out, f"tt{a.block}", b.name, rows)
    print(json.dumps(dict(stats)))


def keywords_from_xml(xml):
    m = re.search(rb"<article-meta>.*?</article-meta>", xml, re.S)
    meta = m.group(0) if m else xml[:200000]
    kw = [re.sub(rb"<[^>]+>", b"", k).decode("utf-8", "replace").strip() for k in re.findall(rb"<kwd>(.*?)</kwd>", meta, re.S)]
    return [re.sub(r"\s+", " ", k) for k in kw if k and not k.lower().startswith(("abbreviations",))][:12]


def cmd_prefix(a):
    stats = Counter()
    for b in bundles(a.src):
        kw = {}
        if a.assets and (b / "assets.tar").exists():
            with tarfile.open(b / "assets.tar") as tf:
                for m in tf.getmembers():
                    if m.name.endswith("/source.xml"):
                        kw[m.name.split("/", 1)[0]] = keywords_from_xml(tf.extractfile(m).read())
        rows = []
        for line in gzip.open(b / "items.jsonl.gz", "rt"):
            r = json.loads(line)
            if r.get("kind") != "text":
                continue
            aid = r["article_id"]
            if a.xml_cache and aid not in kw:
                p = a.xml_cache / f"{aid}.xml.gz"
                kw[aid] = keywords_from_xml(gzip.open(p).read()) if p.exists() else []
            sec = (r.get("locator") or {}).get("section") or []
            head = f"{sec[0]}\n" if sec and sec[0] else ""
            title = re.sub(r"\s+", " ", r.get("title") or "").strip()
            k = kw.get(aid) or []
            ctx = ""
            if "title" in a.mode and title:
                ctx += f"{title}\n"
            if "kwd" in a.mode and k:
                ctx += "Keywords: " + "; ".join(k) + "\n"
            stats["with_kwd"] += bool(k)
            stats["rows"] += 1
            r["embed_text"] = "title: none | text: " + ctx + head + r["text"]
            r["chunk_recipe"] = dict(r.get("chunk_recipe") or {}, prefix=a.mode)
            rows.append(r)
        write_bundle(a.out, f"pre-{a.mode}", b.name, rows)
    print(json.dumps(dict(stats)))


def cmd_latewin(a):
    n = 0
    for b in bundles(a.src):
        docs = OrderedDict()
        for line in gzip.open(b / "items.jsonl.gz", "rt"):
            r = json.loads(line)
            if r.get("kind") != "text" or not (r.get("text") or "").strip():
                continue
            sec = (r.get("locator") or {}).get("section") or []
            key = (r["article_id"],) if a.window == "article" else (r["article_id"], tuple((r.get("locator") or {}).get("section_ids") or sec))
            docs.setdefault(key, []).append(r)
        rows = []
        for key, rs in docs.items():
            title = re.sub(r"\s+", " ", rs[0].get("title") or "none").strip() or "none"
            prefix = "title: none | text: " if a.no_title else f"title: {title} | text: "
            doc, spans = "", []
            for r in rs:
                path = (r.get("locator") or {}).get("section") or []
                head = (" > ".join(x for x in path if x) + "\n") if any(path) else ""
                if doc:
                    doc += "\n\n"
                doc += head
                s0 = len(doc)
                doc += r["text"]
                spans.append({"id": r["unit_id"], "start": s0 + len(prefix), "end": len(doc) + len(prefix)})
            rows.append({"article_id": key[0], "unit_id": uid("late", *[str(x) for x in key]), "kind": "late_doc",
                         "text": prefix + doc, "prefix_chars": len(prefix), "spans": spans})
            n += len(spans)
        write_bundle(a.out, f"late-{a.window}{'-nt' if a.no_title else ''}", b.name, rows)
    print(json.dumps({"spans": n}))


def cmd_artvec(a):
    """One article-level unit per article: "title: <title> | text: <abstract chunks joined>" (EG2 document format, title
    slot used), unit_id "art|<article_id>"; for article-score fusion (score = max chunk + lam*mean + mu*cos(q, art))."""
    n = 0
    for b in bundles(a.src):
        arts = OrderedDict()
        for line in gzip.open(b / "items.jsonl.gz", "rt"):
            r = json.loads(line)
            if r.get("kind") != "text":
                continue
            x = arts.setdefault(r["article_id"], {"title": r.get("title") or "", "abs": [], "body": []})
            (x["abs"] if r.get("is_abstract") else x["body"]).append(r["text"])
        rows = []
        for aid, x in arts.items():
            title = re.sub(r"\s+", " ", x["title"]).strip() or "none"
            text = title if a.title_only else (" ".join(x["abs"]) or " ".join(x["body"])[:3000])
            rows.append({"unit_id": f"art|{aid}", "article_id": aid, "kind": "text", "text": text,
                         "embed_text": f"title: none | text: {title}" if a.title_only else f"title: {title} | text: {text}"})
        write_bundle(a.out, "artvec", b.name, rows)
        n += len(rows)
    print(json.dumps({"articles": n}))


def main():
    ap = argparse.ArgumentParser()
    sp = ap.add_subparsers(dest="cmd", required=True)
    for name in ("sentences", "semantic", "texttile", "prefix", "latewin", "artvec"):
        p = sp.add_parser(name)
        p.add_argument("--src", type=Path, required=True)
        p.add_argument("--out", type=Path, required=True)
        p.add_argument("--min", type=int, default=64)
        p.add_argument("--max", type=int, default=384)
        if name == "semantic":
            p.add_argument("--sentvec", type=Path, required=True)
            p.add_argument("--pct", type=float, default=90)
        if name == "texttile":
            p.add_argument("--block", type=int, default=3)
        if name == "prefix":
            p.add_argument("--mode", choices=["kwd", "title", "title_kwd"], required=True)
            p.add_argument("--xml-cache", type=Path)
            p.add_argument("--assets", action="store_true")
        if name == "artvec":
            p.add_argument("--title-only", action="store_true")
        if name == "latewin":
            p.add_argument("--window", choices=["article", "section"], default="article")
            p.add_argument("--no-title", action="store_true")
    a = ap.parse_args()
    {"sentences": cmd_sentences, "semantic": cmd_semantic, "texttile": cmd_texttile, "prefix": cmd_prefix,
     "latewin": cmd_latewin, "artvec": cmd_artvec}[a.cmd](a)


if __name__ == "__main__":
    sys.exit(main())
