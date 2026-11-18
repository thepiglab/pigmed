from __future__ import annotations
import hashlib
import importlib.metadata
from dataclasses import dataclass
from pathlib import Path
import numpy as np
from .common import stable_id

MODELS = {"neomme": "Hcompany/NeoMME-260M-Retriever", "gemma2": "google/embeddinggemma-2"}


def normalized(a: np.ndarray) -> np.ndarray:
    a = np.asarray(a, dtype=np.float32)
    if not np.isfinite(a).all():
        raise ValueError("Non-finite embedding: check inference dtype and preprocessing")
    norm = np.linalg.norm(a, axis=-1, keepdims=True)
    if np.any(norm < 1e-12):
        raise ValueError("Zero embedding")
    return a / norm


@dataclass
class Encoded:
    dense: np.ndarray
    late: np.ndarray | None = None


class MockEncoder:
    """Deterministic plumbing test ONLY; these vectors have no retrieval quality."""
    tokenizer = None
    name = "MOCK-NOT-A-REAL-EMBEDDING"
    revision = "offline-fixture"
    supported = {"text", "caption", "table_text", "page", "figure", "audio", "video"}

    def __init__(self, dimension=32):
        self.dimension = dimension

    @property
    def identity(self):
        return {"backend": "mock", "model": self.name, "revision": self.revision, "dimension": self.dimension}

    def encode(self, units, task="document"):
        result = []
        for unit in units:
            key = str(unit.get("text", ""))
            if unit.get("media_path"):
                key += hashlib.sha256(Path(unit["media_path"]).read_bytes()).hexdigest()
            seed = int(stable_id(key), 16) % (2**32)
            rng = np.random.default_rng(seed)
            result.append(Encoded(normalized(rng.normal(size=self.dimension)),
                                  normalized(rng.normal(size=(4, 128)))))
        return result


class NeoEncoder:
    supported = {"text", "caption", "table_text", "page", "figure"}

    def __init__(self, model_id: str, revision: str, device: str = "cpu", dimension: int | None = None):
        import torch
        import transformers
        if not hasattr(transformers, "NeoMMEForRetrieval"):
            raise RuntimeError("Installed Transformers lacks NeoMMEForRetrieval. Upgrade and run `pigmed doctor`.")
        self.torch, self.name, self.revision, self.device = torch, model_id, revision, device
        self.processor = transformers.NeoMMEProcessor.from_pretrained(model_id, revision=revision)
        dtype = torch.bfloat16 if device.startswith("cuda") and torch.cuda.is_bf16_supported() else torch.float32
        self.model = transformers.NeoMMEForRetrieval.from_pretrained(
            model_id, revision=revision, torch_dtype=dtype, attn_implementation="sdpa").to(device).eval()
        self.tokenizer = self.processor.tokenizer
        self.dimension = dimension

    @property
    def identity(self):
        return {"backend": "neomme", "model": self.name, "revision": self.revision,
                "dimension": self.dimension, "preprocess": "official-chat-template-v1", "late_scoring": "mean_maxsim"}

    def inputs(self, units, task):
        from PIL import Image
        messages, opened = [], []
        try:
            for unit in units:
                content = []
                if unit.get("media_path"):
                    im = Image.open(unit["media_path"]).convert("RGB")
                    opened.append(im)
                    content.append({"type": "image", "image": im})
                if unit.get("text"):
                    content.append({"type": "text", "text": unit["text"]})
                messages.append([{"role": "user", "content": content}])
            inputs = self.processor.apply_chat_template(messages, task=task, tokenize=True,
                        return_dict=True, return_tensors="pt", processor_kwargs={"padding": "longest"})
            if inputs["attention_mask"].shape[-1] > 16384:
                raise ValueError("NeoMME context exceeded; rechunk instead of silently truncating")
            return inputs.to(self.device)
        finally:
            for im in opened:
                im.close()

    def encode(self, units, task="document"):
        inputs = self.inputs(units, task)
        with self.torch.inference_mode():
            outputs = self.model(**inputs)
        dense = outputs.dense_embeddings.float().cpu().numpy()
        if self.dimension is not None:
            if self.dimension not in {128, 256, 512, 1024, 1792} or self.dimension > dense.shape[-1]:
                raise ValueError("Unsupported NeoMME Matryoshka dimension")
            dense = dense[:, :self.dimension]
        dense = normalized(dense)
        late = outputs.embeddings.float().cpu().numpy()
        mask = inputs["attention_mask"].bool().cpu().numpy()
        return [Encoded(dense[i], normalized(late[i][mask[i]])) for i in range(len(units))]


class GemmaEncoder:
    supported = {"text", "caption", "table_text", "page", "figure", "audio", "video"}

    def __init__(self, model_id: str, revision: str, device="cpu", dimension: int | None = None,
                 modalities="vision"):
        import torch
        from sentence_transformers import SentenceTransformer
        self.name, self.revision, self.device = model_id, revision, device
        self.dimension, self.modalities = dimension or 768, modalities
        if self.dimension not in {128, 256, 512, 768}:
            raise ValueError("EmbeddingGemma 2 dimensions: 128, 256, 512, 768")
        config = {"vision_config": None, "audio_config": None} if modalities == "text" else {"audio_config": None} if modalities == "vision" else {}
        dtype = torch.bfloat16 if device.startswith("cuda") and torch.cuda.is_bf16_supported() else torch.float32
        self.model = SentenceTransformer(model_id, revision=revision, device=device,
                         config_kwargs=config, model_kwargs={"torch_dtype": dtype})
        self.tokenizer = self.model.tokenizer
        if modalities == "text":
            self.supported = {"text", "caption", "table_text"}
        elif modalities == "vision":
            self.supported = {"text", "caption", "table_text", "page", "figure", "video"}

    @property
    def identity(self):
        return {"backend": "gemma2", "model": self.name, "revision": self.revision,
                "dimension": self.dimension, "modalities": self.modalities,
                "preprocess": "model-card-task-prefixes-v1", "vision_budget": "processor-default"}

    def encode(self, units, task="document"):
        prepared = []
        for u in units:
            if u["kind"] not in self.supported:
                raise ValueError(f"Encoder not loaded for {u['kind']}")
            text = u.get("text", "")
            path = u.get("media_path")
            if path:
                modality = u["kind"] if u["kind"] in {"audio", "video"} else "image"
                # Native multimodal inputs; do not prefix an image path as text.
                item = {modality: str(path)}
                if text:
                    item["text"] = text + " " + f"<|{modality}|>"
                prepared.append(item)
            else:
                text = ("task: search result | query: " + text) if task == "query" else (
                    "title: " + (u.get("title") or "none") + " | text: " + text)
                # Ensure library defaults cannot silently truncate long text.
                if len(self.tokenizer(text, add_special_tokens=True, truncation=False)["input_ids"]) > 8192:
                    raise ValueError("EmbeddingGemma 2 context exceeded")
                prepared.append(text)
        vectors = self.model.encode(prepared, batch_size=len(prepared), normalize_embeddings=True,
                                    truncate_dim=self.dimension, convert_to_numpy=True, show_progress_bar=False)
        return [Encoded(x) for x in normalized(vectors)]


def build_encoder(backend, model_id=None, revision=None, device="cpu", dimension=None, modalities="vision"):
    if backend == "none":
        return None
    if backend == "mock":
        return MockEncoder(dimension or 32)
    import re
    if not revision or not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("Real inference requires a pinned 40-character model commit SHA; use `pigmed lock-model`.")
    if backend == "neomme":
        return NeoEncoder(model_id or MODELS[backend], revision, device, dimension)
    if backend == "gemma2":
        return GemmaEncoder(model_id or MODELS[backend], revision, device, dimension, modalities)
    raise ValueError(backend)


def mean_maxsim(query: np.ndarray, document: np.ndarray, tile=256) -> float:
    q, d = normalized(query), normalized(document)
    best = np.full(len(q), -np.inf, dtype=np.float32)
    for start in range(0, len(d), tile):
        best = np.maximum(best, (q @ d[start:start+tile].T).max(axis=1))
    return float(best.mean())


def quantize_int8(vectors):
    x = normalized(vectors)
    scale = np.maximum(np.max(np.abs(x), axis=-1, keepdims=True) / 127, 1e-12)
    return np.clip(np.rint(x / scale), -127, 127).astype(np.int8), scale[:, 0].astype("<f4")
