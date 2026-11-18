"""S3 figure data: a manifest-backed image store and the EG2 image preprocessing for training and eval.

Manifest (figures.jsonl, one row per figure; written by the data side):
    {"id": unit_id, "article_id", "group", "image": "<path relative to the manifest>", "caption", "mentions": [...],
     "licence"}
Images are resized RGB JPEGs (longest side <= 896 px): EG2's processor resizes to <= 280 soft tokens (patch 16,
pooling 3), so bigger files only cost I/O.

An image document is the text `image_doc_text` with EG2's image token in it (default: just the image, which is how
flashembed's image modality builds rows: <bos><|image> soft tokens <image|><eos>; check with the parity script before
relying on v2 image vectors as teacher). Its late mask = the soft-token positions (+ any text after the prompt).
"""
from __future__ import annotations

import json
import os
from pathlib import Path

import torch

IMAGE_DOC_TEXT = "{image}"


class FigureStore:
    """Same interface as train_s1.DocStore (ids, index, group, __len__) plus `image(i)` -> PIL RGB."""

    def __init__(self, root: Path, manifest: str = "figures.jsonl"):
        self.root = Path(root)
        self.ids, self.paths, groups, self.caption = [], [], [], []
        mp = self.root / manifest
        for line in open(mp):
            r = json.loads(line)
            self.ids.append(r["id"])
            self.paths.append(str((mp.parent / r["image"]).resolve()) if not os.path.isabs(r["image"]) else r["image"])
            groups.append(r.get("group"))
            self.caption.append(r.get("caption", ""))
        self.index = {x: i for i, x in enumerate(self.ids)}
        self.group = groups if any(groups) else None
        self.is_image = True

    def __len__(self):
        return len(self.ids)

    def image(self, i):
        from PIL import Image
        with Image.open(self.paths[i]) as im:
            return im.convert("RGB")

    def text(self, i):  # for code paths that sample doc text (e.g. the late-head SVD init): the caption
        return self.caption[i]


class ImageEncoderInputs:
    """EG2 processor wrapper: images (+ optional text template) -> padded model inputs with late_mask."""

    def __init__(self, snapshot, image_doc_text: str = IMAGE_DOC_TEXT, max_len: int = 512):
        from transformers import AutoProcessor
        self.proc = AutoProcessor.from_pretrained(snapshot)
        self.tok = self.proc.tokenizer
        self.image_token_id = self.proc.image_token_id
        self.template = image_doc_text
        self.max_len = max_len

    def __call__(self, images: list, texts: list[str] | None = None) -> dict:
        texts = texts or [""] * len(images)
        prompts = [self.template.format(image=self.tok.image_token, text=t) for t in texts]
        x = self.proc(text=prompts, images=[[im] for im in images], return_tensors="pt")
        ids = x["input_ids"]
        return {"input_ids": ids, "attention_mask": x["attention_mask"], "late_mask": ids == self.image_token_id,
                "pixel_values": x["pixel_values"], "image_position_ids": x["image_position_ids"]}


def slice_images(x: dict, s: int, e: int) -> dict:
    """Rows s:e of an image batch (pixel tensors are per image, one image per row)."""
    out = {k: v[s:e] for k, v in x.items() if k in ("input_ids", "attention_mask", "late_mask", "pixel_values",
                                                    "image_position_ids")}
    L = int(out["attention_mask"].sum(1).max())  # trim common right padding
    for k in ("input_ids", "attention_mask", "late_mask"):
        out[k] = out[k][:, :L]
    return out
