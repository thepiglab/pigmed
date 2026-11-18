from __future__ import annotations
import gzip
import re
from collections import OrderedDict
from pathlib import Path
from urllib.parse import unquote, urlsplit
from lxml import etree
from .common import normal_license, stable_id

XLINK = "{http://www.w3.org/1999/xlink}href"


def clean_text(el) -> str:
    return re.sub(r"\s+", " ", "".join(el.itertext())).strip() if el is not None else ""


def parse_jats(path: Path, metadata: dict) -> dict:
    parser = etree.XMLParser(resolve_entities=False, load_dtd=False, no_network=True,
                             recover=False, huge_tree=False)
    root = etree.parse(str(path), parser).getroot()
    # Handle default JATS namespaces without damaging xlink attributes.
    for el in root.iter():
        if isinstance(el.tag, str):
            el.tag = etree.QName(el).localname
    title = clean_text(root.find(".//article-title")) or metadata.get("title", "")
    pmcid = str(metadata.get("pmcid", ""))
    if not pmcid.startswith("PMC"):
        pmcid = "PMC" + pmcid
    version = int(metadata.get("version", 1))
    article_id = f"{pmcid}.{version}"
    blocks = []
    candidates = root.xpath("./front/article-meta/abstract//p | ./body//p | ./body//table-wrap | ./body//disp-formula")
    for i, el in enumerate(candidates):
        ancestors = list(el.iterancestors())
        if el.tag == "p" and any(a.tag in {"fig", "table-wrap", "supplementary-material", "disp-formula"} for a in ancestors):
            continue
        sections = [a for a in reversed(ancestors) if a.tag in {"sec", "abstract"}]
        path_names = [(clean_text(s.find("title")) or ("Abstract" if s.tag == "abstract" else "Untitled")) for s in sections]
        section_ids = [s.get("id") or root.getroottree().getpath(s) for s in sections]
        text = re.sub(r"\s+", " ", " ".join(el.itertext())).strip() if el.tag == "table-wrap" else clean_text(el)
        if not text:
            continue
        blocks.append({"block_id": el.get("id") or f"b{i}", "section": path_names,
                       "section_id": "/".join(section_ids), "kind": el.tag, "text": text,
                       "xml": etree.tostring(el, encoding="unicode", with_tail=False) if el.tag != "p" else None,
                       "references": [r.get("rid") for r in el.findall(".//xref") if r.get("rid")]})
    figures = []
    for i, fig in enumerate(root.findall(".//fig")):
        hrefs = [g.get(XLINK) or g.get("href") for g in fig.findall(".//graphic")]
        rights_text = " ".join(clean_text(x) for x in fig.xpath(".//permissions | .//copyright-statement | .//credit"))
        figures.append({"figure_id": fig.get("id") or f"fig{i}", "label": clean_text(fig.find("label")),
                        "caption": clean_text(fig.find("caption")), "hrefs": [h for h in hrefs if h],
                        "rights_notice": rights_text,
                        "rights_review_required": bool(rights_text)})
    return {"article_id": article_id, "pmcid": pmcid, "pmid": str(metadata.get("pmid") or ""),
            "doi": metadata.get("doi") or "", "version": version, "title": title,
            "license": normal_license(metadata.get("license_code")),
            "license_statement": clean_text(root.find(".//permissions")),
            "authors": [clean_text(x) for x in root.xpath("./front/article-meta/contrib-group/contrib/name")],
            "is_retracted": bool(metadata.get("is_retracted", False)),
            "publication_dates": [{"type": d.get("pub-type") or d.get("date-type"),
                                   "year": d.findtext("year"), "month": d.findtext("month"), "day": d.findtext("day")}
                                  for d in root.findall("./front/article-meta/pub-date")],
            "blocks": blocks, "figures": figures, "source_metadata": metadata}


def resolve_asset(href: str, urls: list[str]) -> str | None:
    """Match XML image references to actual inventory names; never guess an extension."""
    target = Path(unquote(urlsplit(href).path)).name
    exact = [u for u in urls if Path(unquote(urlsplit(u).path)).name == target]
    if len(exact) == 1:
        return exact[0]
    stem = Path(target).stem
    matches = [u for u in urls if Path(unquote(urlsplit(u).path)).stem == stem]
    return matches[0] if len(matches) == 1 else None


class TokenCounter:
    """Fast-tokenizer offsets, or an explicitly labeled word-budget fallback."""
    def __init__(self, tokenizer=None, name="regex-word-v1"):
        self.tokenizer, self.name = tokenizer, name

    def offsets(self, text: str) -> list[tuple[int, int]]:
        if self.tokenizer is None:
            return [(m.start(), m.end()) for m in re.finditer(r"\S+", text)]
        result = self.tokenizer(text, add_special_tokens=False, return_offsets_mapping=True,
                                truncation=False)
        return [(int(a), int(b)) for a, b in result["offset_mapping"] if b > a]


def split_text(text: str, counter: TokenCounter, budget: int = 512, overlap: int = 64):
    if budget < 1 or overlap < 0 or overlap >= budget:
        raise ValueError("Require 0 <= overlap < budget")
    tokens = counter.offsets(text)
    i = 0
    while i < len(tokens):
        end = min(i + budget, len(tokens))
        # Prefer a sentence/paragraph boundary in the last quarter of the window.
        if end < len(tokens):
            for j in range(end, max(i + overlap + 1, i + budget * 3 // 4), -1):
                suffix = text[tokens[j - 1][0]:tokens[j - 1][1]]
                if re.search(r"[.!?][\"')\]]?$", suffix):
                    end = j
                    break
        start_char, end_char = tokens[i][0], tokens[end - 1][1]
        yield text[start_char:end_char], start_char, end_char, end - i
        if end == len(tokens):
            break
        i = max(i + 1, end - overlap)


def text_units(article: dict, counter: TokenCounter, budget: int = 512, overlap: int = 64):
    groups = OrderedDict()
    for block in article["blocks"]:
        # Tables/equations remain distinct evidence objects instead of being split into prose.
        key = (block["section_id"], block["kind"] if block["kind"] != "p" else "prose",
               block["block_id"] if block["kind"] != "p" else "")
        groups.setdefault(key, []).append(block)
    for key, blocks in groups.items():
        text = "\n\n".join(b["text"] for b in blocks)
        locator = {"section_id": key[0], "section": blocks[0]["section"],
                   "block_ids": [b["block_id"] for b in blocks]}
        for part, start, end, count in split_text(text, counter, budget, overlap):
            yield {"unit_id": stable_id(article["article_id"], key, start, end, part,
                                         counter.name, budget, overlap),
                   "kind": "table_text" if key[1] == "table-wrap" else "text",
                   "text": part, "locator": {**locator, "char_start": start, "char_end": end},
                   "token_count": count, "tokenizer": counter.name,
                   "chunk_recipe": {"budget": budget, "overlap": overlap}, "media_path": None}
    for fig in article["figures"]:
        if fig["caption"]:
            for part, start, end, count in split_text(fig["caption"], counter, budget, overlap):
                yield {"unit_id": stable_id(article["article_id"], fig["figure_id"], "caption", start, part),
                       "kind": "caption", "text": part,
                       "locator": {"figure_id": fig["figure_id"], "char_start": start, "char_end": end},
                       "token_count": count, "tokenizer": counter.name, "media_path": None}


def iter_pubmed(path: Path, source: dict):
    """Streaming citation change log; revised/deleted rows retain numerical source order."""
    with gzip.open(path, "rb") as f:
        events = etree.iterparse(f, events=("end",), tag=("PubmedArticle", "PubmedBookArticle", "DeleteCitation"),
                                resolve_entities=False, load_dtd=False, no_network=True, huge_tree=False)
        for within, (_, el) in enumerate(events):
            common = {"source": "pubmed", "source_file": source["name"], "source_sequence": source["sequence"],
                      "phase": source["phase"], "within_file": within,
                      "license": "PUBMED-CITATION-TERMS", "export_approved": False}
            if el.tag == "DeleteCitation":
                for pmid in el.findall("PMID"):
                    yield {**common, "op": "delete", "pmid": pmid.text}
            else:
                pmid = el.findtext(".//PMID")
                if not pmid:
                    raise ValueError("PubMed record without PMID")
                ids = {x.get("IdType"): x.text for x in el.findall(".//ArticleId")}
                yield {**common, "op": "upsert", "pmid": pmid, "pmcid": ids.get("pmc"), "doi": ids.get("doi"),
                       "title": clean_text(el.find(".//ArticleTitle")),
                       "publication_dates": [{"year": d.findtext("Year"), "month": d.findtext("Month"),
                                              "day": d.findtext("Day"), "medline_date": d.findtext("MedlineDate")}
                                             for d in el.findall(".//PubDate")],
                       "abstract": [{"label": a.get("Label"), "text": clean_text(a)} for a in el.findall(".//AbstractText")],
                       "mesh": [clean_text(x) for x in el.findall(".//MeshHeading/DescriptorName")],
                       "publication_types": [clean_text(x) for x in el.findall(".//PublicationType")],
                       "citation_status": el.find("MedlineCitation").get("Status") if el.find("MedlineCitation") is not None else None,
                       "retraction_links": [{"type": x.get("RefType"), "pmid": x.findtext("PMID")}
                                            for x in el.findall(".//CommentsCorrections")]}
            el.clear()
            while el.getprevious() is not None:
                del el.getparent()[0]
