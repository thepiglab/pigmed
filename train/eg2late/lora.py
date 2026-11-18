"""Minimal LoRA for nn.Linear (no peft dependency), used on the EG2 vision tower in S3.

    y = W x + (alpha / r) * B(A(dropout(x)))       A: [r, in] kaiming, B: [out, r] zeros (starts as the identity map)

`apply_lora` swaps matching nn.Linear children for LoRALinear in place; the frozen base weight stays at
`<name>.base.weight`. `merged_state_dict` returns the module's state dict under the ORIGINAL key names with every
LoRA delta folded into its base weight, so exports load into an unmodified EG2.
"""
from __future__ import annotations

import math
import re

import torch
import torch.nn as nn


class LoRALinear(nn.Module):
    def __init__(self, base: nn.Linear, r: int = 16, alpha: float = 32.0, dropout: float = 0.0):
        super().__init__()
        self.base = base
        for p in self.base.parameters():
            p.requires_grad_(False)
        self.r, self.alpha = r, alpha
        self.scale = alpha / r
        self.A = nn.Parameter(torch.empty(r, base.in_features, dtype=base.weight.dtype, device=base.weight.device))
        self.B = nn.Parameter(torch.zeros(base.out_features, r, dtype=base.weight.dtype, device=base.weight.device))
        nn.init.kaiming_uniform_(self.A, a=math.sqrt(5))
        self.drop = nn.Dropout(dropout) if dropout > 0 else nn.Identity()

    @property
    def weight(self):  # some callers read .weight (dtype/device checks)
        return self.base.weight

    def forward(self, x):
        return self.base(x) + (self.drop(x) @ self.A.t().to(x.dtype) @ self.B.t().to(x.dtype)) * self.scale

    @torch.no_grad()
    def merged_weight(self):
        return self.base.weight + (self.B.float() @ self.A.float()).to(self.base.weight.dtype) * self.scale


DEFAULT_TARGETS = r"(q_proj|k_proj|v_proj|o_proj|gate_proj|up_proj|down_proj)(\.linear)?$"


def apply_lora(module: nn.Module, r: int = 16, alpha: float = 32.0, dropout: float = 0.0,
               targets: str = DEFAULT_TARGETS) -> list[str]:
    """Replace every nn.Linear whose qualified name matches `targets` with a LoRALinear; returns the names."""
    pat = re.compile(targets)
    hits = [n for n, m in module.named_modules() if isinstance(m, nn.Linear) and pat.search(n)]
    for name in hits:
        parent_name, _, child = name.rpartition(".")
        parent = module.get_submodule(parent_name) if parent_name else module
        setattr(parent, child, LoRALinear(getattr(parent, child), r, alpha, dropout))
    return hits


def lora_parameters(module: nn.Module):
    return [p for m in module.modules() if isinstance(m, LoRALinear) for p in (m.A, m.B)]


@torch.no_grad()
def merged_state_dict(module: nn.Module) -> dict[str, torch.Tensor]:
    """State dict with original key names: `x.base.weight` -> `x.weight` (merged), LoRA A/B dropped."""
    lora = {n: m for n, m in module.named_modules() if isinstance(m, LoRALinear)}
    out = {}
    for k, v in module.state_dict().items():
        owner = next((n for n in lora if k.startswith(n + ".")), None)
        if owner is None:
            out[k] = v
            continue
        rest = k[len(owner) + 1:]
        if rest == "base.weight":
            out[f"{owner}.weight"] = lora[owner].merged_weight()
        elif rest.startswith("base."):
            out[f"{owner}.{rest[5:]}"] = v
        # A / B are folded into the weight
    return out
