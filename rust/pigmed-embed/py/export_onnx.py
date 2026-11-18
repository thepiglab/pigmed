"""Export the PigMed embedding models to ONNX (FP32) for the Rust runner.

Graphs (all FP32, dynamic batch / sequence):
  gemma_lm.onnx      EmbeddingGemma 2 text backbone incl. 512->768 projection.
                     in : input_ids i64[B,S], attention_mask i64[B,S], mm_embeds f32[B,S,512], mm_mask bool[B,S]
                     out: last_hidden_state f32[B,S,768]   (Rust: masked mean pool + L2 norm, as ST modules.json)
                     Soft tokens from gemma_vision are placed at <|image|> positions via mm_mask (zeros for text).
  gemma_vision.onnx  Gemma 4 vision tower + spatial pooler + embed_vision, padding NOT stripped.
                     in : pixel_values f32[B,2520,768], image_position_ids i64[B,2520,2]
                     out: soft_tokens f32[B,280,512], soft_mask bool[B,280]
  neomme_text.onnx   NeoMMEForRetrieval, text documents.
                     in : input_ids, attention_mask       out: dense f32[B,1024], late f32[B,S,128]
  neomme_image.onnx  NeoMMEForRetrieval, document images. Patch rows are given per token position.
                     in : input_ids, attention_mask, position_ids i64[2,B,S], patches f32[B,S,3072], patch_mask bool[B,S]
                     out: dense, late

The HF forwards scatter features with data-dependent shapes (masked_scatter / boolean indexing), which ONNX
export handles poorly; the wrappers below use an equivalent `where` merge instead. Each wrapper is checked
against the untouched HF forward before export, and each ONNX graph against its wrapper after export.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import time
from pathlib import Path

import numpy as np
import torch
from torch import nn

torch.set_grad_enabled(False)


# ----------------------------------------------------------------------------------------------- fused attention
# `--attn pigmed`: every HF attention call goes through pigmed::flash_attention, exported as the custom ONNX node
# pigmed.ai::FlashAttention that the Rust runner implements (src/flash.rs). The torch implementation below is a
# dense reference used only for the wrapper-vs-HF checks; tracing uses the fake (shape-only) implementation.
KEY_MASK: dict = {}


@torch.library.custom_op("pigmed::flash_attention", mutates_args=())
def flash_attention(q: torch.Tensor, k: torch.Tensor, v: torch.Tensor, key_mask: torch.Tensor, scale: float,
                    window: int) -> torch.Tensor:
    b, h, s, _ = q.shape
    rep = h // k.shape[1]
    k2, v2 = k.repeat_interleave(rep, dim=1), v.repeat_interleave(rep, dim=1)
    sc = (q @ k2.transpose(-1, -2)).float() * scale
    allowed = key_mask.bool()[:, None, None, :]
    if window >= 0:
        i = torch.arange(s, device=q.device)
        allowed = allowed & ((i[:, None] - i[None, :]).abs() <= window)
    sc = sc.masked_fill(~allowed, torch.finfo(torch.float32).min)
    return (torch.softmax(sc, dim=-1).to(q.dtype) @ v2).transpose(1, 2).contiguous()


@flash_attention.register_fake
def _(q, k, v, key_mask, scale, window):
    return q.new_empty((q.shape[0], q.shape[2], q.shape[1], v.shape[-1]))


@torch.library.custom_op("pigmed::audio_chunk_attention", mutates_args=())
def audio_chunk_attention(q: torch.Tensor, k: torch.Tensor, v: torch.Tensor, relk: torch.Tensor, key_mask: torch.Tensor,
                          left: int, softcap: float, invalid: float) -> torch.Tensor:
    """Gemma 4 audio chunked local attention core (HF Gemma4AudioAttention between the scaled q/k/v and `post`).
    q,k,v [B,T,H,D] (q/k already scaled), relk [P,H,D] (P = left+1 relative positions, index p <-> distance left-p),
    key_mask [B,T] (1 = valid). Query q may attend keys j = q-d, d in [0, left), that are in range and valid; logits
    q.k_j + q.relk[left-d], soft-capped; masked context slots get `invalid` (a fully masked row -> uniform average over
    the 2*left context slots, as in HF). Returns [B,T,H*D]."""
    b, t, h, d = q.shape
    ctx = 2 * left  # chunk (left) + past horizon (left) + future 0
    out = torch.zeros(b, t, h, d, dtype=q.dtype)
    dist = torch.arange(ctx)  # slot o <-> key j = q - left + o - (q % chunk) ... handled below per HF blocking
    chunk = left
    for qi in range(0, t, chunk):
        blk = qi // chunk
        js = blk * chunk + torch.arange(ctx) - left            # context keys of this block
        inr = (js >= 0) & (js < t)
        jc = js.clamp(0, t - 1)
        kk = torch.where(inr[None, :, None, None], k[:, jc], 0.0)  # [B,ctx,H,D]
        vv = torch.where(inr[None, :, None, None], v[:, jc], 0.0)
        for i in range(min(chunk, t - qi)):
            qq = qi + i
            o = torch.arange(ctx)
            dd = qq - js                                          # distance per slot
            ac = torch.einsum("bhd,bohd->bho", q[:, qq], kk)
            p_idx = o - i                                         # rel_shift: relk index o - i when 0 <= o-i <= left
            relv = torch.where(((p_idx >= 0) & (p_idx <= left))[None, :, None, None],
                               relk[p_idx.clamp(0, left)][None], 0.0)
            bd = torch.einsum("bhd,bohd->bho", q[:, qq], relv.expand(b, -1, -1, -1))
            lg = torch.tanh((ac + bd) / softcap) * softcap
            ok = inr & (dd >= 0) & (dd < left)
            ok = ok[None, :] & (key_mask[:, jc] != 0)
            lg = lg.masked_fill(~ok[:, None, :], invalid)
            pr = torch.softmax(lg.float(), dim=-1)
            out[:, qq] = torch.einsum("bho,bohd->bhd", pr, vv)
    return out.reshape(b, t, h * d)


@audio_chunk_attention.register_fake
def _(q, k, v, relk, key_mask, left, softcap, invalid):
    return q.new_empty((q.shape[0], q.shape[1], q.shape[2] * q.shape[3]))


def _audio_attention_forward(self, hidden_states, position_embeddings, attention_mask=None, **kwargs):
    b, t, _ = hidden_states.shape
    shp = (b, t, self.num_heads, self.head_dim)
    import torch.nn.functional as F
    q = self.q_proj(hidden_states).float().view(shp) * self.q_scale * F.softplus(self.per_dim_scale)
    k = self.k_proj(hidden_states).float().view(shp) * self.k_scale
    v = self.v_proj(hidden_states).float().view(shp)
    relk = self.relative_k_proj(position_embeddings).view(-1, self.num_heads, self.head_dim).to(q.dtype)
    out = torch.ops.pigmed.audio_chunk_attention(q, k, v, relk, KEY_MASK["audio"], int(self.chunk_size),
                                                 float(self.attention_logits_soft_cap),
                                                 float(self.config.attention_invalid_logits_value))
    return self.post(out.to(hidden_states.dtype)), None


@torch.library.custom_op("pigmed::channel_layer_norm", mutates_args=())
def channel_layer_norm(x: torch.Tensor, weight: torch.Tensor, eps: float, relu: int) -> torch.Tensor:
    """LayerNorm over dim 1 of an NCHW tensor (no bias), optional ReLU: what Gemma 4's audio subsampling conv
    layers compute as permute(0,2,3,1) -> LayerNorm -> permute(0,3,1,2) (-> ReLU)."""
    y = torch.nn.functional.layer_norm(x.permute(0, 2, 3, 1), (x.shape[1],), weight, None, eps).permute(0, 3, 1, 2)
    return torch.relu(y).contiguous() if relu else y.contiguous()


@channel_layer_norm.register_fake
def _(x, weight, eps, relu):
    return torch.empty_like(x)


def _cln_onnx(x, weight, eps: float, relu: int):
    import onnxscript

    pig = onnxscript.values.Opset("pigmed.ai", 1)
    return pig.ChannelLayerNorm(x, weight, eps=eps, relu=relu)


def _subsample_layer_forward(self, hidden_states, mask=None):
    """Gemma4AudioSubSampleConvProjectionLayer.forward with the channel LayerNorm + ReLU as one op (no permutes)."""
    if mask is not None:
        mask = mask.to(device=hidden_states.device)
        hidden_states = hidden_states * mask[:, None, :, None]
    h = self.conv(hidden_states.to(self.conv.weight.dtype))
    assert isinstance(self.act, nn.ReLU) and getattr(self.norm, "bias", None) is None
    h = torch.ops.pigmed.channel_layer_norm(h, self.norm.weight, float(self.norm.eps), 1)
    if mask is not None:
        mask = mask[:, ::2]
    return h, mask


def _clip_bounds(model):
    """Gemma4ClippableLinear clamps with buffer bounds (Min/Max nodes, single-threaded in ORT: 23 % of the audio
    tower). Read them once so the export emits Clip with constant bounds (multithreaded) and skips infinite ones."""
    n = 0
    for m in model.modules():
        if type(m).__name__ == "Gemma4ClippableLinear" and getattr(m, "use_clipped_linears", False):
            m._pig_bounds = tuple(float(getattr(m, k)) for k in ("input_min", "input_max", "output_min", "output_max"))
            n += 1
    return n


def _clamp_const(h, lo, hi):
    lo_ok, hi_ok = math.isfinite(lo), math.isfinite(hi)
    return torch.clamp(h, min=lo if lo_ok else None, max=hi if hi_ok else None) if (lo_ok or hi_ok) else h


def _clippable_forward(self, hidden_states):
    if self.use_clipped_linears:
        imin, imax, omin, omax = self._pig_bounds
        hidden_states = _clamp_const(hidden_states, imin, imax)
    hidden_states = self.linear(hidden_states)
    if self.use_clipped_linears:
        hidden_states = _clamp_const(hidden_states, omin, omax)
    return hidden_states


def _aca_onnx(q, k, v, relk, key_mask, left: int, softcap: float, invalid: float):
    import onnxscript

    pig = onnxscript.values.Opset("pigmed.ai", 1)
    return pig.AudioChunkAttention(q, k, v, relk, key_mask, left=left, softcap=softcap, invalid=invalid)


def _pigmed_attention(module, query, key, value, attention_mask, scaling=None, dropout=0.0, sliding_window=None,
                      softcap=None, **kwargs):
    assert softcap is None, "softcap not supported by pigmed.ai::FlashAttention"
    if scaling is None:
        scaling = module.head_dim ** -0.5
    window = int(sliding_window) - 1 if sliding_window else -1  # HF passes window+1 (flash convention)
    out = torch.ops.pigmed.flash_attention(query, key, value, KEY_MASK["m"], float(scaling), window)
    return out, None


def _register_pigmed():
    from onnx.defs import OpSchema, has, register_schema
    from transformers import AttentionInterface

    AttentionInterface.register("pigmed_flash", _pigmed_attention)
    if not has("FlashAttention", "pigmed.ai"):  # so onnxscript can emit the node
        P = OpSchema.FormalParameter
        register_schema(OpSchema(
            "AudioChunkAttention", "pigmed.ai", 1,
            inputs=[P("Q", "T"), P("K", "T"), P("V", "T"), P("RelK", "T"), P("key_mask", "tensor(int64)")],
            outputs=[P("O", "T")],
            type_constraints=[("T", ["tensor(float)"], "float32 only")],
            attributes=[OpSchema.Attribute("left", OpSchema.AttrType.INT, "chunk size = past horizon + 1"),
                        OpSchema.Attribute("softcap", OpSchema.AttrType.FLOAT, "logit soft cap"),
                        OpSchema.Attribute("invalid", OpSchema.AttrType.FLOAT, "masked logit value")],
        ))
        register_schema(OpSchema(
            "ChannelLayerNorm", "pigmed.ai", 1,
            inputs=[P("X", "T"), P("W", "T")],
            outputs=[P("Y", "T")],
            type_constraints=[("T", ["tensor(float)"], "float32 only")],
            attributes=[OpSchema.Attribute("eps", OpSchema.AttrType.FLOAT, "variance epsilon"),
                        OpSchema.Attribute("relu", OpSchema.AttrType.INT, "1 = ReLU after the norm")],
        ))
        register_schema(OpSchema(
            "FlashAttention", "pigmed.ai", 1,
            inputs=[P("Q", "T"), P("K", "T"), P("V", "T"), P("key_mask", "tensor(int64)")],
            outputs=[P("O", "T")],
            type_constraints=[("T", ["tensor(float)"], "float32 only")],
            attributes=[OpSchema.Attribute("scale", OpSchema.AttrType.FLOAT, "score scale"),
                        OpSchema.Attribute("window", OpSchema.AttrType.INT, "|i-j| <= window, -1 = full")],
        ))


def use_attn(model, name):
    for m in model.modules():
        c = getattr(m, "config", None)
        if c is not None and hasattr(c, "_attn_implementation"):
            try:
                c._attn_implementation = name
            except Exception:
                c.__dict__["_attn_implementation_internal"] = name


def _fa_onnx(q, k, v, key_mask, scale: float, window: int):
    import onnxscript

    pig = onnxscript.values.Opset("pigmed.ai", 1)
    return pig.FlashAttention(q, k, v, key_mask, scale=scale, window=window)


# ----------------------------------------------------------------------------------------------- gemma
class GemmaLM(nn.Module):
    def __init__(self, model):
        super().__init__()
        self.lm = model.language_model
        self.pad_id = model.config.text_config.pad_token_id

    def forward(self, input_ids, attention_mask, mm_embeds, mm_mask):
        KEY_MASK["m"] = attention_mask.to(torch.int64)
        ids = torch.where(mm_mask, torch.full_like(input_ids, self.pad_id), input_ids)
        emb = self.lm.embed_tokens(ids)
        emb = torch.where(mm_mask.unsqueeze(-1), mm_embeds.to(emb.dtype), emb)
        return self.lm(inputs_embeds=emb, attention_mask=attention_mask).last_hidden_state


class GemmaVision(nn.Module):
    def __init__(self, model):
        super().__init__()
        self.vt = model.vision_tower
        self.ev = model.embed_vision

    def forward(self, pixel_values, image_position_ids):
        vt, cfg = self.vt, self.vt.config
        k = cfg.pooling_kernel_size
        out_len = pixel_values.shape[-2] // (k * k)
        pad = (image_position_ids == -1).all(dim=-1)
        KEY_MASK["m"] = (~pad).to(torch.int64)
        x = vt.patch_embedder(pixel_values, image_position_ids, pad)
        h = vt.encoder(inputs_embeds=x, attention_mask=~pad, pixel_position_ids=image_position_ids).last_hidden_state
        h, pmask = vt.pooler(hidden_states=h, pixel_position_ids=image_position_ids, padding_positions=pad,
                             output_length=out_len)
        if cfg.standardize:
            h = (h - vt.std_bias.float()) * vt.std_scale.float()
        h = h.to(x.dtype)
        return self.ev(inputs_embeds=h), pmask


class GemmaVisionDyn(nn.Module):
    """Vision tower with a dynamic patch count (any multiple of 9: 70..1120 soft tokens, video frames).
    Same math as Gemma4VisionModel + pooler, with the pooling kernel fixed to config.pooling_kernel_size so the
    graph does not specialise on the input length."""

    def __init__(self, model):
        super().__init__()
        self.vt = model.vision_tower
        self.ev = model.embed_vision
        self.k = model.config.vision_config.pooling_kernel_size

    def forward(self, pixel_values, image_position_ids):
        vt, k = self.vt, self.k
        pad = (image_position_ids == -1).all(dim=-1)
        KEY_MASK["m"] = (~pad).to(torch.int64)
        x = vt.patch_embedder(pixel_values, image_position_ids, pad)
        h = vt.encoder(inputs_embeds=x, attention_mask=~pad, pixel_position_ids=image_position_ids).last_hidden_state
        h = h.masked_fill(pad.unsqueeze(-1), 0.0)
        out_len = pixel_values.shape[-2] // (k * k)
        pos = image_position_ids.clamp(min=0)
        max_x = pos[..., 0].max(dim=-1, keepdim=True)[0] + 1
        kidx = torch.div(pos, k, rounding_mode="floor")
        kidx = kidx[..., 0] + (max_x // k) * kidx[..., 1]
        # = HF one_hot(kidx, out_len)/k^2 pooling, as a scatter-add so out_len stays symbolic in the export
        idx = kidx.long()
        pooled = h.float().new_zeros((h.shape[0], out_len, h.shape[2])).scatter_add(
            1, idx.unsqueeze(-1).expand(-1, -1, h.shape[2]), h.float()) / (k * k)
        cnt = h.new_zeros((h.shape[0], out_len)).scatter_add(1, idx, torch.ones_like(idx, dtype=h.dtype))
        pmask = cnt > 0
        pooled = pooled * vt.pooler.root_hidden_size
        if vt.config.standardize:
            pooled = (pooled - vt.std_bias.float()) * vt.std_scale.float()
        return self.ev(inputs_embeds=pooled.to(x.dtype)), pmask


class GemmaAudio(nn.Module):
    def __init__(self, model):
        super().__init__()
        self.at = model.audio_tower
        self.ea = model.embed_audio

    def forward(self, input_features, input_features_mask):
        if ATTN == "pigmed":
            # same as Gemma4AudioModel.forward, minus the dense/blocked mask construction: the Rust
            # AudioChunkAttention op takes the subsampled key mask directly
            at, cfg = self.at, self.at.config
            h, m2 = at.subsample_conv_projection(input_features, input_features_mask)
            KEY_MASK["audio"] = m2.to(torch.int64)
            pe = at.rel_pos_enc(h)
            for layer in at.layers[: cfg.num_hidden_layers]:
                h = layer(h, attention_mask=None, position_embeddings=pe)
            return self.ea(inputs_embeds=at.output_proj(h)), m2
        o = self.at(input_features, input_features_mask, return_dict=True)
        return self.ea(inputs_embeds=o.last_hidden_state), o.attention_mask


class GemmaAudioSub(nn.Module):
    """Audio part 1: subsampling convs (mel frames -> [B, T2, 1024] + mask)."""

    def __init__(self, model):
        super().__init__()
        self.at = model.audio_tower

    def forward(self, input_features, input_features_mask):
        return self.at.subsample_conv_projection(input_features, input_features_mask)


class GemmaAudioEnc(nn.Module):
    """Audio part 2: conformer + output_proj + embed_audio on [B, T2, 1024]. Exported with T2 = 12*K (the attention
    chunk) so the blocked-attention shape arithmetic is linear; the runner pads T2 with masked zeros."""

    def __init__(self, model):
        super().__init__()
        self.at = model.audio_tower
        self.ea = model.embed_audio

    def forward(self, hidden_states, mask):
        at, cfg = self.at, self.at.config
        c = cfg.attention_chunk_size  # 12
        past, fut = cfg.attention_context_left - 1, cfg.attention_context_right  # 12, 0
        ctx = c + past + fut
        b, t = mask.shape
        k = t // c  # t is a multiple of the chunk size by construction
        # blocked additive mask [B, 1, K, chunk, ctx] built directly (same as masking_utils sliding window +
        # key padding + _convert_4d_mask_to_blocked_5d), linear in K for export
        blk = torch.arange(k, device=mask.device)[:, None, None] * c
        q = blk + torch.arange(c, device=mask.device)[None, :, None]
        j = blk + torch.arange(ctx, device=mask.device)[None, None, :] - past
        ok = (j >= 0) & (j < t) & (j - q <= fut) & (q - j <= past)
        kv = mask[:, j.clamp(0, t - 1)] & ok[None]  # [B, K, c, ctx]
        am = torch.zeros(kv.shape, dtype=hidden_states.dtype, device=mask.device).masked_fill(
            ~kv, torch.finfo(hidden_states.dtype).min)[:, None]
        pe = at.rel_pos_enc(hidden_states)
        for layer in at.layers[: cfg.num_hidden_layers]:
            hidden_states = layer(hidden_states, attention_mask=am, position_embeddings=pe)
        return self.ea(inputs_embeds=at.output_proj(hidden_states)), mask


# ----------------------------------------------------------------------------------------------- neomme
def _neomme_body(m, hidden, input_ids, attention_mask, position_ids):
    from transformers.masking_utils import create_bidirectional_mask, create_bidirectional_sliding_window_mask

    mm = m.model
    cfg = mm.config
    KEY_MASK["m"] = attention_mask.to(torch.int64)
    if position_ids.ndim == 2:
        position_ids = position_ids.unsqueeze(0).expand(2, -1, -1)
    hidden = initial = mm.embedding_norm(hidden)
    masks = {}
    kw = {"inputs_embeds": hidden, "attention_mask": attention_mask}
    for i in range(cfg.num_hidden_layers):
        plc = cfg.per_layer_config[i]
        fn = create_bidirectional_sliding_window_mask if plc.sliding_window is not None else create_bidirectional_mask
        masks[i] = fn(config=plc, **kw)
    pe = {lt: mm.rotary_emb(hidden, position_ids, lt) for lt in set(cfg.layer_types)}
    value_embeds = mm.value_embeddings(input_ids)
    for i, layer in enumerate(mm.layers):
        hidden = layer(hidden, initial, value_embeds if i in mm.value_embedding_layers else None,
                       position_embeddings=pe[layer.attention_type], attention_mask=masks[i])
    hidden = mm.final_norm(hidden)
    return m.dense_head(hidden, attention_mask, None), m.multi_vector_head(hidden, attention_mask)


class NeoText(nn.Module):
    def __init__(self, m):
        super().__init__()
        self.m = m

    def forward(self, input_ids, attention_mask):
        b, s = input_ids.shape
        pos = torch.arange(s, device=input_ids.device).unsqueeze(0).expand(b, -1)
        return _neomme_body(self.m, self.m.model.embed_tokens(input_ids), input_ids, attention_mask, pos)


class NeoImage(nn.Module):
    def __init__(self, m):
        super().__init__()
        self.m = m

    def forward(self, input_ids, attention_mask, position_ids, patches, patch_mask):
        mm = self.m.model
        tok = mm.embed_tokens(input_ids)
        pat = mm.patch_embeddings(patches.to(mm.patch_embeddings.norm.weight.dtype))
        hidden = torch.where(patch_mask.unsqueeze(-1), pat, tok)
        return _neomme_body(self.m, hidden, input_ids, attention_mask, position_ids)


# ----------------------------------------------------------------------------------------------- norm rewrite
_ORIG = {}


def _exclusive_forward(self, attn_output, value_states):
    """NeoMMEExclusiveSelfAttention.forward (attn_output [B,S,H,D], value_states [B,Hkv,S,D]) without repeat_kv:
    head h = kv * G + g (repeat_kv order), so attn_output viewed as [B,S,Hkv,G,D] broadcasts against V [B,S,Hkv,1,D]."""
    b, s, h, d = attn_output.shape
    hkv = value_states.shape[1]
    g = h // hkv
    v = value_states.float()
    vu = (v / v.norm(p=2, dim=-1, keepdim=True).clamp_min(1e-12)).to(attn_output.dtype)  # = F.normalize, no expand
    vu = vu.transpose(1, 2).unsqueeze(3)  # [B,S,Hkv,1,D]
    ao = attn_output.reshape(b, s, hkv, g, d)
    proj = (ao * vu).sum(-1, keepdim=True)
    scale = torch.tanh(self.alpha).to(ao.dtype).view(1, 1, hkv, g, 1)
    return (ao - (scale * proj) * vu).reshape(b, s, h, d)


def patch_rmsnorm_for_ort(enable: bool = True):
    """HF writes RMSNorm as x * pow(mean(x^2)+eps, -0.5). Mathematically identical x / sqrt(...) * scale is the
    pattern ONNX Runtime fuses into one SimplifiedLayerNormalization kernel (scale = ones when the HF norm has
    no weight). Only affects the traced graph; parity is re-checked against the unpatched HF forward."""
    import importlib

    def fwd(self, hidden_states):
        x = hidden_states.float()
        ms = x.pow(2).mean(-1, keepdim=True) + self.eps
        y = x / torch.sqrt(ms)
        w = self.weight.float() if getattr(self, "with_scale", True) and hasattr(self, "weight") else \
            torch.ones(x.shape[-1], dtype=x.dtype, device=x.device)
        return (y * w).type_as(hidden_states)

    n = 0
    for mod, cls in [("transformers.models.embedding_gemma2.modeling_embedding_gemma2", "EmbeddingGemma2RMSNorm"),
                     ("transformers.models.neomme.modeling_neomme", "NeoMMERMSNorm"),
                     ("transformers.models.gemma4.modeling_gemma4", "Gemma4RMSNorm")]:
        try:
            c = getattr(importlib.import_module(mod), cls)
        except (ImportError, AttributeError):
            continue
        # Gemma4RMSNorm may use a different weight convention; only patch classes whose forward we verified.
        src = __import__("inspect").getsource(c)
        if "torch.pow(mean_squared, -0.5)" in src and "self.with_scale" in src:
            _ORIG.setdefault(c, c.forward)
            c.forward = fwd if enable else _ORIG[c]
            n += 1
    # NeoMME exclusive self-attention: same math without materialising repeat_kv(V) and F.normalize's expand_as
    # (34 Expand nodes per forward; each a thread-pool sync, which costs most on oversubscribed shared nodes).
    try:
        c = getattr(importlib.import_module("transformers.models.neomme.modeling_neomme"), "NeoMMEExclusiveSelfAttention")
        _ORIG.setdefault(c, c.forward)
        c.forward = _exclusive_forward if enable else _ORIG[c]
        n += 1
    except (ImportError, AttributeError):
        pass
    if enable:
        print(f"  patched {n} RMSNorm / exclusive-attention classes for ORT", flush=True)


# ----------------------------------------------------------------------------------------------- helpers
def cos_rows(a: np.ndarray, b: np.ndarray) -> np.ndarray:
    a = a.reshape(len(a), -1) if a.ndim > 2 else a
    b = b.reshape(len(b), -1) if b.ndim > 2 else b
    return (a * b).sum(-1) / (np.linalg.norm(a, axis=-1) * np.linalg.norm(b, axis=-1) + 1e-12)


def report(name, ref, got):
    ref, got = np.asarray(ref, np.float32), np.asarray(got, np.float32)
    c = cos_rows(ref, got)
    d = float(np.abs(ref - got).max())
    print(f"  {name}: shape={ref.shape} min_cos={c.min():.7f} max_abs={d:.3e}", flush=True)
    return {"min_cos": float(c.min()), "max_abs": d}


OPSET = None  # set from --opset
LEGACY = False  # set from --legacy-export: TorchScript exporter instead of dynamo
ATTN = "sdpa"  # set from --attn


def export(module, args, path: Path, input_names, output_names, dynamic):
    t = time.time()
    path.parent.mkdir(parents=True, exist_ok=True)
    try:
        if LEGACY:
            raise RuntimeError("--legacy-export")
        kw = {"opset_version": OPSET} if OPSET else {}
        if ATTN == "pigmed":
            kw["custom_translation_table"] = {torch.ops.pigmed.flash_attention.default: _fa_onnx,
                                              torch.ops.pigmed.audio_chunk_attention.default: _aca_onnx,
                                              torch.ops.pigmed.channel_layer_norm.default: _cln_onnx}
        opt = os.environ.get("PIGMED_ONNX_OPTIMIZE", "1") == "1"
        prog = torch.onnx.export(module, args, dynamo=True, input_names=input_names, output_names=output_names,
                                 dynamic_shapes=dynamic, optimize=opt, external_data=True, **kw)
        prog.save(str(path), external_data=True)
        how = "dynamo"
    except Exception as e:  # pragma: no cover - fallback path
        print(f"  dynamo export failed ({type(e).__name__}: {str(e)[:300]}); retrying TorchScript exporter", flush=True)
        axes = {}
        for n, d in zip(input_names, dynamic):
            axes[n] = {k: v.__name__ if hasattr(v, "__name__") else str(v) for k, v in (d or {}).items()}
        torch.onnx.export(module, args, str(path), dynamo=False, input_names=input_names, output_names=output_names,
                          dynamic_axes=axes, opset_version=18)
        how = "torchscript"
    print(f"  exported {path.name} via {how} in {time.time() - t:.0f}s", flush=True)
    try:  # structural check (the custom-op graphs cannot be run by stock onnxruntime)
        import onnx

        onnx.checker.check_model(str(path), full_check=False)
        print(f"  onnx.checker ok: {path.name}", flush=True)
    except Exception as e:
        print(f"  onnx.checker FAILED for {path.name}: {str(e)[:300]}", flush=True)
    return how


def ort_run(path: Path, feeds: dict, threads: int):
    import onnxruntime as ort

    if ATTN == "pigmed":  # custom op lives in the Rust runner; parity is checked there against torch
        return None

    so = ort.SessionOptions()
    so.intra_op_num_threads = threads
    so.inter_op_num_threads = 1
    so.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL
    s = ort.InferenceSession(str(path), so, providers=["CPUExecutionProvider"])
    return s.run(None, {k: v.numpy() if torch.is_tensor(v) else v for k, v in feeds.items()})


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--inputs", type=Path, required=True)
    ap.add_argument("--which", nargs="+", default=["gemma_lm", "gemma_vision", "neomme_text", "neomme_image"],
                    help="also: gemma_vision_dyn, gemma_audio")
    ap.add_argument("--threads", type=int, default=int(os.environ.get("SLURM_CPUS_PER_TASK", "8")))
    ap.add_argument("--opset", type=int, default=None, help="ONNX opset (23 maps SDPA to the fused Attention op)")
    ap.add_argument("--attn", default="sdpa", choices=["sdpa", "eager", "pigmed"],
                    help="attention to trace; pigmed = Rust fused attention custom op (src/flash.rs)")
    ap.add_argument("--fuse-norm", action="store_true", help="rewrite RMSNorm into ORT's fusable pattern")
    ap.add_argument("--legacy-export", action="store_true", help="TorchScript exporter (no custom attention op)")
    args = ap.parse_args()
    global OPSET, ATTN, LEGACY
    LEGACY = args.legacy_export
    OPSET = args.opset
    ATTN = args.attn
    load_attn = "eager" if args.attn == "pigmed" else args.attn  # HF reference: eager (the audio tower is eager-only)
    if args.attn == "pigmed":
        _register_pigmed()
    torch.set_num_threads(args.threads)
    models = json.load(open(Path(os.environ["EMBED_ROOT"]) / "models.json"))
    from PIL import Image

    texts = [json.loads(l)["text"] for l in open(args.inputs / "texts.jsonl")][:2]
    imrows = [json.loads(l) for l in open(args.inputs / "images.jsonl")]
    # one page + one figure, different sizes -> exercises padding in both models
    ims = [Image.open(args.inputs / r["path"]).convert("RGB") for r in
           [next(r for r in imrows if r["kind"] == "page"), next(r for r in imrows if r["kind"] == "figure")]]
    summary = {"attn": args.attn, "opset": args.opset, "fuse_norm": args.fuse_norm}

    if any(w.startswith("gemma") for w in args.which):
        from transformers import AutoModel, AutoProcessor

        snap = models["google/embeddinggemma-2"]["path"]
        proc = AutoProcessor.from_pretrained(snap)
        akw = {} if any(w.startswith("gemma_audio") for w in args.which) else {"audio_config": None}
        model = AutoModel.from_pretrained(snap, dtype=torch.float32, attn_implementation=load_attn, **akw).eval()
        print("gemma loaded:", type(model).__name__, "attn:", model.config._attn_implementation, flush=True)
        H = model.config.text_config.hidden_size

        if "gemma_lm" in args.which:
            enc = proc.tokenizer(texts[:2], padding=True, truncation=True, max_length=512, return_tensors="pt")
            ids, am = enc["input_ids"], enc["attention_mask"]
            patch_rmsnorm_for_ort(False)
            use_attn(model, load_attn) if ATTN == 'pigmed' else None
            ref = model(input_ids=ids, attention_mask=am).last_hidden_state
            if ATTN == "pigmed":
                use_attn(model, "pigmed_flash")
            if args.fuse_norm:
                patch_rmsnorm_for_ort(True)
            w = GemmaLM(model).eval()
            z = torch.zeros(*ids.shape, H)
            zm = torch.zeros_like(ids, dtype=torch.bool)
            got = w(ids, am, z, zm)
            r = {"wrapper_vs_hf": report("gemma_lm wrapper vs HF", ref, got)}
            if ATTN == "pigmed":  # beyond the 512-token sliding window, against HF eager
                lid = torch.from_numpy(np.random.default_rng(1).integers(1000, 200000, (1, 1400)))
                lam = torch.ones_like(lid)
                use_attn(model, load_attn)
                patch_rmsnorm_for_ort(False)
                lref = model(input_ids=lid, attention_mask=lam).last_hidden_state
                use_attn(model, "pigmed_flash")
                patch_rmsnorm_for_ort(args.fuse_norm)
                lgot = w(lid, lam, torch.zeros(1, 1400, H), torch.zeros_like(lid, dtype=torch.bool))
                r["wrapper_vs_hf_1400"] = report("gemma_lm wrapper vs HF @1400 tokens", lref, lgot)
            B, S = torch.export.Dim("batch", min=1, max=64), torch.export.Dim("seq", min=2, max=8192)
            r["how"] = export(w, (ids, am, z, zm), args.out / "gemma_lm.onnx",
                              ["input_ids", "attention_mask", "mm_embeds", "mm_mask"], ["last_hidden_state"],
                              ({0: B, 1: S}, {0: B, 1: S}, {0: B, 1: S}, {0: B, 1: S}))
            if ATTN != "pigmed":
                (o,) = ort_run(args.out / "gemma_lm.onnx", {"input_ids": ids, "attention_mask": am, "mm_embeds": z,
                                                             "mm_mask": zm.numpy()}, args.threads)
                r["onnx_vs_wrapper"] = report("gemma_lm onnx vs wrapper", got, o)
            summary["gemma_lm"] = r

        if "gemma_vision" in args.which:
            enc = proc(images=[[im] for im in ims], return_tensors="pt", padding=True)
            print("  processor keys:", {k: tuple(v.shape) if hasattr(v, "shape") else v for k, v in enc.items()})
            patch_rmsnorm_for_ort(False)
            use_attn(model, load_attn) if ATTN == 'pigmed' else None
            ref = model(**{k: v for k, v in enc.items() if k != "num_soft_tokens_per_image"}).last_hidden_state
            if ATTN == "pigmed":
                use_attn(model, "pigmed_flash")
            if args.fuse_norm:
                patch_rmsnorm_for_ort(True)
            vis = GemmaVision(model).eval()
            soft, smask = vis(enc["pixel_values"], enc["image_position_ids"])
            # rebuild the multimodal merge with the wrapper outputs and compare the full forward
            lm = GemmaLM(model).eval()
            mm_mask = enc["input_ids"] == model.config.image_token_id
            mm = torch.zeros(*enc["input_ids"].shape, H)
            for b in range(len(ims)):
                n = int(mm_mask[b].sum())
                assert n == int(smask[b].sum()), (n, int(smask[b].sum()))
                mm[b, mm_mask[b]] = soft[b, smask[b]][:n]
            got = lm(enc["input_ids"], enc["attention_mask"], mm, mm_mask)
            r = {"wrapper_vs_hf": report("gemma image (vision+lm wrappers) vs HF", ref, got)}
            B = torch.export.Dim("batch", min=1, max=64)
            pv, pp = enc["pixel_values"].float(), enc["image_position_ids"].long()
            r["how"] = export(vis, (pv, pp), args.out / "gemma_vision.onnx", ["pixel_values", "image_position_ids"],
                              ["soft_tokens", "soft_mask"], ({0: B}, {0: B}))
            if ATTN != "pigmed":
                o_soft, o_mask = ort_run(args.out / "gemma_vision.onnx", {"pixel_values": pv, "image_position_ids": pp},
                                         args.threads)
                r["onnx_vs_wrapper"] = report("gemma_vision onnx vs wrapper", soft, o_soft)
                assert (o_mask == smask.numpy()).all()
            summary["gemma_vision"] = r
        if "gemma_vision_dyn" in args.which:
            r = {}
            vis = GemmaVisionDyn(model).eval()
            B, P = torch.export.Dim("batch", min=1, max=64), torch.export.Dim("p9", min=8, max=1120)
            for budget in (70, 140, 280, 560, 1120):
                enc = proc(images=[[im] for im in ims], return_tensors="pt", padding=True,
                           max_soft_tokens=budget)
                use_attn(model, load_attn) if ATTN == "pigmed" else None
                patch_rmsnorm_for_ort(False)
                ref_soft, ref_mask = GemmaVision(model).eval()(enc["pixel_values"], enc["image_position_ids"])
                patch_rmsnorm_for_ort(args.fuse_norm)
                if ATTN == "pigmed":
                    use_attn(model, "pigmed_flash")
                soft, sm = vis(enc["pixel_values"], enc["image_position_ids"])
                r[f"dyn_vs_hf_pooler_{budget}"] = report(f"vision_dyn vs HF pooler @{budget}", ref_soft, soft)
                assert (sm == ref_mask).all()
                if budget == 280:
                    pv, pp = enc["pixel_values"].float(), enc["image_position_ids"].long()
            pv9 = pv  # trace with 2520 patches; P is dynamic (multiple of 9)
            r["how"] = export(vis, (pv9, pp), args.out / "gemma_vision_dyn.onnx", ["pixel_values", "image_position_ids"],
                              ["soft_tokens", "soft_mask"], ({0: B, 1: 9 * P}, {0: B, 1: 9 * P}))
            for budget in (70, 1120):
                enc = proc(images=[[im] for im in ims], return_tensors="pt", padding=True,
                           max_soft_tokens=budget)
                soft, _ = vis(enc["pixel_values"], enc["image_position_ids"])
                if ATTN != "pigmed":
                    o_soft, _ = ort_run(args.out / "gemma_vision_dyn.onnx", {"pixel_values": enc["pixel_values"].float(),
                                        "image_position_ids": enc["image_position_ids"].long()}, args.threads)
                    r[f"onnx_vs_wrapper_{budget}"] = report(f"gemma_vision_dyn onnx vs wrapper @{budget}", soft, o_soft)
            summary["gemma_vision_dyn"] = r

        if "gemma_audio_split" in args.which:
            r = {}
            fe = proc.feature_extractor
            t = np.arange(16000 * 7) / 16000
            wav = [(0.3 * np.sin(2 * np.pi * (200 + 150 * t) * t)).astype(np.float32),
                   (0.3 * np.sin(2 * np.pi * 330 * t[: 16000 * 4])).astype(np.float32)]
            feats = fe(wav, return_tensors="pt")
            hf = model.get_audio_features(feats["input_features"], feats["input_features_mask"], return_dict=True)
            sub, enc = GemmaAudioSub(model).eval(), GemmaAudioEnc(model).eval()
            h, m2 = sub(feats["input_features"], feats["input_features_mask"])
            T2 = h.shape[1]
            P = -(-T2 // 12) * 12
            hp = torch.nn.functional.pad(h, (0, 0, 0, P - T2))
            mp = torch.nn.functional.pad(m2, (0, P - T2), value=False)
            soft, smask = enc(hp, mp)
            r["split_padded_vs_hf"] = report("audio sub+enc (T2 padded to 12k) vs HF", hf.pooler_output[hf.attention_mask],
                                             soft[:, :T2][smask[:, :T2]])
            B, T, K = (torch.export.Dim("batch", min=1, max=64), torch.export.Dim("frames", min=16, max=200000),
                       torch.export.Dim("k12", min=1, max=20000))
            if os.environ.get("SKIP_SUB") != "1":
              r["how_sub"] = export(sub, (feats["input_features"], feats["input_features_mask"]),
                                  args.out / "gemma_audio_sub.onnx", ["input_features", "input_features_mask"],
                                  ["hidden", "mask"], ({0: B, 1: T}, {0: B, 1: T}))
            r["how_enc"] = export(enc, (hp, mp), args.out / "gemma_audio_enc.onnx", ["hidden", "mask"],
                                  ["soft_tokens", "soft_mask"], ({0: B, 1: 12 * K}, {0: B, 1: 12 * K}))
            summary["gemma_audio_split"] = r

        if "gemma_audio" in args.which:
            r = {}
            fe = proc.feature_extractor
            rng = np.random.default_rng(0)
            t = np.arange(16000 * 7) / 16000
            wav = [(0.3 * np.sin(2 * np.pi * (200 + 150 * t) * t) + 0.02 * rng.standard_normal(len(t))).astype(np.float32),
                   (0.3 * np.sin(2 * np.pi * 330 * t[: 16000 * 4])).astype(np.float32)]
            feats = fe(wav, return_tensors="pt")
            print("  audio features:", {k: tuple(v.shape) for k, v in feats.items()}, flush=True)
            au = GemmaAudio(model).eval()
            soft, amask = au(feats["input_features"], feats["input_features_mask"])
            hf = model.get_audio_features(feats["input_features"], feats["input_features_mask"], return_dict=True)
            if ATTN == "pigmed":
                from transformers.models.gemma4 import modeling_gemma4 as g4
                g4.Gemma4AudioAttention.forward = _audio_attention_forward
                g4.Gemma4AudioSubSampleConvProjectionLayer.forward = _subsample_layer_forward
                print(f"  {_clip_bounds(model)} clippable linears -> constant Clip", flush=True)
                g4.Gemma4ClippableLinear.forward = _clippable_forward
                soft, amask = au(feats["input_features"], feats["input_features_mask"])
            r["wrapper_vs_hf"] = report("gemma_audio wrapper vs HF get_audio_features", hf.pooler_output[hf.attention_mask],
                                        soft[amask])
            B, T = torch.export.Dim("batch", min=1, max=64), torch.export.Dim("frames", min=16, max=200000)
            r["how"] = export(au, (feats["input_features"], feats["input_features_mask"]), args.out / "gemma_audio.onnx",
                              ["input_features", "input_features_mask"], ["soft_tokens", "soft_mask"],
                              ({0: B, 1: T}, {0: B, 1: T}))
            feats2 = fe([wav[0][: 16000 * 3]], return_tensors="pt")
            for name, f in (("7s+4s batch", feats), ("3s", feats2)):
                s_, m_ = au(f["input_features"], f["input_features_mask"])
                if ATTN != "pigmed":
                    os_, om_ = ort_run(args.out / "gemma_audio.onnx", {"input_features": f["input_features"],
                                       "input_features_mask": f["input_features_mask"].numpy()}, args.threads)
                    r[f"onnx_vs_wrapper_{name}"] = report(f"gemma_audio onnx vs wrapper ({name})", s_, os_)
                    assert (om_ == m_.numpy()).all()
            summary["gemma_audio"] = r
        del model

    if any(w.startswith("neomme") for w in args.which):
        from transformers import NeoMMEForRetrieval, NeoMMEProcessor

        snap = models["Hcompany/NeoMME-260M-Retriever"]["path"]
        proc = NeoMMEProcessor.from_pretrained(snap)
        m = NeoMMEForRetrieval.from_pretrained(snap, dtype=torch.float32, attn_implementation=load_attn).eval()
        print("neomme loaded, attn:", m.config._attn_implementation, flush=True)

        def enc_docs(items, image):
            msgs = [[{"role": "user", "content": [{"type": "image", "image": x}]}] if image else
                    [{"role": "user", "content": x}] for x in items]
            kw = {"padding": "longest"} if image else {"padding": "longest", "truncation": True, "max_length": 512}
            return proc.apply_chat_template(msgs, task="document", tokenize=True, return_dict=True,
                                            return_tensors="pt", processor_kwargs=kw)

        if "neomme_text" in args.which:
            enc = enc_docs(texts, False)
            patch_rmsnorm_for_ort(False)
            use_attn(m, load_attn) if ATTN == 'pigmed' else None
            ref = m(**enc)
            if ATTN == "pigmed":
                use_attn(m, "pigmed_flash")
            if args.fuse_norm:
                patch_rmsnorm_for_ort(True)
            w = NeoText(m).eval()
            dense, late = w(enc["input_ids"], enc["attention_mask"])
            if ATTN == "pigmed":  # beyond the 256/1024 sliding windows, against HF eager
                lid = torch.from_numpy(np.random.default_rng(1).integers(1000, 120000, (1, 1400)))
                lam = torch.ones_like(lid)
                use_attn(m, load_attn)
                patch_rmsnorm_for_ort(False)
                lref = m(input_ids=lid, attention_mask=lam)
                use_attn(m, "pigmed_flash")
                patch_rmsnorm_for_ort(args.fuse_norm)
                ld, ll = w(lid, lam)
                summary["neomme_text_1400"] = {"dense": report("neomme text dense @1400", lref.dense_embeddings, ld),
                                               "late": report("neomme text late @1400", lref.embeddings, ll)}
            r = {"wrapper_vs_hf_dense": report("neomme text dense wrapper vs HF", ref.dense_embeddings, dense),
                 "wrapper_vs_hf_late": report("neomme text late wrapper vs HF", ref.embeddings, late)}
            B, S = torch.export.Dim("batch", min=1, max=64), torch.export.Dim("seq", min=2, max=16384)
            r["how"] = export(w, (enc["input_ids"], enc["attention_mask"]), args.out / "neomme_text.onnx",
                              ["input_ids", "attention_mask"], ["dense", "late"], ({0: B, 1: S}, {0: B, 1: S}))
            if ATTN != "pigmed":
                od, ol = ort_run(args.out / "neomme_text.onnx", {"input_ids": enc["input_ids"],
                                                                 "attention_mask": enc["attention_mask"]}, args.threads)
                r["onnx_vs_wrapper_dense"] = report("neomme_text onnx dense", dense, od)
                r["onnx_vs_wrapper_late"] = report("neomme_text onnx late", late, ol)
            summary["neomme_text"] = r

        if "neomme_image" in args.which:
            enc = enc_docs(ims, True)
            print("  processor keys:", {k: tuple(v.shape) for k, v in enc.items()})
            patch_rmsnorm_for_ort(False)
            use_attn(m, load_attn) if ATTN == 'pigmed' else None
            ref = m(**enc)
            if ATTN == "pigmed":
                use_attn(m, "pigmed_flash")
            if args.fuse_norm:
                patch_rmsnorm_for_ort(True)
            ids = enc["input_ids"]
            prev = torch.nn.functional.pad(ids[:, :-1], (1, 0), value=m.config.pad_token_id or 0)
            pmask = (ids == m.config.image_token_id) & (prev != m.config.document_token_id)
            patches = torch.zeros(*ids.shape, enc["pixel_values"].shape[-1])
            patches[pmask] = enc["pixel_values"].float()
            w = NeoImage(m).eval()
            dense, late = w(ids, enc["attention_mask"], enc["position_ids"], patches, pmask)
            r = {"wrapper_vs_hf_dense": report("neomme image dense wrapper vs HF", ref.dense_embeddings, dense),
                 "wrapper_vs_hf_late": report("neomme image late wrapper vs HF", ref.embeddings, late)}
            B, S = torch.export.Dim("batch", min=1, max=64), torch.export.Dim("seq", min=2, max=16384)
            r["how"] = export(w, (ids, enc["attention_mask"], enc["position_ids"], patches, pmask),
                              args.out / "neomme_image.onnx",
                              ["input_ids", "attention_mask", "position_ids", "patches", "patch_mask"],
                              ["dense", "late"],
                              ({0: B, 1: S}, {0: B, 1: S}, {1: B, 2: S}, {0: B, 1: S}, {0: B, 1: S}))
            if ATTN != "pigmed":
                od, ol = ort_run(args.out / "neomme_image.onnx",
                                 {"input_ids": ids, "attention_mask": enc["attention_mask"],
                                  "position_ids": enc["position_ids"], "patches": patches, "patch_mask": pmask.numpy()},
                                 args.threads)
                r["onnx_vs_wrapper_dense"] = report("neomme_image onnx dense", dense, od)
                r["onnx_vs_wrapper_late"] = report("neomme_image onnx late", late, ol)
            summary["neomme_image"] = r

    (args.out / f"export-summary-{os.environ.get('SLURM_JOB_ID', 'local')}.json").write_text(
        json.dumps(summary, indent=1))
    print(json.dumps(summary, indent=1))


if __name__ == "__main__":
    main()
