"""Embed committed ingest bundles with a flashembed CPU runner, one bundle at a time per worker.

Workers on any number of nodes share an ingest ROOT and a VECTORS dir. A worker claims a bundle with a
non-blocking flock on VECTORS/<model>/<bundle>.lock (released automatically if the worker dies), builds the
model's inputs, runs `flashembed embed` once per modality, and writes VECTORS/<model>/<bundle>/:

  dense.npy      float16 [N, dim], L2-normalised by the runner
  ids.json       {"unit_id": [...], "modality": [...]} in row order
  late.npy, late_offsets.npy   NeoMME late vectors (float16), only for bundles in the late sample
  meta.json      counts, seconds per modality, items/s, skipped media; written last = done marker

Inputs per model (model-card formats):
  text   kinds text, table_text, caption, supplement_text, table_card. EmbeddingGemma 2 documents use
         "title: none | text: <chunk>"; NeoMME gets the raw chunk (runner adds <doc>).
  image  figure units: the original file is fetched from its source_url (bundles are text-only;
         archive_media=false) and checked with Pillow; undecodable or missing files are skipped and counted.
  video, audio   media rows with that modality (EmbeddingGemma 2 only), size-capped.

Also writes VECTORS/units/<bundle>.jsonl.gz (unit_id, article_id, kind, figure id, caption, title) once, so
evaluation still works after the publisher deletes the bundle.

usage: python -I scripts/embed_bundles.py --root R --vectors V --model gemma|neomme --bin FLASHEMBED
         --onnx DIR --snapshot DIR [--threads N] [--batch B] [--until-file F] [--late-mod M]
"""
from __future__ import annotations

import argparse
import concurrent.futures as cf
import fcntl
import gzip
import hashlib
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import numpy as np
import requests

TEXT_KINDS = {"text", "table_text", "caption", "supplement_text", "table_card"}
MEDIA_CAP = {"image": 64 << 20, "video": 512 << 20, "audio": 256 << 20}


def https(url: str) -> str:
    if url.startswith("s3://"):
        bucket, _, key = url[5:].partition("/")
        return f"https://{bucket}.s3.amazonaws.com/{key.split('?')[0]}"
    return url


def atomic_write(path: Path, data: bytes):
    tmp = path.with_name(path.name + f".tmp{os.getpid()}")
    tmp.write_bytes(data)
    os.replace(tmp, path)


def rows(bundle: Path):
    with gzip.open(bundle / "items.jsonl.gz", "rt") as f:
        for line in f:
            yield json.loads(line)


def fetch(session, url, dest: Path, cap: int):
    try:
        with session.get(https(url), stream=True, timeout=(15, 120)) as r:
            if r.status_code != 200:
                return f"http {r.status_code}"
            n = 0
            with open(dest, "wb") as f:
                for chunk in r.iter_content(1 << 20):
                    n += len(chunk)
                    if n > cap:
                        return "too-large"
                    f.write(chunk)
        return None
    except Exception as e:  # network errors become a skipped item, not a failed bundle
        return f"error {type(e).__name__}"


def check_image(path: Path):
    from PIL import Image
    Image.MAX_IMAGE_PIXELS = 400_000_000
    try:
        with Image.open(path) as im:
            im.load()
            if im.width < 8 or im.height < 8:
                return "tiny"
        return None
    except Exception as e:
        return f"decode {type(e).__name__}"


def check_stream(path: Path, mod: str):
    """Skip media without the stream the runner needs (e.g. an .avi with only audio)."""
    sel = "v:0" if mod == "video" else "a:0"
    try:
        p = subprocess.run([os.environ.get("FLASHEMBED_FFPROBE", "ffprobe"), "-v", "error", "-select_streams", sel,
                            "-show_entries", "stream=codec_type", "-of", "csv=p=0", str(path)],
                           capture_output=True, text=True, timeout=60)
        return None if p.stdout.strip() else f"no-{mod}-stream"
    except Exception as e:
        return f"probe {type(e).__name__}"


def write_units(bundle: Path, vectors: Path):
    out = vectors / "units" / f"{bundle.name}.jsonl.gz"
    if out.exists():
        return
    out.parent.mkdir(parents=True, exist_ok=True)
    tmp = out.with_name(out.name + f".tmp{os.getpid()}")
    with gzip.open(tmp, "wt") as f:
        for r in rows(bundle):
            k = r.get("kind")
            if k == "audit":
                continue
            keep = {"unit_id": r.get("unit_id"), "article_id": r.get("article_id"), "kind": k}
            if k == "article":
                keep.update(title=r.get("title"), license=r.get("license"))
            elif k == "figure":
                keep.update(caption=r.get("caption"), figure_id=(r.get("locator") or {}).get("figure_id"))
            elif k == "caption":
                keep.update(figure_id=(r.get("locator") or {}).get("figure_id"))
            elif k == "media":
                keep.update(modality=r.get("modality"), role=r.get("role"))
            elif k in TEXT_KINDS:
                keep.update(section=((r.get("locator") or {}).get("section") or [None])[0])
            f.write(json.dumps(keep) + "\n")
    os.replace(tmp, out)


def build_inputs(bundle: Path, model: str, work: Path, http_workers: int, only=None, av=True, text_only=False):
    texts, media, late = [], {"image": [], "video": [], "audio": []}, []
    for r in rows(bundle):
        if only is not None and not ((r.get("kind") == "media" and r.get("modality") in only)):
            continue
        k = r.get("kind")
        if k == "late_doc":  # late chunking: whole prompted document + char spans, one vector per span
            late.append({"id": r["unit_id"], "text": r["text"], "prefix_chars": r.get("prefix_chars", 0), "spans": r["spans"]})
        elif k in TEXT_KINDS and (r.get("text") or "").strip():
            t = r["text"]
            et = r.get("embed_text")  # experiments pass the exact model input
            texts.append({"id": r["unit_id"], "text": et if et is not None else (f"title: none | text: {t}" if model == "gemma" else t)})
        elif text_only:
            continue
        elif k == "figure" and r.get("source_url"):
            media["image"].append((r["unit_id"], r["source_url"]))
        elif k == "media" and r.get("role") == "media_file" and r.get("modality") in ("video", "audio") and model == "gemma" and av:
            media[r["modality"]].append((r["unit_id"], r["source_url"]))
    skipped, jobs = {}, []
    s = requests.Session()
    s.mount("https://", requests.adapters.HTTPAdapter(pool_connections=http_workers, pool_maxsize=http_workers))
    with cf.ThreadPoolExecutor(http_workers) as ex:
        for mod, items in media.items():
            (work / mod).mkdir(parents=True, exist_ok=True)
            for uid, url in items:
                ext = Path(url.split("?")[0]).suffix.lower() or ".bin"
                dest = work / mod / f"{uid}{ext}"
                jobs.append((mod, uid, dest, ex.submit(fetch, s, url, dest, MEDIA_CAP[mod])))
        ok = {"image": [], "video": [], "audio": []}
        for mod, uid, dest, fut in jobs:
            err = fut.result()
            if err is None and mod == "image":
                err = check_image(dest)
            elif err is None:
                err = check_stream(dest, mod)
            if err:
                skipped[err] = skipped.get(err, 0) + 1
                dest.unlink(missing_ok=True)
            else:
                ok[mod].append({"id": uid, "path": f"{mod}/{dest.name}"})
    files = {}
    if texts:
        files["text"] = texts
    if late:
        files["text_late"] = late
    for mod, items in ok.items():
        if items:
            files[mod] = items
    for mod, items in files.items():
        with open(work / f"{mod}.jsonl", "w") as f:
            for it in items:
                f.write(json.dumps(it) + "\n")
    counts = {"planned": {"text": len(texts), **{m: len(v) for m, v in media.items()}}}
    return files, skipped, counts


def per_item(cmd, work: Path, mod: str, items):
    """Run the embed command once per item (video/audio fallback). Returns dense rows, ids, [(id, error)]."""
    dense, ids, bad = [], [], []
    for n, it in enumerate(items):
        one = work / f"{mod}-one-{n}.jsonl"
        one.write_text(json.dumps(it) + "\n")
        od = work / f"out-{mod}-one-{n}"
        c = list(cmd)
        c[c.index("--input") + 1], c[c.index("--out") + 1] = str(one), str(od)
        p = subprocess.run(c, capture_output=True, text=True)
        if p.returncode != 0:
            bad.append([it["id"], p.stderr.strip().splitlines()[-1][-300:] if p.stderr.strip() else f"rc={p.returncode}"])
            continue
        dense.append(np.load(od / "embeddings.dense.npy"))
        g = json.loads((od / "embeddings.ids.json").read_text())
        ids += g["ids"] if isinstance(g, dict) else g
    return (np.concatenate(dense) if dense else None), ids, bad


def needs_repair(meta: dict) -> bool:
    """Older workers recorded a whole-modality failure (one bad video sank the batch) as an `error` entry."""
    return any("error" in v for v in meta.get("modalities", {}).values())


def repair(a, bundle: Path, out: Path):
    """Re-embed only the failed video/audio items of an already embedded bundle and append them."""
    meta = json.loads((out / "meta.json").read_text())
    bad = [m for m, v in meta["modalities"].items() if "error" in v]
    work = Path(tempfile.mkdtemp(prefix=f"rep-{a.model}-", dir=os.environ.get("TMPDIR", "/tmp")))
    try:
        files, skipped, _ = build_inputs(bundle, a.model, work, a.http_workers, only=set(bad))
        dense = np.load(out / "dense.npy")
        ids = json.loads((out / "ids.json").read_text())
        for mod in bad:
            t0 = time.time()
            items = files.get(mod, [])
            cmd = [a.bin, "embed", "--model", a.model, "--modality", mod, "--input", "x", "--out", "y",
                   "--onnx", a.onnx, "--snapshot", a.snapshot, "--batch", "1"] + (["--threads", str(a.threads)] if a.threads else [])
            d, got, failed = per_item(cmd, work, mod, items) if items else (None, [], [])
            if got:
                dense = np.concatenate([dense, d.astype(np.float16)]) if len(dense) else d.astype(np.float16)
                ids["unit_id"] += got
                ids["modality"] += [mod] * len(got)
            secs = time.time() - t0
            meta["modalities"][mod] = {"items": len(got), "seconds": round(secs, 2),
                                       "items_per_s": round(len(got) / max(secs, 1e-9), 3), "repaired": True,
                                       "failed_items": len(failed), "errors_sample": failed[:3]}
        for k, v in skipped.items():
            meta["skipped_media"][k] = meta["skipped_media"].get(k, 0) + v
        tmp = out.with_name(out.name + f".tmp{os.getpid()}")
        shutil.rmtree(tmp, ignore_errors=True)
        shutil.copytree(out, tmp)
        np.save(tmp / "dense.npy", dense)
        (tmp / "ids.json").write_text(json.dumps(ids))
        (tmp / "meta.json").write_text(json.dumps(meta))
        old = out.with_name(out.name + f".old{os.getpid()}")
        os.replace(out, old)
        os.replace(tmp, out)
        shutil.rmtree(old, ignore_errors=True)
        return meta
    finally:
        shutil.rmtree(work, ignore_errors=True)


def run_model(a, bundle: Path, out: Path, late: bool):
    t_start = time.time()
    work = Path(tempfile.mkdtemp(prefix=f"emb-{a.model}-", dir=os.environ.get("TMPDIR", "/tmp")))
    try:
        files, skipped, counts = build_inputs(bundle, a.model, work, a.http_workers, av=a.runner == "cpu",
                                              only=set(a.only.split(",")) if a.only else None)
        t_inputs = time.time() - t_start
        dense, ids, mods, lates, offs, per = [], [], [], [], [], {}
        for mod in ("text", "image", "video", "audio"):
            if mod not in files:
                continue
            od = work / f"out-{mod}"
            if a.runner == "gpu":  # flashembed-gpu: same inputs/outputs; text + images only; packs by token budget
                cmd = [a.bin, "embed", "--model", a.model, "--modality", mod, "--input", str(work / f"{mod}.jsonl"),
                       "--out", str(od), "--snapshot", a.snapshot, "--dtype", a.dtype]
            else:
                cmd = [a.bin, "embed", "--model", a.model, "--modality", mod, "--input", str(work / f"{mod}.jsonl"),
                       "--out", str(od), "--onnx", a.onnx, "--snapshot", a.snapshot, "--batch", str(a.batch)]
            if a.threads and a.runner == "cpu":
                cmd += ["--threads", str(a.threads)]
            if a.model == "neomme" and not late:
                cmd.append("--no-late")
            if mod == "text" and a.max_len:
                cmd += ["--max-len", str(a.max_len)]
            t0 = time.time()
            p = subprocess.run(cmd, capture_output=True, text=True)
            failed_items = []
            if p.returncode != 0:
                if mod not in ("video", "audio"):
                    raise RuntimeError(f"flashembed {mod} rc={p.returncode}: {p.stderr[-1500:]}")
                # one bad file fails the whole batch run: embed the items one at a time and keep the good ones
                d, got, failed_items = per_item(cmd, work, mod, files[mod])
                if not got:
                    per[mod] = {"items": 0, "failed_items": len(failed_items), "seconds": round(time.time() - t0, 2),
                                "items_per_s": 0.0, "errors_sample": failed_items[:3]}
                    continue
            else:
                d = np.load(od / "embeddings.dense.npy")
                got = json.loads((od / "embeddings.ids.json").read_text())
                got = got["ids"] if isinstance(got, dict) else got
            secs = time.time() - t0
            assert len(got) == len(d), (mod, len(got), d.shape)
            dense.append(d.astype(np.float16))
            ids += got
            mods += [mod] * len(got)
            info = {}
            if (od / "embeddings.json").exists():
                info = json.loads((od / "embeddings.json").read_text())
            per[mod] = {"items": len(got), "seconds": round(secs, 2), "items_per_s": round(len(got) / max(secs, 1e-9), 3),
                        **({"failed_items": len(failed_items), "errors_sample": failed_items[:3]} if failed_items else {}),
                        "runner": {k: info.get(k) for k in ("infer_s", "tokens", "peak_rss_mb", "policy", "threads", "cpu")
                                   if k in info}}
            if late and (od / "embeddings.late.npy").exists():
                lv = np.load(od / "embeddings.late.npy").astype(np.float16)
                o = np.load(od / "embeddings.late_offsets.npy").astype(np.int64)
                # offsets are item starts (N) or starts + end (N + 1); keep per-item lengths, rebuild at the end
                lens = np.diff(o) if len(o) == len(got) + 1 else np.diff(np.append(o, len(lv)))
                assert len(lens) == len(got) and lens.sum() == len(lv), (mod, len(o), len(got), len(lv))
                lates.append(lv)
                offs.append(lens)
            elif late:
                offs.append(np.zeros(len(got), np.int64))
        tmp = out.with_name(out.name + f".tmp{os.getpid()}")
        shutil.rmtree(tmp, ignore_errors=True)
        tmp.mkdir(parents=True)
        np.save(tmp / "dense.npy", np.concatenate(dense) if dense else np.zeros((0, 0), np.float16))
        (tmp / "ids.json").write_text(json.dumps({"unit_id": ids, "modality": mods}))
        if lates:
            np.save(tmp / "late.npy", np.concatenate(lates))
            np.save(tmp / "late_offsets.npy", np.concatenate([[0], np.cumsum(np.concatenate(offs))]).astype(np.int64))
        meta = {"bundle": bundle.name, "model": a.model, "runner": a.runner, "dtype": a.dtype if a.runner == "gpu" else "fp32",
                "host": socket.gethostname(), "threads": a.threads,
                "batch": a.batch, "late": bool(lates), "counts": counts, "skipped_media": skipped, "modalities": per,
                "seconds_inputs": round(t_inputs, 2), "seconds_total": round(time.time() - t_start, 2),
                "finished_at": time.time(), "manifest_sha256": hashlib.sha256((bundle / "manifest.json").read_bytes()).hexdigest()}
        (tmp / "meta.json").write_text(json.dumps(meta))
        shutil.rmtree(out, ignore_errors=True)
        os.replace(tmp, out)
        return meta
    finally:
        shutil.rmtree(work, ignore_errors=True)


# ---------------------------------------------------------------------------------------------------------------
# Serve mode: one long-lived runner process per model (`<bin> serve ...`, models loaded once), media downloaded and
# checked once per bundle for all models, and the next bundle's inputs prepared while the current one is embedded.
# Protocol (flashembed / flashembed-gpu `serve`): a `{"ready": true}` line, then one JSON request per stdin line
# {"id","modality","input","out"} -> exactly one JSON reply line {"id","ok",...}. Same output files as `embed`.
# ---------------------------------------------------------------------------------------------------------------
MODEL_MODALITIES = {"gemma": ("text", "text_late", "image", "video", "audio"), "neomme": ("text", "image")}
GEMMA_PREFIX = "title: none | text: "


class Server:
    def __init__(self, cmd, log: Path):
        self.cmd, self.log, self.p, self.n = cmd, log, None, 0
        self.start()

    def start(self):
        if self.p and self.p.poll() is None:
            self.p.kill()
        self.p = subprocess.Popen(self.cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=open(self.log, "a"),
                                  text=True, bufsize=1)
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise RuntimeError(f"serve exited before ready: {' '.join(self.cmd[:4])} (see {self.log})")
            try:
                if json.loads(line).get("ready"):
                    return
            except ValueError:
                continue

    def run(self, modality: str, inp: Path, out: Path) -> dict:
        self.n += 1
        rid = f"r{self.n}"
        try:
            self.p.stdin.write(json.dumps({"id": rid, "modality": modality, "input": str(inp), "out": str(out)}) + "\n")
            self.p.stdin.flush()
            while True:
                line = self.p.stdout.readline()
                if not line:
                    raise BrokenPipeError("server died")
                try:
                    r = json.loads(line)
                except ValueError:
                    continue
                if r.get("id") == rid:
                    return r
        except (BrokenPipeError, OSError) as e:
            self.start()  # restart once; the caller treats this request as failed
            return {"id": rid, "ok": False, "error": f"server died ({e}); restarted"}

    def close(self):
        if self.p and self.p.poll() is None:
            self.p.stdin.close()
            try:
                self.p.wait(timeout=60)
            except subprocess.TimeoutExpired:
                self.p.kill()


def prepare(bundle: Path, a, models):
    """Download/check media once; write per-model input files. Returns (work dir, {model: files}, skipped, counts)."""
    t0 = time.time()
    work = Path(tempfile.mkdtemp(prefix="emb-serve-", dir=os.environ.get("TMPDIR", "/tmp")))
    av = "gemma" in models and a.av
    files, skipped, counts = build_inputs(bundle, "gemma", work, a.http_workers, av=av,
                                          only=set(a.only.split(",")) if a.only else None, text_only=a.text_only)
    per = {}
    for m in models:
        f = {k: v for k, v in files.items() if k in MODEL_MODALITIES[m]}
        if m == "neomme" and "text" in f:  # NeoMME takes raw text; the runner adds <doc>
            raw = [{"id": t["id"], "text": t["text"][len(GEMMA_PREFIX):] if t["text"].startswith(GEMMA_PREFIX) else t["text"]}
                   for t in f["text"]]
            with open(work / "text-neomme.jsonl", "w") as fh:
                fh.writelines(json.dumps(x) + "\n" for x in raw)
            f["text"] = raw
        per[m] = f
    return work, per, skipped, counts, time.time() - t0


def write_shard(out: Path, dense, ids, mods, meta):
    tmp = out.with_name(out.name + f".tmp{os.getpid()}")
    shutil.rmtree(tmp, ignore_errors=True)
    tmp.mkdir(parents=True)
    np.save(tmp / "dense.npy", np.concatenate(dense) if dense else np.zeros((0, 0), np.float16))
    (tmp / "ids.json").write_text(json.dumps({"unit_id": ids, "modality": mods}))
    (tmp / "meta.json").write_text(json.dumps(meta))
    shutil.rmtree(out, ignore_errors=True)
    os.replace(tmp, out)


def embed_prepared(a, bundle: Path, model: str, server: Server, work: Path, files: dict, skipped, counts, t_inputs):
    t_start = time.time()
    dense, ids, mods, per = [], [], [], {}
    for mod in ("text", "text_late", "image", "video", "audio"):
        if mod not in files:
            continue
        inp = work / (f"{mod}-neomme.jsonl" if model == "neomme" and mod == "text" else f"{mod}.jsonl")
        od = work / f"out-{model}-{mod}"
        t0 = time.time()
        r = server.run(mod, inp, od)
        failed = []
        if not r.get("ok"):
            if mod not in ("video", "audio"):
                raise RuntimeError(f"{model} {mod}: {r.get('error')}")
            got, d = [], []  # one bad file sank the request: retry item by item, keep the good ones
            for n, it in enumerate(files[mod]):
                one = work / f"{model}-{mod}-one-{n}.jsonl"
                one.write_text(json.dumps(it) + "\n")
                o1 = work / f"out-{model}-{mod}-one-{n}"
                r1 = server.run(mod, one, o1)
                if r1.get("ok"):
                    d.append(np.load(o1 / "embeddings.dense.npy"))
                    g = json.loads((o1 / "embeddings.ids.json").read_text())
                    got += g["ids"] if isinstance(g, dict) else g
                else:
                    failed.append([it["id"], str(r1.get("error"))[-300:]])
            dv = np.concatenate(d) if d else None
        else:
            dv = np.load(od / "embeddings.dense.npy")
            g = json.loads((od / "embeddings.ids.json").read_text())
            got = g["ids"] if isinstance(g, dict) else g
            failed = [[x.get("id"), str(x.get("error"))[-300:]] for x in (r.get("failed") or [])]  # per-item failures
        secs = time.time() - t0
        if got:
            dense.append(dv.astype(np.float16))
            ids += got
            mods += ["text" if mod == "text_late" else mod] * len(got)
        per[mod] = {"items": len(got), "seconds": round(secs, 2), "items_per_s": round(len(got) / max(secs, 1e-9), 3),
                    **({"failed_items": len(failed), "errors_sample": failed[:3]} if failed else {})}
    meta = {"bundle": bundle.name, "model": model, "runner": a.runner, "mode": "serve",
            "dtype": a.dtype if a.runner == "gpu" else "fp32", "host": socket.gethostname(), "threads": a.threads,
            "batch": a.batch, "late": False, "counts": counts, "skipped_media": skipped, "modalities": per,
            "seconds_inputs": round(t_inputs, 2), "seconds_total": round(time.time() - t_start, 2), "finished_at": time.time(),
            "manifest_sha256": hashlib.sha256((bundle / "manifest.json").read_bytes()).hexdigest()}
    write_shard(a.vectors / model / bundle.name, dense, ids, mods, meta)
    return meta


def serve_cmd(a, model: str, snapshot: str):
    want = {"text"} | (set() if a.text_only else {"image"}) | ({"video", "audio"} if a.av and not a.text_only else set()) \
        | ({"text_late"} if a.late else set())
    mods = ",".join(m for m in MODEL_MODALITIES[model] if m in want)
    cmd = [a.bin, "serve", "--model", model, "--snapshot", snapshot, "--modalities", mods]
    if a.runner == "gpu":
        cmd += ["--dtype", a.dtype]
    else:
        cmd += ["--onnx", a.onnx, "--batch", str(a.batch)] + (["--threads", str(a.threads)] if a.threads else [])
    if a.max_len:
        cmd += ["--max-len", str(a.max_len)]
    if model == "neomme":
        cmd.append("--no-late")
    return cmd


def main_serve(a):
    """Serve-mode worker: claims whole bundles (all requested models at once) and pipelines prepare/embed."""
    models = a.models.split(",")
    snaps = json.loads(Path(a.models_json).read_text())
    key = {"gemma": "google/embeddinggemma-2", "neomme": "Hcompany/NeoMME-260M-Retriever"}
    for m in models:
        (a.vectors / m).mkdir(parents=True, exist_ok=True)
    claims = a.vectors / "_claims"
    claims.mkdir(parents=True, exist_ok=True)
    logdir = a.vectors / "_serve-logs"
    logdir.mkdir(exist_ok=True)
    servers = {m: Server(serve_cmd(a, m, snaps[key[m]]["path"]), logdir / f"{m}-{socket.gethostname()}-{os.getpid()}.log")
               for m in models}

    def todo(b):
        return [m for m in models if not (a.vectors / m / b.name / "meta.json").exists()]

    def claim():
        for b in ready_bundles(a.root):
            if not todo(b) or (claims / f"{b.name}.failed").exists():
                continue
            lock = open(claims / f"{b.name}.lock", "a")
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except OSError:
                lock.close()
                continue
            if todo(b):
                return b, lock
            fcntl.flock(lock, fcntl.LOCK_UN)
            lock.close()
        return None

    pool = cf.ThreadPoolExecutor(1)
    done_n, idle = 0, 0
    nxt = claim()
    fut = pool.submit(prepare, nxt[0], a, todo(nxt[0])) if nxt else None
    try:
        while True:
            if not nxt:
                if a.until_file and a.until_file.exists() and not any(todo(b) for b in ready_bundles(a.root)):
                    return
                idle += 1
                time.sleep(min(60, 5 * idle))
                nxt = claim()
                fut = pool.submit(prepare, nxt[0], a, todo(nxt[0])) if nxt else None
                continue
            idle = 0
            (b, lock), cur_fut = nxt, fut
            nxt = claim()  # claim + prefetch the next bundle while this one is embedded
            fut = pool.submit(prepare, nxt[0], a, todo(nxt[0])) if nxt else None
            work = None
            try:
                work, per, skipped, counts, t_in = cur_fut.result()
                out = {}
                for m in todo(b):
                    meta = embed_prepared(a, b, m, servers[m], work, per[m], skipped, counts, t_in)
                    out[m] = {k: (v.get("items"), v.get("items_per_s")) for k, v in meta["modalities"].items()}
                write_units(b, a.vectors)
                print(json.dumps({"t": time.time(), "bundle": b.name, "inputs_s": round(t_in, 2), "models": out,
                                  "skipped": skipped}), flush=True)
                done_n += 1
            except Exception as e:
                print(json.dumps({"t": time.time(), "bundle": b.name, "error": str(e)[-2000:]}), flush=True)
                if (b / "manifest.json").exists():
                    (claims / f"{b.name}.failed").write_text(str(e)[-4000:])
            finally:
                if work:
                    shutil.rmtree(work, ignore_errors=True)
                fcntl.flock(lock, fcntl.LOCK_UN)
                lock.close()
            if a.max_bundles and done_n >= a.max_bundles:
                if nxt:  # release the prefetched claim
                    fut.result()
                    fcntl.flock(nxt[1], fcntl.LOCK_UN)
                return
    finally:
        pool.shutdown(wait=True)
        for s in servers.values():
            s.close()


def ready_bundles(root: Path):
    return sorted(p.parent for p in root.glob("runs/*/ready/*/manifest.json") if not p.parent.name.endswith(".partial"))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", type=Path, required=True)
    ap.add_argument("--vectors", type=Path, required=True)
    ap.add_argument("--model", choices=["gemma", "neomme"], help="per-invocation mode: the one model to embed")
    ap.add_argument("--bin", required=True)
    ap.add_argument("--onnx", help="CPU runner: exported ONNX graph dir")
    ap.add_argument("--runner", choices=["cpu", "gpu"], default="cpu", help="gpu = flashembed-gpu (text + images)")
    ap.add_argument("--dtype", default="bf16", help="GPU runner precision: bf16 | fp32")
    ap.add_argument("--only", default="", help="embed only these media modalities (e.g. video), for targeted re-embeds")
    ap.add_argument("--serve", action="store_true", help="long-lived `<bin> serve` per model; media fetched once; prefetch")
    ap.add_argument("--models", default="", help="serve mode: comma-separated models embedded together (gemma,neomme)")
    ap.add_argument("--models-json", help="serve mode: models.json with HF snapshot paths")
    ap.add_argument("--no-av", dest="av", action="store_false", help="skip video/audio (Gemma)")
    ap.add_argument("--text-only", action="store_true", help="serve mode: embed text rows only (skip figures/media)")
    ap.add_argument("--late", action="store_true", help="serve mode: also start the runner's text_late (late chunking) modality")
    ap.add_argument("--snapshot", help="per-invocation mode: HF snapshot dir")
    ap.add_argument("--threads", type=int)
    ap.add_argument("--batch", type=int, default=8)
    ap.add_argument("--max-len", type=int)
    ap.add_argument("--http-workers", type=int, default=16)
    ap.add_argument("--late-mod", type=int, default=0, help="NeoMME: keep late vectors for bundles with hash %% M == 0")
    ap.add_argument("--until-file", type=Path, help="exit when this exists and no bundle is left")
    ap.add_argument("--max-bundles", type=int, default=0)
    ap.add_argument("--log", type=Path)
    a = ap.parse_args()
    if a.serve:
        if not a.models or not a.models_json:
            ap.error("--serve needs --models and --models-json")
        if a.runner == "cpu" and not a.onnx:
            ap.error("--onnx is required for the CPU runner")
        return main_serve(a)
    if not a.model or not a.snapshot:
        ap.error("--model and --snapshot are required without --serve")
    if a.runner == "cpu" and not a.onnx:
        ap.error("--onnx is required for the CPU runner")
    mdir = a.vectors / a.model
    mdir.mkdir(parents=True, exist_ok=True)
    done_n, idle = 0, 0
    while True:
        did = False
        for b in ready_bundles(a.root):
            out = mdir / b.name
            if (out / "meta.json").exists():
                try:
                    if not needs_repair(json.loads((out / "meta.json").read_text())):
                        continue
                except (OSError, ValueError):
                    continue
            elif (mdir / f"{b.name}.failed").exists():
                continue
            lock = open(mdir / f"{b.name}.lock", "a")
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except OSError:
                lock.close()
                continue
            try:
                if not (b / "manifest.json").exists():
                    continue
                if (out / "meta.json").exists():
                    meta = json.loads((out / "meta.json").read_text())
                    if not needs_repair(meta):
                        continue
                    meta = repair(a, b, out)
                    (mdir / f"{b.name}.failed").unlink(missing_ok=True)
                else:
                    write_units(b, a.vectors)
                    late = a.model == "neomme" and a.late_mod > 0 and int(hashlib.sha256(b.name.encode()).hexdigest(), 16) % a.late_mod == 0
                    meta = run_model(a, b, out, late)
                line = json.dumps({"t": time.time(), "bundle": b.name, "total_s": meta.get("seconds_total"),
                                   "mods": {m: (v.get("items"), v.get("items_per_s")) for m, v in meta["modalities"].items()},
                                   "skipped": meta["skipped_media"]})
                print(line, flush=True)
                did, done_n = True, done_n + 1
            except Exception as e:
                print(json.dumps({"t": time.time(), "bundle": b.name, "error": str(e)[-2000:]}), flush=True)
                if (b / "manifest.json").exists() and not (out / "meta.json").exists():  # vanished bundles are not failures
                    (mdir / f"{b.name}.failed").write_text(str(e)[-4000:])
            finally:
                fcntl.flock(lock, fcntl.LOCK_UN)
                lock.close()
            if a.max_bundles and done_n >= a.max_bundles:
                return
            break  # rescan: the newest bundles and other workers' progress
        if not did:
            pending = [b for b in ready_bundles(a.root) if not (mdir / b.name / "meta.json").exists()
                       and not (mdir / f"{b.name}.failed").exists()]
            if a.until_file and a.until_file.exists() and not pending:
                return
            idle += 1
            time.sleep(min(60, 5 * idle))
        else:
            idle = 0


if __name__ == "__main__":
    sys.exit(main())
