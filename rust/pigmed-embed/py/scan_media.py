"""Find REAL audio/video for the modality benchmarks.

1. Local PigMed bundles (assets.tar -> */article.json media_inventory with kind video/audio).
2. PMC open-access metadata (anonymous HTTPS) over --pmc-ranges, for audio/video extensions (audio is rare).
Writes --out (jsonl): {"pmcid", "license", "kind", "url", "ext", "origin"}, unique by url.
"""
import argparse, concurrent.futures as cf, json, tarfile, urllib.request
from pathlib import Path

BASE = "https://pmc-oa-opendata.s3.amazonaws.com/"
VIDEO = {".mp4", ".mov", ".avi", ".webm", ".m4v", ".mpg", ".mpeg", ".wmv", ".mkv", ".ogv"}
AUDIO = {".wav", ".mp3", ".m4a", ".flac", ".ogg", ".aac", ".wma", ".aif", ".aiff", ".oga"}
IMAGE = {".jpg", ".jpeg", ".png", ".gif", ".tif", ".tiff", ".bmp", ".webp"}


def kind_of(url):
    ext = Path(url.split("?")[0]).suffix.lower()
    return ("video" if ext in VIDEO else "audio" if ext in AUDIO else "image" if ext in IMAGE else None), ext


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tars", nargs="*", default=[])
    ap.add_argument("--pmc-ranges", nargs="*", default=[], help="start:count ...")
    ap.add_argument("--out", type=Path, required=True)
    a = ap.parse_args()
    found = {}
    for t in a.tars:
        try:
            with tarfile.open(t) as tf:
                for m in tf.getmembers():
                    if not m.name.endswith("/article.json"):
                        continue
                    d = json.load(tf.extractfile(m))
                    for x in d.get("media_inventory", []):
                        k, ext = kind_of(x["url"])
                        if k:
                            u = x["url"].split("?")[0]
                            found.setdefault(u, {"pmcid": d.get("pmcid"), "license": d.get("license"), "kind": k,
                                                 "url": u, "ext": ext, "origin": "pigmed-bundle"})
        except Exception as e:
            print("skip", t, e)
    print("from bundles:", sum(v["kind"] == "video" for v in found.values()), "video,",
          sum(v["kind"] == "audio" for v in found.values()), "audio", flush=True)

    def meta(i):
        try:
            req = urllib.request.Request(f"{BASE}metadata/PMC{i}.1.json", headers={"User-Agent": "pigmed-embed-bench/0.1"})
            return json.loads(urllib.request.urlopen(req, timeout=20).read())
        except Exception:
            return None

    for rng in a.pmc_ranges:
        s, n = map(int, rng.split(":"))
        with cf.ThreadPoolExecutor(64) as ex:
            for m in ex.map(meta, range(s, s + n)):
                if not m:
                    continue
                for u in m.get("media_urls") or []:
                    k, ext = kind_of(u)
                    if k:
                        u = u.split("?")[0]
                        found.setdefault(u, {"pmcid": m["pmcid"], "license": m.get("license_code"), "kind": k,
                                             "url": u, "ext": ext, "origin": "pmc-oa-metadata"})
        print(f"after PMC{s}+{n}:", sum(v["kind"] == "video" for v in found.values()), "video,",
              sum(v["kind"] == "audio" for v in found.values()), "audio", flush=True)
    a.out.write_text("".join(json.dumps(v) + "\n" for v in found.values()))
    from collections import Counter
    print(Counter((v["kind"], v["ext"], v["license"]) for v in found.values()).most_common(30))


if __name__ == "__main__":
    main()
