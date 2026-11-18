"""Data-mix config for train_s1 (`--mix-config FILE`). JSON:

    {"name": "s1.5-a",
     "steps": 2600,                                   # optional; total optimiser steps (all ranks)
     "sources": {"pmc_title": {"weight": 0.25},       # directory name under --data
                 "pmc_abssent": {"weight": 0.30},
                 "sym_stackx": {"weight": 0.05}},     # symmetric-ness comes from the source's source.json
     "args": {"lr": 2e-5, "l2sp": 0.01}}              # optional trainer flag defaults (underscored names)

Precedence: an explicit command-line flag > the config's "args"/"steps" > argparse defaults. Weights are relative.
"""
from __future__ import annotations

import json
import sys
from pathlib import Path


def explicit_flags(argv: list[str]) -> set[str]:
    """Dest names of flags given on the command line (--foo-bar / --foo-bar=x -> foo_bar)."""
    out = set()
    for t in argv:
        if t.startswith("--"):
            out.add(t[2:].split("=", 1)[0].replace("-", "_"))
    return out


def apply_mix_config(a, path: Path, argv: list[str] | None = None) -> dict:
    cfg = json.loads(Path(path).read_text())
    given = explicit_flags(sys.argv[1:] if argv is None else argv)
    src = cfg.get("sources") or {}
    if not src:
        raise ValueError(f"{path}: no sources")
    w = {k: float(v["weight"] if isinstance(v, dict) else v) for k, v in src.items()}
    if any(x < 0 for x in w.values()) or sum(w.values()) <= 0:
        raise ValueError(f"{path}: weights must be >= 0 with a positive sum")
    if "sources" not in given:
        a.sources = ",".join(k for k, x in w.items() if x > 0)
    if "mix" not in given:
        a.mix = ",".join(f"{k}:{x}" for k, x in w.items() if x > 0)
    if "steps" not in given and cfg.get("steps"):
        a.steps = int(cfg["steps"])
    for k, v in (cfg.get("args") or {}).items():
        k = k.replace("-", "_")
        if not hasattr(a, k):
            raise ValueError(f"{path}: unknown trainer flag {k!r}")
        if k not in given:
            setattr(a, k, v)
    if not a.steps:
        raise ValueError("a mix needs --steps (or \"steps\" in the config)")
    return cfg
