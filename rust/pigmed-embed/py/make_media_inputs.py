"""Media inputs for the modality / file-type / size sweeps (needs `ffmpeg` on PATH).

Writes into --out (= $EMBED_ROOT/inputs):
  images-formats.jsonl   3 source images x {png, jpg, tiff, gif, webp, bmp}
  images-res-<R>.jsonl   4 page renders resized to longest side R in {512, 1024, 1536, 2048}
  media.jsonl            audio + video clips: real PMC supplementary media when found (CC licence recorded),
                         plus synthetic clips; each source transcoded to several containers/codecs and lengths
Row: {"id", "kind", "path", "format", "source", "seconds", ...}
"""

from __future__ import annotations

import argparse
import concurrent.futures as cf
import json
import shutil
import subprocess
import urllib.request
from pathlib import Path

from PIL import Image

BASE = "https://pmc-oa-opendata.s3.amazonaws.com/"
VIDEO_EXT = (".mp4", ".mov", ".avi", ".webm", ".m4v", ".mpg", ".mpeg", ".wmv")
AUDIO_EXT = (".wav", ".mp3", ".m4a", ".flac", ".ogg", ".aac")


def get(url, timeout=60):
    with urllib.request.urlopen(urllib.request.Request(url, headers={"User-Agent": "pigmed-embed-bench/0.1"}),
                                timeout=timeout) as r:
        return r.read()


def ff(*args):
    subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", *args], check=True)


def ffmpeg_encoders():
    out = subprocess.run(["ffmpeg", "-hide_banner", "-encoders"], capture_output=True, text=True).stdout
    return {l.split()[1] for l in out.splitlines() if len(l.split()) > 1 and l.startswith(" ")}


def av_transcode(src, dst, kind, codec, secs, rate=None):
    """Encode with PyAV (bundled codecs) when the cluster ffmpeg lacks the encoder (libx264, libvpx-vp9, mp3)."""
    import av

    with av.open(str(src)) as inp, av.open(str(dst), "w") as out:
        if kind == "video":
            ist = inp.streams.video[0]
            fps = ist.average_rate or 30
            ost = out.add_stream(codec, rate=fps)
            ost.width, ost.height, ost.pix_fmt = ist.codec_context.width, ist.codec_context.height, "yuv420p"
            for fr in inp.decode(video=0):
                if fr.time is not None and fr.time >= secs:
                    break
                for pk in ost.encode(fr.reformat(format="yuv420p")):
                    out.mux(pk)
        else:
            ost = out.add_stream(codec, rate=rate or 44100)
            for fr in inp.decode(audio=0):
                if fr.time is not None and fr.time >= secs:
                    break
                fr.pts = None
                for pk in ost.encode(fr):
                    out.mux(pk)
        for pk in ost.encode(None):
            out.mux(pk)


def probe(path):
    out = subprocess.run(["ffprobe", "-v", "error", "-show_entries", "format=duration:stream=codec_type,codec_name",
                          "-of", "json", str(path)], capture_output=True, text=True, check=True).stdout
    return json.loads(out)


def scan_pmc(start, count, max_video, max_audio, max_bytes):
    """Find supplementary audio/video in PMC OA metadata (anonymous HTTPS)."""
    def meta(i):
        try:
            return json.loads(get(f"{BASE}metadata/PMC{i}.1.json", timeout=20))
        except Exception:
            return None

    found = {"video": [], "audio": []}
    with cf.ThreadPoolExecutor(16) as ex:
        for m in ex.map(meta, range(start, start + count)):
            if not m:
                continue
            for u in m.get("media_urls") or []:
                key = u.split("?")[0]
                ext = Path(key).suffix.lower()
                kind = "video" if ext in VIDEO_EXT else "audio" if ext in AUDIO_EXT else None
                if kind and len(found[kind]) < (max_video if kind == "video" else max_audio):
                    found[kind].append({"pmcid": m["pmcid"], "license": m.get("license_code"), "url": key, "ext": ext})
    got = []
    for kind, lst in found.items():
        for f in lst:
            url = BASE + f["url"][len("s3://pmc-oa-opendata/"):]
            try:
                req = urllib.request.Request(url, method="HEAD", headers={"User-Agent": "pigmed-embed-bench/0.1"})
                size = int(urllib.request.urlopen(req, timeout=20).headers.get("Content-Length", 0))
                if size > max_bytes:
                    continue
                f["bytes"] = size
                f["data"] = get(url, timeout=300)
                f["kind"] = kind
                got.append(f)
            except Exception:
                continue
    return got


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--real", type=Path, default=None, help="jsonl from scan_media.py (real PMC audio/video)")
    ap.add_argument("--max-real", type=int, default=8, help="real sources per kind")
    ap.add_argument("--max-bytes", type=int, default=150_000_000)
    args = ap.parse_args()
    out = args.out
    assert shutil.which("ffmpeg") and shutil.which("ffprobe"), "ffmpeg/ffprobe not on PATH"
    (out / "media").mkdir(parents=True, exist_ok=True)
    (out / "images-fmt").mkdir(parents=True, exist_ok=True)

    # ---- images: formats
    rows = []
    for src in ("page00", "fig00", "fig05"):
        im = Image.open(out / "images" / f"{src}.png").convert("RGB")
        for fmt, ext, kw in [("PNG", "png", {}), ("JPEG", "jpg", {"quality": 95}), ("TIFF", "tiff", {}),
                             ("GIF", "gif", {}), ("WEBP", "webp", {"quality": 90}), ("BMP", "bmp", {})]:
            p = out / "images-fmt" / f"{src}.{ext}"
            im.save(p, fmt, **kw)
            rows.append({"id": f"{src}.{ext}", "kind": "image", "path": f"images-fmt/{src}.{ext}", "format": ext,
                         "source": src})
    with open(out / "images-formats.jsonl", "w") as f:
        f.writelines(json.dumps(r) + "\n" for r in rows)
    # ---- images: real PMC figure files exactly as published (JPEG/PNG/GIF/TIFF ...), up to 4 per type
    if args.real:
        (out / "images-native").mkdir(exist_ok=True)
        nat, per = [], {}
        for c in [json.loads(l) for l in open(args.real)]:
            if c["kind"] != "image" or per.get(c["ext"], 0) >= 4:
                continue
            try:
                data = get(BASE + c["url"][len("s3://pmc-oa-opendata/"):], timeout=120)
                if not 2_000 < len(data) <= 25_000_000:
                    continue
                p = out / "images-native" / f"{c['pmcid']}-{Path(c['url']).name}"
                p.write_bytes(data)
                with Image.open(p) as im:
                    w, h, mode = im.width, im.height, im.mode
            except Exception:
                continue
            per[c["ext"]] = per.get(c["ext"], 0) + 1
            nat.append({"id": p.name, "kind": "image", "path": f"images-native/{p.name}",
                        "format": "native" + c["ext"].lstrip(".").replace("jpeg", "jpg").replace("tiff", "tif"),
                        "width": w, "height": h, "mode": mode, "source": f"{c['pmcid']} {c['url']} ({c.get('license')})",
                        "real": True})
        with open(out / "images-native.jsonl", "w") as f:
            f.writelines(json.dumps(r) + "\n" for r in nat)
        print("native images:", per)

    # ---- images: resolution sweep (page renders, LANCZOS down from 2048)
    for R in (512, 1024, 1536, 2048):
        (out / f"images-res{R}").mkdir(exist_ok=True)
        rr = []
        for k in range(4):
            im = Image.open(out / "images" / f"page{k:02d}.png").convert("RGB")
            s = R / max(im.size)
            if s != 1:
                im = im.resize((round(im.width * s), round(im.height * s)), Image.Resampling.LANCZOS)
            p = out / f"images-res{R}" / f"page{k:02d}.png"
            im.save(p)
            rr.append({"id": f"page{k:02d}@{R}", "kind": "image", "path": str(p.relative_to(out)), "format": "png",
                       "width": im.width, "height": im.height})
        with open(out / f"images-res-{R}.jsonl", "w") as f:
            f.writelines(json.dumps(r) + "\n" for r in rr)

    # ---- audio / video sources: real PMC supplementary media + synthetic
    srcdir = out / "media" / "src"
    srcdir.mkdir(parents=True, exist_ok=True)
    sources = []
    cands = [json.loads(l) for l in open(args.real)] if args.real else []
    rank = {"CC-BY": 0, "CC0": 0, "CC BY": 0}
    cands.sort(key=lambda c: (rank.get(c.get("license") or "", 1), c["kind"], c["ext"], c["pmcid"]))
    for kind in ("video", "audio"):
        picked, seen_pmc, seen_ext = 0, set(), {}
        for c in [c for c in cands if c["kind"] == kind]:
            if picked >= args.max_real or c["pmcid"] in seen_pmc or seen_ext.get(c["ext"], 0) >= 3:
                continue
            url = BASE + c["url"][len("s3://pmc-oa-opendata/"):]
            try:
                req = urllib.request.Request(url, method="HEAD", headers={"User-Agent": "pigmed-embed-bench/0.1"})
                size = int(urllib.request.urlopen(req, timeout=20).headers.get("Content-Length", 0))
                if not 10_000 < size <= args.max_bytes:
                    continue
                p = srcdir / f"{c['pmcid']}-{Path(c['url']).name}"
                p.write_bytes(get(url, timeout=600))
                info = probe(p)
                if kind not in {st["codec_type"] for st in info.get("streams", [])}:
                    continue
            except Exception as e:
                print("skip", c["url"], type(e).__name__)
                continue
            sources.append({"kind": kind, "path": p, "real": True, "duration": float(info["format"].get("duration", 0)),
                            "source": f"{c['pmcid']} {c['url']} ({c.get('license')}, {c['origin']})"})
            seen_pmc.add(c["pmcid"])
            seen_ext[c["ext"]] = seen_ext.get(c["ext"], 0) + 1
            picked += 1
        print(f"real {kind}: {picked}", flush=True)
    enc = ffmpeg_encoders()
    syn_v = srcdir / "synthetic-testsrc2.avi"  # lossless-ish intermediate with a codec every ffmpeg has
    ff("-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30:duration=45", "-c:v", "mjpeg", "-q:v", "2", str(syn_v))
    sources.append({"kind": "video", "path": syn_v, "source": "synthetic ffmpeg testsrc2 1280x720@30", "duration": 45.0,
                    "real": False})
    syn_a = srcdir / "synthetic-chirp.wav"
    ff("-f", "lavfi", "-i", "aevalsrc=0.3*sin(2*PI*(200+150*t)*t)+0.05*random(0):s=44100:d=45", "-ac", "2", str(syn_a))
    sources.append({"kind": "audio", "path": syn_a, "source": "synthetic chirp+noise 44.1k stereo", "duration": 45.0,
                    "real": False})

    media = []
    nreal = {"video": 0, "audio": 0}
    for si, s in enumerate(sources):
        tag = f"{s['kind'][0]}{si}"
        full = not s["real"] or nreal[s["kind"]] < 2
        if s["real"]:
            nreal[s["kind"]] += 1
            # the real file exactly as published (native container/codec/length)
            ext0 = s["path"].suffix.lower().lstrip(".")
            dst0 = out / "media" / f"{tag}-native.{ext0}"
            shutil.copyfile(s["path"], dst0)
            media.append({"id": dst0.name, "kind": s["kind"], "path": f"media/{dst0.name}", "format": f"native{ext0}",
                          "seconds": round(s["duration"], 1), "source": s["source"], "real": True,
                          "codecs": sorted({st.get("codec_name") for st in probe(dst0).get("streams", [])}),
                          "encoded_with": "original"})
        if s["kind"] == "video":
            # container/codec matrix at 10 s, length sweep in mp4
            h264 = ["-c:v", "libx264", "-pix_fmt", "yuv420p"]
            variants = [("mp4", h264, 10), ("mov", h264, 10), ("avi", ["-c:v", "mpeg4", "-q:v", "3"], 10),
                        ("webm", ["-c:v", "libvpx-vp9", "-b:v", "1M"], 10), ("mp4", h264, 4), ("mp4", h264, 20),
                        ("mp4", h264, 40)]
            if not full:
                variants = [variants[0]]
        else:
            variants = [("wav", ["-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le"], 10),
                        ("wav", ["-ar", "44100", "-ac", "2", "-c:a", "pcm_s16le"], 10),
                        ("mp3", ["-ar", "44100", "-c:a", "libmp3lame", "-b:a", "128k"], 10),
                        ("opus", ["-ar", "48000", "-c:a", "opus", "-strict", "-2"], 10),
                        ("flac", ["-ar", "16000", "-ac", "1", "-c:a", "flac"], 10),
                        ("ogg", ["-ar", "48000", "-c:a", "vorbis", "-strict", "-2", "-ac", "2"], 10), ("m4a", ["-ar", "44100", "-c:a", "aac"], 10),
                        ("wav", ["-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le"], 4),
                        ("wav", ["-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le"], 25),
                        ("wav", ["-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le"], 40)]
            if not full:
                variants = [variants[0]]
        for ext, codec, secs in variants:
            secs = min(secs, s["duration"]) if s["duration"] else secs
            hz = "-16k" if "16000" in codec else ""
            name = f"{tag}-{int(secs)}s{hz}.{ext}"
            dst = out / "media" / name
            extra = ["-an"] if s["kind"] == "video" else ["-vn"]
            cname = codec[codec.index("-c:v" if s["kind"] == "video" else "-c:a") + 1]
            how = "ffmpeg"
            try:
                if cname in enc or cname in ("opus", "vorbis"):
                    ff("-i", str(s["path"]), "-t", f"{secs}", *codec, *extra, str(dst))
                else:  # encoder missing from this ffmpeg build: PyAV's bundled one
                    alt = {"libx264": "libx264", "libvpx-vp9": "libvpx-vp9", "libmp3lame": "mp3"}[cname]
                    av_transcode(s["path"], dst, s["kind"], alt, secs, 44100)
                    how = "pyav"
            except Exception as e:
                print("skip", name, type(e).__name__, str(e)[:200])
                continue
            codecs = sorted({st.get("codec_name") for st in probe(dst).get("streams", [])})
            media.append({"id": name, "kind": s["kind"], "path": f"media/{name}", "format": ext + hz,
                          "seconds": secs, "source": s["source"], "codecs": codecs, "encoded_with": how,
                          "real": s["real"]})
    with open(out / "media.jsonl", "w") as f:
        f.writelines(json.dumps(r) + "\n" for r in media)
    print(f"images-formats={len(rows)} media={len(media)} (real={sum(m['real'] for m in media)})")
    for s_ in sources:
        print("  source:", s_["kind"], s_["source"], f"{s_['duration']:.1f}s")


if __name__ == "__main__":
    main()
