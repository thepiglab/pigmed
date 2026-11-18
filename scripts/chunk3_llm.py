"""Chunking v3 research, arms 8/9 (LLM-enriched index) on a ~5k-article sub-corpus. Research only, not v3 candidates.

  subset  --x WORK/chunk3 --s-queries Q.json --n 5000 [--rel 20 --nonrel 10]
          Picks the sub-corpus and writes WORK/chunk3/sub/articles.json:
            - every S target article (bench1k, from the 98k corpus);
            - per TREC topic: up to --rel judged-relevant and --nonrel judged-non-relevant PMCIDs from the real corpus
              (judged non-relevant docs are hard negatives: pooled from TREC runs);
            - random 98k distractors up to --n.
  filter  --x WORK/chunk3 --arms v2 v2-late arttitle
          sub/<corpus>/<arm>/root = rows of the chosen articles. Vectors are not copied: the scorer reads the full
          <corpus>/<arm>/vectors and keeps only the ids in the root.
  tasks   --x WORK/chunk3 --mode ctx-chunk|ctx-section|prop --out TASKS.jsonl [--max-words 12000]
          Chat tasks for pigmed-llmbatch, from sub/<corpus>/v2/root. Tasks for one article are adjacent and share the
          document prefix (server prefix cache).
            ctx-chunk    Anthropic "contextual retrieval": a 50-100 token context per chunk, given the whole document
            ctx-section  one context per section group (balanced-c group), shared by its chunks: ~5x fewer calls
            prop         Dense X propositions per chunk (title + heading for decontextualisation), JSON list
  apply   --x WORK/chunk3 --mode ... --results R.jsonl [R2.jsonl ...]
          sub/<corpus>/<mode>/root: ctx-* = v2 rows with the context before the heading line in embed_text (same
          unit_ids, so the v2-late mix applies); prop = one unit per proposition (unit_id <chunk>|p<i>).
"""
import argparse
import gzip
import json
import random
import re
import sys
from collections import Counter, OrderedDict, defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from chunk3_arms import bundles, write_bundle  # noqa: E402

CORPORA = ("98k", "real")

CTX_DOC = "<document>\n{doc}\n</document>\n"
CTX_CHUNK = ("Here is the chunk we want to situate within the whole document\n<chunk>\n{chunk}\n</chunk>\n"
             "Please give a short succinct context to situate this chunk within the overall document for the purposes of "
             "improving search retrieval of the chunk. Answer only with the succinct context and nothing else.")
CTX_SECTION = ("Here is a section we want to situate within the whole document\n<section>\n{chunk}\n</section>\n"
               "Please give a short succinct context (at most 80 words) to situate this section within the overall document "
               "for the purposes of improving search retrieval of passages from it. Answer only with the succinct context "
               "and nothing else.")
PROP = ("Decompose the passage into clear and simple propositions, ensuring they are interpretable out of context.\n"
        "1. Split compound sentences into simple sentences. Keep the original phrasing from the input whenever possible.\n"
        "2. For any named entity accompanied by additional descriptive information, separate this information into its "
        "own distinct proposition.\n"
        "3. Decontextualize each proposition by adding necessary modifiers to nouns or entire sentences and replacing "
        "pronouns (e.g. \"it\", \"he\", \"this\", \"they\") with the full name of the entities they refer to.\n"
        "4. Skip author lists, funding, disclosures and reference fragments.\n"
        "Return JSON: {{\"propositions\": [ ... ]}}.\n\n"
        "Article title: {title}\nSection: {section}\nPassage:\n{chunk}")
PROP_SCHEMA = {"type": "object", "properties": {"propositions": {"type": "array", "items": {"type": "string"}}},
               "required": ["propositions"]}


def rows_of(root):
    for b in bundles(root):
        for line in gzip.open(b / "items.jsonl.gz", "rt"):
            yield b.name, json.loads(line)


def pmcid_of(aid):
    return aid.split(".")[0]


def cmd_subset(a):
    rnd = random.Random(0)
    s_arts = sorted({q["article_id"] for q in json.loads(a.s_queries.read_text())})
    qrels = json.loads((a.x / "real/qrels.json").read_text())
    real_by_pmcid = {}
    for _, r in rows_of(a.x / "real/v2/root"):
        if r.get("kind") == "text":
            real_by_pmcid.setdefault(pmcid_of(r["article_id"]), r["article_id"])
    k98 = sorted({r["article_id"] for _, r in rows_of(a.x / "98k/arttitle/root")})
    have = {pmcid_of(x) for x in s_arts}
    real_pick = []
    topics = sorted({re.sub(r"^covid-(q|kw)-", "covid-", q) for q in qrels})
    for t in topics:
        src = qrels.get(t) or qrels.get(t.replace("covid-", "covid-q-")) or {}
        rel = sorted(p for p, g in src.items() if g > 0 and p in real_by_pmcid)
        non = sorted(p for p, g in src.items() if g <= 0 and p in real_by_pmcid)
        rnd.shuffle(rel)
        rnd.shuffle(non)
        for p in rel[:a.rel] + non[:a.nonrel]:
            if p not in have:
                have.add(p)
                real_pick.append(real_by_pmcid[p])
    fill = [x for x in k98 if pmcid_of(x) not in have and x not in s_arts]
    rnd.shuffle(fill)
    n_fill = max(0, a.n - len(s_arts) - len(real_pick))
    out = {"98k": sorted(s_arts + fill[:n_fill]), "real": sorted(real_pick),
           "counts": {"s_targets": len(s_arts), "real_judged": len(real_pick), "fill_98k": min(n_fill, len(fill)), "topics": len(topics)}}
    (a.x / "sub").mkdir(parents=True, exist_ok=True)
    (a.x / "sub/articles.json").write_text(json.dumps(out))
    print(json.dumps(out["counts"]))


def cmd_filter(a):
    sel = json.loads((a.x / "sub/articles.json").read_text())
    for c in CORPORA:
        keep = set(sel[c])
        for arm in a.arms:
            src, dst = a.x / c / arm / "root", a.x / "sub" / c / arm / "root"
            n = 0
            for b in bundles(src):
                rows = [r for r in (json.loads(l) for l in gzip.open(b / "items.jsonl.gz", "rt")) if r.get("article_id") in keep]
                if rows:
                    write_bundle(dst, b.parent.parent.name, b.name, rows)
                    n += len(rows)
            print(c, arm, n, flush=True)


def articles(root):
    """-> OrderedDict aid -> {title, groups: OrderedDict(section_ids tuple -> {heading, stream, chunks: [row]})}.
    Each group's stream is rebuilt from its overlapping chunks by char offsets (packed-stream locator)."""
    arts = OrderedDict()
    for bname, r in rows_of(root):
        if r.get("kind") != "text":
            continue
        loc = r.get("locator") or {}
        x = arts.setdefault(r["article_id"], {"title": re.sub(r"\s+", " ", r.get("title") or "").strip(), "groups": OrderedDict(), "bundle": bname})
        sec = loc.get("section") or []
        g = x["groups"].setdefault(tuple(loc.get("section_ids") or []), {"heading": sec[0] if sec and sec[0] else "", "pieces": [], "chunks": []})
        g["pieces"].append((loc.get("char_start", 0), loc.get("char_end", 0), r["text"]))
        g["chunks"].append(r)
    for x in arts.values():
        for g in x["groups"].values():
            n = max(e for _, e, _ in g["pieces"])
            buf = [" "] * n
            for s, e, t in g["pieces"]:
                if e - s == len(t):
                    buf[s:e] = list(t)
                else:  # offsets do not match the text: append
                    buf += list("\n" + t)
            g["stream"] = re.sub(r"[ \t]+\n", "\n", "".join(buf)).strip()
    return arts


def doc_text(x, max_words):
    parts = [f"Title: {x['title']}"] if x["title"] else []
    for g in x["groups"].values():
        parts.append((g["heading"] + "\n" if g["heading"] else "") + g["stream"])
    words = "\n\n".join(parts).split(" ")
    return " ".join(words[:max_words])


def cmd_tasks(a):
    n = Counter()
    with open(a.out, "w") as f:
        for c in CORPORA:
            for aid, x in articles(a.x / "sub" / c / "v2/root").items():
                doc = doc_text(x, a.max_words) if a.mode.startswith("ctx") else None
                for sids, g in x["groups"].items():
                    if a.mode == "ctx-section":
                        t = {"id": f"{c}|{aid}|{json.dumps(list(sids))}", "max_tokens": 200,
                             "messages": [{"role": "user", "content": CTX_DOC.format(doc=doc) + CTX_SECTION.format(chunk=" ".join(g["stream"].split(" ")[:1500]))}]}
                        f.write(json.dumps(t) + "\n")
                        n["tasks"] += 1
                        n["chunks"] += len(g["chunks"])
                        continue
                    for r in g["chunks"]:
                        if a.mode == "ctx-chunk":
                            t = {"id": r["unit_id"], "max_tokens": 200,
                                 "messages": [{"role": "user", "content": CTX_DOC.format(doc=doc) + CTX_CHUNK.format(chunk=r["text"])}]}
                        else:
                            t = {"id": r["unit_id"], "max_tokens": 2000, "schema": PROP_SCHEMA,
                                 "messages": [{"role": "user", "content": PROP.format(title=x["title"] or "none", section=g["heading"] or "none", chunk=r["text"])}]}
                        f.write(json.dumps(t) + "\n")
                        n["tasks"] += 1
                        n["chunks"] += 1
                n["articles"] += 1
    print(json.dumps(dict(n)))


def load_results(paths):
    out = {}
    for p in paths:
        for line in open(p):
            r = json.loads(line)
            if r.get("error") is None and isinstance(r.get("content"), str):
                out[r["id"]] = r
    return out


def cmd_apply(a):
    res = load_results(a.results)
    st = Counter()
    for c in CORPORA:
        by_bundle = defaultdict(list)
        for aid, x in articles(a.x / "sub" / c / "v2/root").items():
            for sids, g in x["groups"].items():
                gctx = res.get(f"{c}|{aid}|{json.dumps(list(sids))}")
                for r in g["chunks"]:
                    head = f"{g['heading']}\n" if g["heading"] else ""
                    if a.mode.startswith("ctx"):
                        hit = gctx if a.mode == "ctx-section" else res.get(r["unit_id"])
                        ctx = re.sub(r"\s+", " ", hit["content"]).strip() if hit else ""
                        st["with_ctx" if ctx else "no_ctx"] += 1
                        st["ctx_words"] += len(ctx.split())
                        r = dict(r, embed_text="title: none | text: " + (ctx + "\n" if ctx else "") + head + r["text"],
                                 chunk_recipe=dict(r.get("chunk_recipe") or {}, context=a.mode))
                        by_bundle[x["bundle"]].append(r)
                    else:
                        hit = res.get(r["unit_id"])
                        props = []
                        if hit:
                            try:
                                props = [p.strip() for p in json.loads(hit["content"]).get("propositions", []) if isinstance(p, str) and p.strip()]
                            except (ValueError, AttributeError):
                                st["bad_json"] += 1
                        st["chunks"] += 1
                        st["props"] += len(props)
                        if not props:  # keep the chunk itself so the article stays retrievable
                            st["chunk_fallback"] += 1
                            by_bundle[x["bundle"]].append(r)
                            continue
                        for i, p in enumerate(props):
                            by_bundle[x["bundle"]].append({"unit_id": f"{r['unit_id']}|p{i}", "article_id": r["article_id"], "pmcid": r.get("pmcid"),
                                                           "title": r.get("title"), "kind": "text", "text": p, "is_abstract": r.get("is_abstract"),
                                                           "parent": r["unit_id"], "embed_text": "title: none | text: " + p})
        for bname, rows in by_bundle.items():
            write_bundle(a.x / "sub" / c / a.mode / "root", a.mode, bname, rows)
    print(json.dumps(dict(st)))


def main():
    ap = argparse.ArgumentParser()
    sp = ap.add_subparsers(dest="cmd", required=True)
    s = sp.add_parser("subset")
    s.add_argument("--x", type=Path, required=True)
    s.add_argument("--s-queries", type=Path, required=True)
    s.add_argument("--n", type=int, default=5000)
    s.add_argument("--rel", type=int, default=20)
    s.add_argument("--nonrel", type=int, default=10)
    f = sp.add_parser("filter")
    f.add_argument("--x", type=Path, required=True)
    f.add_argument("--arms", nargs="+", required=True)
    t = sp.add_parser("tasks")
    t.add_argument("--x", type=Path, required=True)
    t.add_argument("--mode", choices=["ctx-chunk", "ctx-section", "prop"], required=True)
    t.add_argument("--out", type=Path, required=True)
    t.add_argument("--max-words", type=int, default=12000)
    p = sp.add_parser("apply")
    p.add_argument("--x", type=Path, required=True)
    p.add_argument("--mode", choices=["ctx-chunk", "ctx-section", "prop"], required=True)
    p.add_argument("--results", nargs="+", required=True)
    a = ap.parse_args()
    {"subset": cmd_subset, "filter": cmd_filter, "tasks": cmd_tasks, "apply": cmd_apply}[a.cmd](a)


if __name__ == "__main__":
    sys.exit(main())
