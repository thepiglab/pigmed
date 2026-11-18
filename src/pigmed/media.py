from __future__ import annotations
import json
import subprocess
from pathlib import Path
from urllib.parse import urlsplit
from PIL import Image, ImageOps

VIDEO = {".mp4", ".avi", ".mov", ".flv", ".webm", ".m4v", ".mpg", ".mpeg", ".wmv"}
AUDIO = {".mp3", ".wav", ".flac", ".ogg", ".m4a", ".aac", ".aif", ".aiff"}
IMAGES = {".jpg", ".jpeg", ".png", ".tif", ".tiff", ".webp", ".gif"}


def media_kind(url: str) -> str:
    extension = Path(urlsplit(url).path).suffix.lower()
    return "video" if extension in VIDEO else "audio" if extension in AUDIO else "image" if extension in IMAGES else "other"


def image_view(source: Path, destination: Path, longest: int = 2048) -> dict:
    with Image.open(source) as im:
        im = ImageOps.exif_transpose(im).convert("RGB")
        original = list(im.size)
        im.thumbnail((longest, longest), Image.Resampling.LANCZOS)  # no upsampling
        im.save(destination, "PNG")
        return {
            "original_size": original, "rendered_size": list(im.size), "longest_side": longest}


def render_pdf(path: Path, out: Path, longest: int = 2048, max_pages: int = 200):
    import pypdfium2 as pdfium
    out.mkdir(parents=True, exist_ok=True)
    pdf = pdfium.PdfDocument(str(path))
    try:
        if len(pdf) > max_pages:
            raise ValueError(f"PDF has {len(pdf)} pages, exceeding configured max_pages={max_pages}; not silently truncated")
        for i in range(len(pdf)):
            page = pdf[i]
            try:
                width, height = page.get_size()
                bitmap = page.render(scale=longest / max(width, height))
                try:
                    im = bitmap.to_pil().convert("RGB")
                    dst = out / f"page-{i + 1:04d}.png"
                    im.save(dst, "PNG")
                finally:
                    bitmap.close()
                yield dst, {"page": i + 1, "page_size_pt": [width, height], "longest_side": longest}
            finally:
                page.close()
    finally:
        pdf.close()


def probe_media(path: Path) -> dict:
    proc = subprocess.run(["ffprobe", "-v", "error", "-show_format", "-show_streams", "-of", "json", str(path)],
                          check=True, capture_output=True, text=True, timeout=120)
    info = json.loads(proc.stdout)
    return {"duration": float(info.get("format", {}).get("duration", 0)),
            "has_audio": any(x.get("codec_type") == "audio" for x in info.get("streams", [])),
            "has_video": any(x.get("codec_type") == "video" for x in info.get("streams", []))}


def time_windows(duration: float, seconds: float, overlap: float):
    if seconds <= 0 or not 0 <= overlap < seconds:
        raise ValueError("Invalid clip length/overlap")
    start = 0.0
    while start < duration:
        end = min(duration, start + seconds)
        yield start, end
        if end == duration:
            break
        start = end - overlap


def make_clip(path: Path, destination: Path, start: float, end: float, kind: str):
    args = ["ffmpeg", "-nostdin", "-loglevel", "error", "-y", "-ss", str(start), "-i", str(path), "-t", str(end-start)]
    if kind == "audio":
        args += ["-vn", "-ac", "1", "-ar", "16000", "-c:a", "pcm_s16le"]
    else:
        # Video is visual-only here. Soundtracks are an explicit separate audio view.
        args += ["-an", "-vf", "scale='min(1280,iw)':-2", "-c:v", "libx264", "-preset", "fast", "-crf", "23"]
    subprocess.run(args + [str(destination)], check=True, timeout=600, capture_output=True)
