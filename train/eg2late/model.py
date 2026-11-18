"""EmbeddingGemma 2 (text tower) with its dense head plus a 128-d late-interaction (ColBERT-style) head.

Dense head: EG2 as released. The backbone already projects every token to 768-d (`embedding_projection`), then
sentence-transformers mean-pools all tokens (prompt included) and L2-normalises. We reproduce exactly that.

Late head: a small per-token MLP on the same 768-d token outputs -> 128-d, L2-normalised, scored with MaxSim.
  y = W_lin x + W_down gelu(W_up x)        (no bias; W_down starts at zero, W_lin from an SVD of token outputs)
Only text tokens enter MaxSim: prompt tokens, special tokens and padding are masked out (they still shape the
context through attention). Scores are the MEAN over query tokens of the max over document tokens, so they live
in [-1, 1] like the dense cosine and the two heads can share temperatures and a KL alignment term.

S1/S2 train the text tower only. S3 (`load(..., vision=True)`) keeps the vision tower and its embedder
(`embed_vision`, the projection into the text space): image soft tokens are scattered into the input embeddings
exactly as transformers' EmbeddingGemma2Model does, then run through the same text backbone, so an image document
gets a dense vector (mean over all tokens) and late vectors (late head on its soft tokens). The vision tower is
frozen or LoRA-adapted (`lora.py`); `embed_vision` can train.

`export_snapshot` writes an HF snapshot that flashembed and sentence-transformers load unchanged (language_model.*
tensors replaced; vision_tower.* / embed_vision.* replaced too when they were trained, LoRA merged; audio copied
from the base) plus late_head.safetensors.
"""
from __future__ import annotations

import json
import os
import shutil
from pathlib import Path

import torch
import torch.nn as nn
import torch.nn.functional as F

QUERY_PROMPT = "task: search result | query: "
DOC_PROMPT = "title: none | text: "


class LateHead(nn.Module):
    def __init__(self, d_in: int = 768, d_hidden: int = 1536, d_out: int = 128):
        super().__init__()
        self.lin = nn.Linear(d_in, d_out, bias=False)
        self.up = nn.Linear(d_in, d_hidden, bias=False)
        self.down = nn.Linear(d_hidden, d_out, bias=False)
        nn.init.normal_(self.up.weight, std=d_in ** -0.5)
        nn.init.zeros_(self.down.weight)
        nn.init.orthogonal_(self.lin.weight)
        self.cfg = {"d_in": d_in, "d_hidden": d_hidden, "d_out": d_out}

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        return F.normalize(self.lin(x) + self.down(F.gelu(self.up(x))), dim=-1)

    @torch.no_grad()
    def init_from_tokens(self, toks: torch.Tensor):
        """W_lin <- top right-singular vectors of a sample of (uncentred) token outputs [n, d_in]."""
        _, _, vh = torch.linalg.svd(toks.float(), full_matrices=False)
        self.lin.weight.copy_(vh[: self.lin.out_features].to(self.lin.weight.dtype))


class EG2Dual(nn.Module):
    def __init__(self, backbone: nn.Module, late: LateHead, snapshot: Path):
        super().__init__()
        self.backbone = backbone
        self.late = late
        self.snapshot = Path(snapshot)

    @classmethod
    def load(cls, path: str | os.PathLike, base_snapshot: str | os.PathLike | None = None, attn: str = "sdpa",
             dtype=torch.float32, vision: bool = False):
        """path: an HF snapshot (base EG2 or one written by export_snapshot). A late_head.safetensors next to it is
        loaded if present; otherwise the late head is freshly initialised. vision=True keeps the vision tower."""
        from transformers import AutoModel
        path = Path(path)
        # the audio tower has no SDPA path, so load with the default and set the text tower's attention afterwards
        full = AutoModel.from_pretrained(path, dtype=dtype)
        backbone = full.language_model
        backbone.config._attn_implementation = attn
        mm = None
        if vision:
            if full.vision_tower is None:
                raise ValueError(f"{path} has no vision tower")
            mm = {"vision": full.vision_tower, "embed_vision": full.embed_vision,
                  "image_token_id": full.config.image_token_id, "pad_id": full.config.text_config.pad_token_id,
                  "k": full.config.vision_config.pooling_kernel_size}
        del full  # audio (and, unless vision=True, the vision tower) dropped
        lh = path / "late_head.safetensors"
        if lh.exists():
            from safetensors.torch import load_file
            cfg = json.loads((path / "late_head.json").read_text())
            late = LateHead(**cfg)
            late.load_state_dict(load_file(lh))
        else:
            late = LateHead(d_in=backbone.config.embedding_dim)
        m = cls(backbone, late, Path(base_snapshot) if base_snapshot else path)
        m.has_trained_late = lh.exists()
        if mm is not None:
            m.vision, m.embed_vision = mm["vision"], mm["embed_vision"]
            m.image_token_id, m.pad_id, m.pool_k = mm["image_token_id"], mm["pad_id"], mm["k"]
        return m

    # ---- images (S3) ----
    def vision_mode(self, mode: str = "frozen", r: int = 16, alpha: float = 32.0, train_embedder: bool = True):
        """frozen: no vision-tower parameter trains; lora: only LoRA A/B on the tower's linears train; full: all.
        `embed_vision` (the projection into the text space) trains when train_embedder."""
        from .lora import apply_lora
        for q in self.vision.parameters():
            q.requires_grad_(mode == "full")
        hits = apply_lora(self.vision, r=r, alpha=alpha) if mode == "lora" else []
        for q in self.embed_vision.parameters():
            q.requires_grad_(train_embedder)
        return hits

    def image_features(self, pixel_values, image_position_ids):
        """Vision tower + embed_vision -> soft tokens [n_soft_total, hidden] in text-embedding space, the images'
        tokens concatenated in order (mirrors EmbeddingGemma2Model.get_image_features)."""
        vo = self.vision(pixel_values=pixel_values, pixel_position_ids=image_position_ids)
        return self.embed_vision(inputs_embeds=vo.last_hidden_state).reshape(-1, self.embed_vision.text_hidden_size)

    def embed_inputs(self, input_ids, pixel_values=None, image_position_ids=None):
        """Token embeddings with image placeholders replaced by the image soft tokens."""
        img = input_ids == self.image_token_id if hasattr(self, "image_token_id") else None
        ids = torch.where(img, self.pad_id, input_ids) if img is not None else input_ids
        emb = self.backbone.embed_tokens(ids)
        if pixel_values is not None:
            f = self.image_features(pixel_values, image_position_ids).to(emb.dtype)
            n = int(img.sum())
            if f.shape[0] != n:
                raise ValueError(f"{f.shape[0]} image soft tokens for {n} placeholders")
            emb = emb.masked_scatter(img.unsqueeze(-1).expand_as(emb), f)
        return emb

    def forward_mm(self, input_ids, attention_mask, late_mask, pixel_values=None, image_position_ids=None):
        """Padded batch that may contain images: same outputs as `forward`."""
        emb = self.embed_inputs(input_ids, pixel_values, image_position_ids)
        cfg = self.backbone.config
        impl = cfg._attn_implementation
        cfg._attn_implementation = "sdpa" if impl == "flex_attention" else impl
        try:
            h = self.backbone(inputs_embeds=emb, attention_mask=attention_mask).last_hidden_state
        finally:
            cfg._attn_implementation = impl
        m = attention_mask.unsqueeze(-1).to(h.dtype)
        dense = F.normalize((h * m).sum(1) / m.sum(1).clamp_min(1), dim=-1)
        late = self.late(h) * late_mask.unsqueeze(-1).to(h.dtype)
        return dense, late

    def _padded(self, input_ids, attention_mask):
        """Padded batches always run SDPA (flex is only used for packed rows; its padded path in transformers has
        no small-tile option and crashes on B200 / exceeds shared memory on RTX PRO 6000 at head_dim 512)."""
        cfg = self.backbone.config
        impl = cfg._attn_implementation
        cfg._attn_implementation = "sdpa" if impl == "flex_attention" else impl
        try:
            return self.backbone(input_ids=input_ids, attention_mask=attention_mask).last_hidden_state
        finally:
            cfg._attn_implementation = impl

    def forward(self, input_ids, attention_mask, late_mask):
        h = self._padded(input_ids, attention_mask)  # [B, L, 768]
        m = attention_mask.unsqueeze(-1).to(h.dtype)
        dense = F.normalize((h * m).sum(1) / m.sum(1).clamp_min(1), dim=-1)
        late = self.late(h) * late_mask.unsqueeze(-1).to(h.dtype)
        return dense, late

    def token_outputs(self, input_ids, attention_mask):
        return self._padded(input_ids, attention_mask)

    def forward_packed(self, p: dict):
        """Packed micro-batch (see `pack`): one row of T tokens holding many sequences, block-diagonal flex attention
        (+ the sliding window), positions restarting at 0 per sequence. Needs attn "flex_attention".
        Returns dense [n, 768] (mean over each sequence's tokens) and late [T, 128] (zeros off the late mask)."""
        masks = p.get("masks") or packed_block_masks(p["seg"], self.backbone.config.sliding_window)
        p["masks"] = masks  # reused by the GradCache re-encode of the same micro-batch
        ko = getattr(self, "flex_kernel_options", FLEX_SMALL)  # None = flex defaults (opt-in)
        kw = {"kernel_options": ko} if ko else {}
        h = self.backbone(input_ids=p["ids"][None], attention_mask=masks,
                          position_ids=p["pos"][None], **kw).last_hidden_state[0]  # [T, 768]
        n = p["n"]
        seg = p["seg"]
        real = seg < n
        sums = torch.zeros(n, h.shape[-1], device=h.device, dtype=torch.float32)
        sums.index_add_(0, seg[real], h[real].float())
        dense = F.normalize(sums / p["len"].unsqueeze(1).float(), dim=-1)
        late = self.late(h) * p["late_mask"].unsqueeze(-1).to(h.dtype)
        return dense, late


    @torch.no_grad()
    def export_snapshot(self, out: str | os.PathLike, extra_meta: dict | None = None):
        """Write a full HF snapshot: base files symlinked, model.safetensors = base with language_model.* replaced
        (bf16), late_head.safetensors (fp32) + late_head.json."""
        from safetensors import safe_open
        from safetensors.torch import save_file
        out = Path(out)
        tmp = out.with_name(out.name + ".tmp")
        if tmp.exists():
            shutil.rmtree(tmp)
        tmp.mkdir(parents=True)
        base = self.snapshot
        for p in base.iterdir():
            if p.name in ("model.safetensors", "late_head.safetensors", "late_head.json", "train_meta.json"):
                continue
            src = p.resolve()
            if p.is_dir():
                shutil.copytree(src, tmp / p.name)
            else:
                shutil.copy2(src, tmp / p.name)
        sd = {f"language_model.{k}": v.detach().to(torch.bfloat16).contiguous().cpu()
              for k, v in self.backbone.state_dict().items()}
        if getattr(self, "vision", None) is not None and getattr(self, "export_vision", True):
            from .lora import merged_state_dict
            for pre, mod in (("vision_tower", self.vision), ("embed_vision", self.embed_vision)):
                sd.update({f"{pre}.{k}": v.detach().to(torch.bfloat16).contiguous().cpu()
                           for k, v in merged_state_dict(mod).items()})
        tensors = {}
        with safe_open(base / "model.safetensors", "pt") as f:
            meta = f.metadata()
            names = list(f.keys())
            for k in names:
                if k.startswith("language_model."):
                    if k not in sd:
                        raise KeyError(f"trained backbone lacks {k}")
                    tensors[k] = sd[k]
                elif k in sd:  # vision tower / embedder (S3)
                    tensors[k] = sd[k].to(f.get_tensor(k).dtype)  # keep the base file's dtype
                else:
                    tensors[k] = f.get_tensor(k)
        missing = set(k for k in sd if k not in set(names))
        if missing:  # buffers etc. that the base checkpoint does not store
            pass
        save_file(tensors, tmp / "model.safetensors", metadata=meta or {"format": "pt"})
        save_file({k: v.detach().float().contiguous().cpu() for k, v in self.late.state_dict().items()},
                  tmp / "late_head.safetensors")
        (tmp / "late_head.json").write_text(json.dumps(self.late.cfg))
        (tmp / "train_meta.json").write_text(json.dumps(extra_meta or {}, indent=1))
        if out.exists():
            shutil.rmtree(out)
        tmp.rename(out)
        return out


# Smaller flex tiles: head_dim 512 (EG2's full-attention layers) can exceed ~100 KB of shared memory per SM on
# consumer Blackwell (RTX PRO 6000, sm_120) with the default tiles.
FLEX_SMALL = {"BLOCK_M": 32, "BLOCK_N": 32, "BLOCK_M1": 16, "BLOCK_N1": 32, "BLOCK_M2": 32, "BLOCK_N2": 16,
              "num_stages": 1, "num_warps": 4}


def packed_block_masks(seg: torch.Tensor, window: int):
    """Flex BlockMasks for a packed row: tokens attend only within their own sequence (seg id); sliding layers also
    need |q - k| <= window (transformers' bidirectional sliding rule). Padding is its own segment."""
    from torch.nn.attention.flex_attention import create_block_mask
    T = seg.shape[0]

    def doc(b, h, q, k):
        return seg[q] == seg[k]

    def slide(b, h, q, k):
        return (seg[q] == seg[k]) & ((q - k).abs() <= window)

    return {"full_attention": create_block_mask(doc, None, None, T, T, device=seg.device, _compile=True),
            "sliding_attention": create_block_mask(slide, None, None, T, T, device=seg.device, _compile=True)}


def pack(x: dict, T: int):
    """Padded tokenised batch {input_ids, attention_mask, late_mask} [N, L] -> list of packed micro-batches, each
    exactly T tokens (static shapes for compile/flex), sequences placed first-fit by decreasing length.
    Each item: ids/pos/seg/late_mask [T], n, len [n], rows [n] (indices into the N sequences), and
    tok_row/tok_col [T_real] mapping each real token back to (row, column) of the padded layout."""
    am = x["attention_mask"].bool()
    lens = am.sum(1).tolist()
    order = sorted(range(len(lens)), key=lambda i: -lens[i])
    bins: list[list[int]] = []
    room: list[int] = []
    for i in order:
        L = min(lens[i], T)
        for b in range(len(bins)):
            if room[b] >= L:
                bins[b].append(i)
                room[b] -= L
                break
        else:
            bins.append([i])
            room.append(T - L)
    out = []
    for rows in bins:
        ids = torch.zeros(T, dtype=torch.long)
        pos = torch.zeros(T, dtype=torch.long)
        lm = torch.zeros(T, dtype=torch.bool)
        seg = torch.full((T,), len(rows), dtype=torch.long)  # padding = segment n
        tok_row, tok_col, lens_b = [], [], []
        o = 0
        for j, r in enumerate(rows):
            cols = am[r].nonzero().squeeze(1)[:T]
            L = cols.numel()
            ids[o:o + L] = x["input_ids"][r, cols]
            lm[o:o + L] = x["late_mask"][r, cols]
            pos[o:o + L] = torch.arange(L)
            seg[o:o + L] = j
            tok_row.append(torch.full((L,), r, dtype=torch.long))
            tok_col.append(cols)
            lens_b.append(L)
            o += L
        out.append({"ids": ids, "pos": pos, "seg": seg, "late_mask": lm, "n": len(rows),
                    "len": torch.tensor(lens_b), "rows": torch.tensor(rows), "n_real": o,
                    "tok_row": torch.cat(tok_row), "tok_col": torch.cat(tok_col)})
    return out


class Tok:
    """Tokenise prompted texts; late_mask keeps tokens that cover characters after the prompt (no specials)."""

    def __init__(self, snapshot: str | os.PathLike):
        from transformers import AutoTokenizer
        self.tk = AutoTokenizer.from_pretrained(snapshot)

    def __call__(self, texts: list[str], prefix_chars: list[int] | int, max_len: int):
        if isinstance(prefix_chars, int):
            prefix_chars = [prefix_chars] * len(texts)
        enc = self.tk(texts, padding=True, truncation=True, max_length=max_len, return_offsets_mapping=True,
                      return_special_tokens_mask=True, return_tensors="pt")
        off = enc.pop("offset_mapping")
        spec = enc.pop("special_tokens_mask").bool()
        pc = torch.tensor(prefix_chars).unsqueeze(1)
        late_mask = (off[..., 1] > pc) & (off[..., 1] > off[..., 0]) & ~spec & enc["attention_mask"].bool()
        return {"input_ids": enc["input_ids"], "attention_mask": enc["attention_mask"], "late_mask": late_mask,
                "offsets": off}


def maxsim(q: torch.Tensor, qm: torch.Tensor, d: torch.Tensor, dm: torch.Tensor, chunk: int = 512) -> torch.Tensor:
    """q [B, Lq, D], qm [B, Lq] bool; d [N, Ld, D], dm [N, Ld] bool -> [B, N] mean-over-query-tokens MaxSim."""
    out = []
    qn = qm.sum(1).clamp_min(1).to(q.dtype)
    neg = -1e4  # finite in bf16; a masked-out doc token never wins the max (scores are in [-1, 1])
    for s in range(0, d.shape[0], chunk):
        sim = torch.einsum("bqd,nkd->bnqk", q, d[s:s + chunk])
        sim = sim.masked_fill(~dm[s:s + chunk][None, :, None, :], neg).amax(-1)  # [B, n, Lq]
        sim = (sim * qm[:, None, :].to(sim.dtype)).sum(-1) / qn[:, None]
        out.append(sim)
    return torch.cat(out, 1)
