from __future__ import annotations
import hashlib
import json
import random
import re
import time
from email.utils import parsedate_to_datetime
from pathlib import Path
from urllib.parse import parse_qs, quote, unquote, urlencode, urlsplit, urlunsplit
import requests

PMC_BUCKET = "pmc-oa-opendata"
PMC_BASE = f"https://{PMC_BUCKET}.s3.amazonaws.com/"
ALLOWED_HOSTS = {f"{PMC_BUCKET}.s3.amazonaws.com", f"{PMC_BUCKET}.s3.us-east-1.amazonaws.com",
                 "ftp.ncbi.nlm.nih.gov"}


def https_url(url: str) -> str:
    p = urlsplit(url)
    if p.scheme == "s3" and p.netloc == PMC_BUCKET:
        return urlunsplit(("https", f"{PMC_BUCKET}.s3.amazonaws.com", p.path, p.query, ""))
    if p.scheme != "https" or p.hostname not in ALLOWED_HOSTS:
        raise ValueError(f"Not an authorized bulk-source URL: {url!r}")
    return url


def expected_md5(url: str) -> str | None:
    value = parse_qs(urlsplit(url).query).get("md5", [None])[0]
    if value is not None and not re.fullmatch(r"[0-9a-fA-F]{32}", value):
        raise ValueError("Malformed source MD5")
    return value


def s3_url(key: str) -> str:
    # Keys from S3 inventory CSV are URL-encoded; callers decode exactly once.
    return PMC_BASE + quote(key, safe="/")


def retry_delay(value: str | None, attempt: int) -> float:
    if value:
        try:
            return min(600, max(0, float(value)))
        except ValueError:
            try:
                return min(600, max(0, parsedate_to_datetime(value).timestamp() - time.time()))
            except (ValueError, TypeError):
                pass
    return min(60, 2 ** attempt) + random.random()


class Http:
    """Bounded bulk HTTP. No E-utilities, proxies, IP rotation, or web scraping.

    One instance per process. Total concurrency is the array's concurrency, not
    an unbounded thread pool. Download failures restart an individual object;
    immutable shard checkpoints provide job-level resume.
    """
    def __init__(self, contact: str = "", retries: int = 5, interval: float = .1):
        self.session = requests.Session()
        self.session.headers["User-Agent"] = "PigMed/0.1" + (f" (mailto:{contact})" if contact else "")
        self.retries, self.interval, self.last = retries, interval, 0.0

    def get(self, url: str, *, stream: bool = False, max_bytes: int = 16 << 20):
        url = https_url(url)
        error = None
        for attempt in range(self.retries):
            time.sleep(max(0, self.last + self.interval - time.monotonic()))
            self.last = time.monotonic()
            try:
                r = self.session.get(url, stream=True, timeout=(15, 120), allow_redirects=False)
                if r.status_code in (429, 500, 502, 503, 504):
                    delay = retry_delay(r.headers.get("Retry-After"), attempt)
                    r.close()
                    time.sleep(delay)
                    continue
                if 300 <= r.status_code < 400:
                    r.close()
                    raise RuntimeError("Bulk source redirected; inspect the source adapter before continuing")
                r.raise_for_status()
                if stream:
                    return r
                data = bytearray()
                with r:
                    for part in r.iter_content(1 << 20):
                        data.extend(part)
                        if len(data) > max_bytes:
                            raise ValueError("Response exceeds configured memory bound")
                return bytes(data)
            except (requests.ConnectionError, requests.Timeout) as exc:
                error = exc
                time.sleep(retry_delay(None, attempt))
        raise RuntimeError(f"Retry budget exhausted for {url}: {error}")

    def json(self, url: str) -> dict:
        return json.loads(self.get(url))

    def download(self, url: str, target: Path, max_bytes: int,
                 md5: str | None = None) -> dict:
        url = https_url(url)
        target.parent.mkdir(parents=True, exist_ok=True)
        tmp = target.with_suffix(target.suffix + ".partial")
        md5 = md5 or expected_md5(url)
        error = None
        for attempt in range(self.retries):
            h, h256, size = hashlib.md5(), hashlib.sha256(), 0
            try:
                with self.get(url, stream=True) as r, tmp.open("wb") as out:
                    length = r.headers.get("Content-Length")
                    if length and int(length) > max_bytes:
                        raise ValueError(f"Object exceeds {max_bytes} bytes: {url}")
                    for part in r.iter_content(4 << 20):
                        size += len(part)
                        if size > max_bytes:
                            raise ValueError(f"Object exceeds {max_bytes} bytes: {url}")
                        out.write(part)
                        h.update(part)
                        h256.update(part)
                    if length and size != int(length):
                        raise IOError("Truncated HTTP body")
                if md5 and h.hexdigest().lower() != md5.lower():
                    raise IOError("Source MD5 mismatch")
                tmp.replace(target)
                return {"bytes": size, "sha256": h256.hexdigest(), "md5": h.hexdigest(), "url": url}
            except (requests.RequestException, IOError) as exc:
                error = exc
                tmp.unlink(missing_ok=True)
                time.sleep(retry_delay(None, attempt))
            except BaseException:
                tmp.unlink(missing_ok=True)
                raise
        raise RuntimeError(f"Download retry budget exhausted: {url}: {error}")

    def list_s3(self, prefix: str, delimiter: str | None = None):
        from lxml import etree
        token = None
        while True:
            params = {"list-type": "2", "prefix": prefix, "max-keys": "1000"}
            if delimiter:
                params["delimiter"] = delimiter
            if token:
                params["continuation-token"] = token
            xml = etree.fromstring(self.get(PMC_BASE + "?" + urlencode(params)),
                                   parser=etree.XMLParser(resolve_entities=False, no_network=True))
            ns = {"s": "http://s3.amazonaws.com/doc/2006-03-01/"}
            for obj in xml.findall("s:Contents", ns):
                yield {"key": obj.findtext("s:Key", namespaces=ns),
                       "etag": obj.findtext("s:ETag", namespaces=ns).strip('"')}
            for obj in xml.findall("s:CommonPrefixes", ns):
                yield {"prefix": obj.findtext("s:Prefix", namespaces=ns)}
            token = xml.findtext("s:NextContinuationToken", namespaces=ns)
            if not token:
                break
