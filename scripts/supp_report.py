"""Summarise supplement ingestion in a run root, optionally against a run without supplements.

usage: python -I scripts/supp_report.py ROOT [BASELINE_ROOT]
"""
import gzip
import json
import sys
from collections import Counter
from pathlib import Path


def rows(root):
    for f in sorted(Path(root).glob("runs/*/ready/*/items.jsonl.gz")):
        if f.parent.name.endswith(".partial"):
            continue
        with gzip.open(f, "rt") as fh:
            for line in fh:
                yield json.loads(line)


def words(s):
    return len((s or "").split())


def summarise(root):
    kinds, wordsk, status, by_ext_ok, by_ext_all, timing = Counter(), Counter(), Counter(), Counter(), Counter(), []
    articles, with_supp_units, card_trunc, cards = set(), set(), 0, 0
    for r in rows(root):
        k = r.get("kind")
        kinds[k] += 1
        if k == "article":
            articles.add(r["article_id"])
        if k in ("text", "table_text", "caption", "supplement_text", "table_card"):
            wordsk[k] += words(r.get("text"))
        if k in ("supplement_text", "table_card"):
            with_supp_units.add(r["article_id"])
        if k == "table_card":
            cards += 1
            card_trunc += bool(r["locator"].get("truncated"))
        if k == "media" and "supplement" in r:
            s = r["supplement"]
            status[s["status"]] += 1
            by_ext_all[r["extension"]] += 1
            if s["status"] == "ok":
                by_ext_ok[r["extension"]] += 1
                timing.append((s.get("download_ms", 0), s.get("convert_ms", 0)))
    main = wordsk["text"] + wordsk["table_text"] + wordsk["caption"]
    med = lambda v: sorted(v)[len(v) // 2] if v else None
    return {"articles": len(articles), "row_kinds": dict(kinds), "words_by_kind": dict(wordsk),
            "supplement_words_vs_main": {"prose": wordsk["supplement_text"] / max(main, 1), "table_cards": wordsk["table_card"] / max(main, 1)},
            "articles_with_supplement_units": len(with_supp_units),
            "supplement_file_status": dict(status),
            "ok_by_extension": {e: f"{by_ext_ok[e]}/{n}" for e, n in by_ext_all.most_common(20)},
            "table_cards": cards, "table_cards_truncated": card_trunc,
            "median_download_ms": med([t[0] for t in timing]), "median_convert_ms": med([t[1] for t in timing]),
            "embeddable_units": sum(kinds[k] for k in ("text", "table_text", "caption", "supplement_text", "table_card"))}


out = {"supplements": summarise(sys.argv[1])}
if len(sys.argv) > 2:
    out["baseline"] = {k: v for k, v in summarise(sys.argv[2]).items() if k in ("articles", "row_kinds", "words_by_kind", "embeddable_units")}
print(json.dumps(out, indent=2))
