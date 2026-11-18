"""OCR engine benchmark on real supplementary PDF pages.

Ground truth: born-digital PDF pages (their text layer is the reference). Each page is rendered to a
300-dpi grayscale PNG and OCR'd as if it were a scan, so every engine gets a real error rate on our
own content. Image-only pages (real scans / figure sheets, no reference) get cost + cross-engine
agreement, and their outputs are kept for visual inspection.

  prepare  --root BUNDLE_ROOT --out DIR     pick pages from ingest bundles, download, render, reference text
  run      --out DIR --engine NAME          OCR every page (or --limit N) with one engine; per-page CPU seconds
  score    --out DIR                        metrics per engine and page class

Engines: tesseract (300 dpi), tesseract-200, tesseract-200-osd (psm 1), tesseract-200-rot (psm 0 OSD + rotate), onnxtr-fast /
onnxtr-accurate (docTR ONNX port; "-rot" suffix straightens rotated pages),
ocrs (Rust CLI), granite-docling (IBM granite-docling-258M via transformers).
Cost is CPU seconds per page (user+sys via getrusage, children included), so engines that use
several threads are compared fairly with single-threaded ones.
"""
from __future__ import annotations
import argparse
import glob
import gzip
import hashlib
import json
import os
import re
import resource
import subprocess
import sys
import time
import unicodedata
from concurrent.futures import ProcessPoolExecutor
from pathlib import Path


def https(url: str) -> str:
    if url.startswith("s3://"):
        b, _, k = url[5:].partition("/")
        return f"https://{b}.s3.amazonaws.com/{k}"
    return url


def stable(s: str) -> str:
    return hashlib.sha256(s.encode()).hexdigest()


# ------------------------------------------------------------------ prepare

def prepare(a):
    import pypdfium2 as pdfium
    import requests
    out = Path(a.out); (out / "pages").mkdir(parents=True, exist_ok=True); (out / "pdf").mkdir(exist_ok=True)
    text_pdfs, image_pdfs = [], []
    for f in sorted(glob.glob(f"{a.root}/runs/*/ready/*/items.jsonl.gz")):
        for line in gzip.open(f, "rt"):
            if '"supplement"' not in line:
                continue
            r = json.loads(line)
            s = r.get("supplement") or {}
            if r.get("extension") != ".pdf":
                continue
            if s.get("status") == "ok" and s.get("tool") == "pdf-inspector" and s.get("pdf_type") == "TextBased":
                text_pdfs.append(r)
            elif s.get("status") == "no-text":
                image_pdfs.append(r)
    pick = lambda rows, n: sorted(rows, key=lambda r: stable("ocr-bench" + r["source_url"]))[:n]
    pages = []
    for group, rows, per_pdf in (("gt", pick(text_pdfs, a.text_pdfs), 3), ("image", pick(image_pdfs, a.image_pdfs), 2)):
        for i, r in enumerate(rows):
            p = out / "pdf" / f"{group}{i:03d}.pdf"
            try:
                if not p.exists():
                    resp = requests.get(https(r["source_url"]), timeout=120)
                    resp.raise_for_status()
                    p.write_bytes(resp.content)
                doc = pdfium.PdfDocument(str(p))
            except Exception as e:
                print("skip", r["file_name"], e, file=sys.stderr)
                continue
            n = len(doc)
            # GT: pages whose text layer has enough words; image: first pages.
            cand = list(range(n))
            chosen = []
            for k in cand:
                page = doc[k]
                ref = page.get_textpage().get_text_range() if group == "gt" else ""
                words = re.findall(r"\w+", ref)
                if group == "gt" and len(words) < 40:
                    continue
                w, h = page.get_size()
                scale = min(300 / 72, 4400 / max(w, h))
                img = page.render(scale=scale, grayscale=True).to_pil()
                pid = f"{group}{i:03d}-p{k + 1:03d}"
                img.save(out / "pages" / f"{pid}.png")
                numeric = sum(bool(re.fullmatch(r"[\d.,%+\-–()<>=×eE]+", t)) for t in ref.split()) / max(1, len(ref.split()))
                pages.append({"id": pid, "group": group, "file": r["file_name"], "article_id": r["article_id"], "page": k + 1,
                              "png": str(out / "pages" / f"{pid}.png"), "ref": ref,
                              "class": ("table" if numeric > 0.35 else "prose") if group == "gt" else "image",
                "ref_words_per_mpx": round(len(words) / (img.size[0] * img.size[1] / 1e6), 1),
                              "width_px": img.size[0], "height_px": img.size[1]})
                chosen.append(k)
                if len(chosen) >= per_pdf:
                    break
    (out / "pages.jsonl").write_text("".join(json.dumps(p) + "\n" for p in pages))
    from collections import Counter
    print(json.dumps({"pages": len(pages), "by_class": Counter(p["class"] for p in pages),
                      "text_pdf_candidates": len(text_pdfs), "image_pdf_candidates": len(image_pdfs)}))


# ------------------------------------------------------------------ engines

def cpu_now():
    s, c = resource.getrusage(resource.RUSAGE_SELF), resource.getrusage(resource.RUSAGE_CHILDREN)
    return s.ru_utime + s.ru_stime + c.ru_utime + c.ru_stime


_ENGINE = None


def make_engine(name: str, threads: int):
    os.environ.setdefault("OMP_NUM_THREADS", str(threads))
    os.environ["OMP_THREAD_LIMIT"] = str(threads)
    if name.startswith("tesseract"):
        dpi = 300 if name == "tesseract" else 200
        psm = "1" if name.endswith("osd") else "3"  # 1 = automatic orientation + script detection (needs osd.traineddata)

        def run(png):
            path = png
            if dpi != 300:
                from PIL import Image
                im = Image.open(png)
                im = im.resize((round(im.width * dpi / 300), round(im.height * dpi / 300)))
                path = png + f".{dpi}.png"
                im.save(path)
            def ocr(f):  # text + confidence-weighted quality (chars of words with conf >= 60), one run
                base = f + ".ocr"
                subprocess.run(["tesseract", f, base, "-l", "eng", "--psm", psm, "txt", "tsv"], capture_output=True, timeout=300)
                text = open(base + ".txt").read() if os.path.exists(base + ".txt") else ""
                q = 0
                if os.path.exists(base + ".tsv"):
                    for line in open(base + ".tsv").read().splitlines()[1:]:
                        c = line.split("\t")
                        if len(c) >= 12 and c[11].strip() and float(c[10]) >= 60:
                            q += len(c[11].strip())
                return text, q
            text, q = ocr(path)
            if name.endswith("-rot"):  # OSD proposes a rotation; OCR it too and keep the more confident read
                o = subprocess.run(["tesseract", path, "stdout", "--psm", "0"], capture_output=True, text=True, timeout=120).stdout
                rot = re.search(r"Rotate: (\d+)", o); conf = re.search(r"Orientation confidence: ([\d.]+)", o)
                if rot and conf and int(rot.group(1)) in (90, 180, 270) and float(conf.group(1)) >= 1.0:
                    from PIL import Image
                    p2 = png + ".rot.png"; Image.open(path).rotate(-int(rot.group(1)), expand=True).save(p2)
                    t2, q2 = ocr(p2)
                    if q2 > q:
                        text = t2
            return text
        return run
    if name.startswith("onnxtr"):
        import onnxruntime as ort
        from onnxtr.io import DocumentFile
        from onnxtr.models import ocr_predictor
        try:
            from onnxtr.models import EngineConfig
            so = ort.SessionOptions(); so.intra_op_num_threads = threads; so.inter_op_num_threads = 1
            cfg = EngineConfig(session_options=so, providers=["CPUExecutionProvider"])
            kw = {"det_engine_cfg": cfg, "reco_engine_cfg": cfg}
        except Exception:
            kw = {}
        det, reco = ("db_mobilenet_v3_large", "crnn_mobilenet_v3_small") if name.startswith("onnxtr-fast") else ("fast_base", "parseq")
        if name.endswith("-rot"):  # rotated pages (landscape tables): classify page orientation and straighten first
            kw.update(assume_straight_pages=False, detect_orientation=True, straighten_pages=True)
        try:
            model = ocr_predictor(det_arch=det, reco_arch=reco, **kw)
        except TypeError:
            model = ocr_predictor(det_arch=det, reco_arch=reco)

        def run(png):
            return model(DocumentFile.from_images(png)).render()
        return run
    if name == "ocrs":
        models = Path(os.environ.get("OCRS_MODELS", ""))

        def run(png):
            cmd = ["ocrs", png]
            for ext in ("onnx", "rten"):  # pre-fetched models: avoids concurrent first-run downloads racing on one cache file
                if (models / f"text-detection.{ext}").exists():
                    cmd += ["--detect-model", str(models / f"text-detection.{ext}"), "--rec-model", str(models / f"text-recognition.{ext}")]
                    break
            r = subprocess.run(cmd, capture_output=True, text=True, timeout=300, env={**os.environ, "RAYON_NUM_THREADS": str(threads)})
            if r.returncode:
                raise RuntimeError(r.stderr[-300:])
            return r.stdout
        return run
    if name == "granite-docling":
        import torch
        torch.set_num_threads(threads)
        from PIL import Image
        from transformers import AutoProcessor
        try:
            from transformers import AutoModelForImageTextToText as AutoVLM
        except ImportError:
            from transformers import AutoModelForVision2Seq as AutoVLM
        mid = "ibm-granite/granite-docling-258M"
        proc = AutoProcessor.from_pretrained(mid)
        model = AutoVLM.from_pretrained(mid, dtype=torch.float32, _attn_implementation="sdpa").eval()
        msgs = [{"role": "user", "content": [{"type": "image"}, {"type": "text", "text": "Convert this page to docling."}]}]
        prompt = proc.apply_chat_template(msgs, add_generation_prompt=True)

        def run(png):
            img = Image.open(png).convert("RGB")
            inputs = proc(text=prompt, images=[img], return_tensors="pt")
            with torch.inference_mode():
                ids = model.generate(**inputs, max_new_tokens=4096)
            tags = proc.batch_decode(ids[:, inputs["input_ids"].shape[1]:], skip_special_tokens=False)[0].lstrip()
            try:
                from docling_core.types.doc import DoclingDocument
                from docling_core.types.doc.document import DocTagsDocument
                d = DocTagsDocument.from_doctags_and_image_pairs([tags], [img])
                return DoclingDocument.load_from_doctags(d, document_name="p").export_to_markdown()
            except Exception:
                return re.sub(r"<[^>]+>", " ", tags)
        return run
    raise SystemExit(f"unknown engine {name}")


def _init(name, threads):
    global _ENGINE
    _ENGINE = make_engine(name, threads)


def _one(page):
    t, c = time.perf_counter(), cpu_now()
    try:
        text, err = _ENGINE(page["png"]), None
    except Exception as e:
        text, err = "", str(e)[:300]
    return {"id": page["id"], "text": text, "error": err, "wall_s": time.perf_counter() - t, "cpu_s": cpu_now() - c}


def run(a):
    out = Path(a.out)
    pages = [json.loads(l) for l in open(out / "pages.jsonl")]
    if a.limit:
        gt = [p for p in pages if p["group"] == "gt"][: a.limit]
        im = [p for p in pages if p["group"] == "image"][: max(1, a.limit // 4)]
        pages = gt + im
    t0 = time.perf_counter()
    with ProcessPoolExecutor(a.workers, initializer=_init, initargs=(a.engine, a.threads)) as ex:
        res = list(ex.map(_one, pages, chunksize=1))
    wall = time.perf_counter() - t0
    (out / f"ocr-{a.engine}.jsonl").write_text("".join(json.dumps(r) + "\n" for r in res))
    errs = [r for r in res if r["error"]]
    print(json.dumps({"engine": a.engine, "pages": len(res), "wall_s": round(wall, 1), "workers": a.workers, "threads": a.threads,
                      "errors": len(errs), "first_error": errs[0]["error"] if errs else None}))


# ------------------------------------------------------------------ score

LIG = str.maketrans({"ﬁ": "fi", "ﬂ": "fl", "ﬀ": "ff", "ﬃ": "ffi", "ﬄ": "ffl", "‐": "-", "–": "-", "—": "-", "−": "-"})


def norm(s: str) -> str:
    s = unicodedata.normalize("NFKC", s).translate(LIG).lower()
    s = re.sub(r"[|#*_`]+", " ", s)  # markdown syntax from structured engines
    return " ".join(s.split())


def toks(s: str):
    return re.findall(r"\w+", norm(s))


def score(a):
    from collections import Counter, defaultdict
    from rapidfuzz.distance import Levenshtein
    out = Path(a.out)
    pages = {p["id"]: p for p in (json.loads(l) for l in open(out / "pages.jsonl"))}
    engines = sorted(f[len(str(out)) + 5:-6] for f in glob.glob(str(out / "ocr-*.jsonl")))
    report = {}
    texts = {}
    for e in engines:
        rows = [json.loads(l) for l in open(out / f"ocr-{e}.jsonl")]
        texts[e] = {r["id"]: r["text"] for r in rows}
        agg = defaultdict(lambda: {"n": 0, "tp": 0, "ref": 0, "hyp": 0, "cer": []})
        cpu = [r["cpu_s"] for r in rows]
        worst = []
        for r in rows:
            p = pages[r["id"]]
            if p["group"] != "gt":
                continue
            R, H = Counter(toks(p["ref"])), Counter(toks(r["text"]))
            tp = sum((R & H).values())
            cer = Levenshtein.normalized_distance(norm(p["ref"]), norm(r["text"]))
            # Vector micro-text (e.g. thousands of gene labels on one figure) is below any OCR's resolution at 300 dpi.
            wpm = p.get("ref_words_per_mpx") or sum(R.values()) / (p["width_px"] * p["height_px"] / 1e6)
            klass = "illegible" if wpm > 1000 else p["class"]
            for k in (("all", "legible", klass) if klass != "illegible" else ("all", klass)):
                g = agg[k]; g["n"] += 1; g["tp"] += tp; g["ref"] += sum(R.values()); g["hyp"] += sum(H.values()); g["cer"].append(cer); g.setdefault("pr", []).append(tp / max(1, sum(R.values())))
            worst.append((tp / max(1, sum(R.values())), r["id"]))
        rep = {"cpu_s_per_page_mean": round(sum(cpu) / len(cpu), 2), "cpu_s_per_page_p90": round(sorted(cpu)[int(0.9 * (len(cpu) - 1))], 2),
               "pages": len(rows), "errors": sum(1 for r in rows if r["error"])}
        for k, g in agg.items():
            rec, prec = g["tp"] / max(1, g["ref"]), g["tp"] / max(1, g["hyp"])
            rep[k] = {"pages": g["n"], "word_recall": round(rec, 3), "word_precision": round(prec, 3),
                      "word_f1": round(2 * rec * prec / max(1e-9, rec + prec), 3), "cer_median": round(sorted(g["cer"])[len(g["cer"]) // 2], 3),
                      "page_recall_median": round(sorted(g["pr"])[len(g["pr"]) // 2], 3), "page_recall_p10": round(sorted(g["pr"])[len(g["pr"]) // 10], 3),
                      "pages_recall_lt_0.5": sum(x < 0.5 for x in g["pr"])}
        rep["worst_gt_pages"] = [i for _, i in sorted(worst)[:5]]
        report[e] = rep
    # Image-only pages: words found and pairwise agreement with tesseract (no reference exists).
    img_ids = [i for i, p in pages.items() if p["group"] == "image"]
    for e in engines:
        found = [len(toks(texts[e].get(i, ""))) for i in img_ids if i in texts[e]]
        report[e]["image_pages"] = {"pages": len(found), "words_mean": round(sum(found) / max(1, len(found)), 1)}
    print(json.dumps(report, indent=1))
    (out / "score.json").write_text(json.dumps(report, indent=1))


def main():
    p = argparse.ArgumentParser()
    s = p.add_subparsers(dest="cmd", required=True)
    x = s.add_parser("prepare"); x.add_argument("--root", required=True); x.add_argument("--out", required=True)
    x.add_argument("--text-pdfs", type=int, default=70); x.add_argument("--image-pdfs", type=int, default=40)
    x = s.add_parser("run"); x.add_argument("--out", required=True); x.add_argument("--engine", required=True)
    x.add_argument("--workers", type=int, default=16); x.add_argument("--threads", type=int, default=1); x.add_argument("--limit", type=int, default=0)
    x = s.add_parser("score"); x.add_argument("--out", required=True)
    a = p.parse_args()
    {"prepare": prepare, "run": run, "score": score}[a.cmd](a)


if __name__ == "__main__":
    main()
