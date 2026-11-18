"""Build a fixed, versioned benchmark input set for the PigMed embedding runtimes.

Outputs (under --out):
  texts.jsonl   {"id", "pmcid", "text"} passages of ~500 EmbeddingGemma-2 tokens, already in the card's
                document format `title: {title} | text: {content}`.
  images/*.png  8 page renders from the PigMed smoke bundles (2048 px longest side) + 8 modern PMC figures
                downscaled to <= 2048 px longest side (never upscaled), RGB PNG.
  images.jsonl  {"id", "path", "kind", "width", "height"}
  inputs.json   provenance (sources, sha256 of every file).

Text and figures come from the public pmc-oa-opendata bucket (open-access articles; anonymous HTTPS GET).
"""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import tarfile
import urllib.request
from pathlib import Path

from PIL import Image

BASE = "https://pmc-oa-opendata.s3.amazonaws.com/"


def get(url: str) -> bytes:
    req = urllib.request.Request(url, headers={"User-Agent": "pigmed-embed-bench/0.1"})
    with urllib.request.urlopen(req, timeout=60) as r:
        return r.read()


def s3_to_https(u: str) -> str:
    assert u.startswith("s3://pmc-oa-opendata/")
    return BASE + u[len("s3://pmc-oa-opendata/"):].split("?")[0]


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--bundles", type=Path, nargs="+", required=True, help="smoke bundle dirs with assets.tar")
    ap.add_argument("--gemma", required=True, help="local snapshot dir of google/embeddinggemma-2")
    ap.add_argument("--n-pages", type=int, default=8)
    ap.add_argument("--n-figures", type=int, default=8)
    ap.add_argument("--n-passages", type=int, default=64)
    ap.add_argument("--passage-tokens", type=int, default=500)
    ap.add_argument("--start-pmc", type=int, default=9000000)
    args = ap.parse_args()

    from transformers import AutoTokenizer

    tok = AutoTokenizer.from_pretrained(args.gemma)
    out: Path = args.out
    (out / "images").mkdir(parents=True, exist_ok=True)
    prov: dict = {"pages": [], "articles": []}
    images = []

    # 1) Page renders from the smoke bundles (real pipeline output).
    pages = 0
    for b in args.bundles:
        items = {}
        import gzip

        with gzip.open(b / "items.jsonl.gz", "rt") as f:
            for line in f:
                r = json.loads(line)
                if r.get("kind") in ("page", "figure") and r.get("asset_member"):
                    items[r["asset_member"]] = r
        with tarfile.open(b / "assets.tar") as tf:
            members = sorted(m for m in items if items[m]["kind"] == "page")
            for m in members[:: max(1, len(members) // max(1, args.n_pages // len(args.bundles)))]:
                if pages >= args.n_pages:
                    break
                im = Image.open(io.BytesIO(tf.extractfile(m).read())).convert("RGB")
                iid = f"page{pages:02d}"
                p = out / "images" / f"{iid}.png"
                im.save(p)
                images.append({"id": iid, "path": f"images/{iid}.png", "kind": "page", "width": im.width,
                                "height": im.height})
                prov["pages"].append({"id": iid, "bundle": str(b), "member": m, "source_url": items[m]["source_url"]})
                pages += 1

    # 2) Modern open-access articles: plain text + figures.
    passages = []
    figs = 0
    pmc = args.start_pmc
    while (len(passages) < args.n_passages or figs < args.n_figures) and pmc < args.start_pmc + 400:
        pmc += 1
        try:
            meta = json.loads(get(f"{BASE}metadata/PMC{pmc}.1.json"))
        except Exception:
            continue
        if meta.get("is_historical_ocr") or not meta.get("text_url"):
            continue
        try:
            text = get(s3_to_https(meta["text_url"])).decode("utf-8", "replace")
        except Exception:
            continue
        title = (meta.get("title") or "none").strip() or "none"
        ids = tok(text, add_special_tokens=False)["input_ids"]
        used = 0
        # At most 4 passages per article so the set spans several papers.
        for k in range(0, len(ids) - args.passage_tokens, args.passage_tokens):
            if len(passages) >= args.n_passages or used >= 4:
                break
            if k == 0:
                continue  # skip front matter
            body = tok.decode(ids[k:k + args.passage_tokens]).strip()
            passages.append({"id": f"t{len(passages):03d}", "pmcid": f"PMC{pmc}",
                             "text": f"title: {title} | text: {body}"})
            used += 1
        art = {"pmcid": f"PMC{pmc}", "title": title, "license": meta.get("license_code"), "passages": used,
               "figures": []}
        for mu in meta.get("media_urls") or []:
            if figs >= args.n_figures or len(art["figures"]) >= 2:
                break
            if not mu.split("?")[0].lower().endswith((".jpg", ".jpeg", ".png")):
                continue
            try:
                im = Image.open(io.BytesIO(get(s3_to_https(mu)))).convert("RGB")
            except Exception:
                continue
            if min(im.size) < 200:
                continue
            s = 2048 / max(im.size)
            if s < 1:
                im = im.resize((max(1, round(im.width * s)), max(1, round(im.height * s))), Image.Resampling.LANCZOS)
            iid = f"fig{figs:02d}"
            im.save(out / "images" / f"{iid}.png")
            images.append({"id": iid, "path": f"images/{iid}.png", "kind": "figure", "width": im.width,
                           "height": im.height})
            art["figures"].append({"id": iid, "source_url": mu.split("?")[0]})
            figs += 1
        if used or art["figures"]:
            prov["articles"].append(art)

    with open(out / "texts.jsonl", "w") as f:
        for p in passages:
            f.write(json.dumps(p) + "\n")
    with open(out / "images.jsonl", "w") as f:
        for im in images:
            f.write(json.dumps(im) + "\n")
    prov["sha256"] = {str(p.relative_to(out)): hashlib.sha256(p.read_bytes()).hexdigest()
                      for p in sorted(out.rglob("*")) if p.is_file() and p.name != "inputs.json"}
    (out / "inputs.json").write_text(json.dumps(prov, indent=1))
    print(f"passages={len(passages)} images={len(images)} (pages={pages}, figures={figs})")


if __name__ == "__main__":
    main()
