"""Distributed helpers with node-capped negative gathering (S1.5, multi-node).

GradCache shares negatives by all_gathering every rank's reps. The gathered late reps grow with the number of GPUs
([W * N docs, L, 128]), so on many nodes the gather is capped to a *gather group* of G consecutive ranks (default:
one node, LOCAL_WORLD_SIZE). Each group computes the loss over its own gathered batch; ranks backprop only their
local slice, so the sum of a group's gradients is that group's loss gradient. Gradients are then summed over ALL
ranks and divided by the number of groups: the update is the mean of the group losses' gradients, the same scale as
a single-node run.
"""
from __future__ import annotations

import os

import torch
import torch.distributed as dist
import torch.nn.functional as F

_GROUP = None  # process group for gathers (None = the whole world)
_GSIZE = 1


def rank() -> int:
    return dist.get_rank() if dist.is_initialized() else 0


def world() -> int:
    return dist.get_world_size() if dist.is_initialized() else 1


def init_gather_groups(size: int = 0) -> int:
    """Split the world into consecutive groups of `size` ranks (0 = LOCAL_WORLD_SIZE, i.e. one node; capped at the
    world). Every rank must call this. Returns the group size."""
    global _GROUP, _GSIZE
    W = world()
    if size <= 0:
        size = int(os.environ.get("LOCAL_WORLD_SIZE", W))
    size = max(1, min(size, W))
    if W % size:
        raise ValueError(f"world {W} is not a multiple of gather group {size}")
    _GSIZE = size
    if W == 1 or size == W:
        _GROUP = None
        return size
    for g in range(W // size):  # new_group must be called by all ranks for every group, in the same order
        ranks = list(range(g * size, (g + 1) * size))
        pg = dist.new_group(ranks)
        if rank() in ranks:
            _GROUP = pg
    return size


def gworld() -> int:
    """Ranks in this rank's gather group."""
    return _GSIZE if dist.is_initialized() else 1


def grank() -> int:
    """This rank's index within its gather group."""
    return rank() % _GSIZE if dist.is_initialized() else 0


def n_groups() -> int:
    return world() // gworld()


def gather_cat(x: torch.Tensor, pad_dim: int | None = None, value=0.0) -> torch.Tensor:
    """all_gather along dim 0 within the gather group (equal dim-0 sizes per rank); `pad_dim` is padded to the max
    across the group first."""
    G = gworld()
    if G == 1:
        return x
    if pad_dim is not None:
        n = torch.tensor([x.shape[pad_dim]], device=x.device)
        dist.all_reduce(n, op=dist.ReduceOp.MAX, group=_GROUP)
        if int(n) > x.shape[pad_dim]:
            pad = [0, 0] * (x.dim() - 1 - pad_dim) + [0, int(n) - x.shape[pad_dim]]
            x = F.pad(x, pad, value=value)
    out = [torch.empty_like(x) for _ in range(G)]
    dist.all_gather(out, x.contiguous(), group=_GROUP)
    return torch.cat(out)


def gather_var(x: torch.Tensor, value=0.0) -> tuple[torch.Tensor, list[int]]:
    """all_gather along dim 0 with per-rank sizes that may differ: pads dim 0 to the group max; returns the gathered
    tensor [G * nmax, ...] and the true sizes."""
    G = gworld()
    if G == 1:
        return x, [x.shape[0]]
    n = torch.tensor([x.shape[0]], device=x.device)
    sizes = [torch.zeros_like(n) for _ in range(G)]
    dist.all_gather(sizes, n, group=_GROUP)
    sizes = [int(s) for s in sizes]
    m = max(sizes)
    if m > x.shape[0]:
        x = torch.cat([x, x.new_full((m - x.shape[0], *x.shape[1:]), value)])
    out = [torch.empty_like(x) for _ in range(G)]
    dist.all_gather(out, x.contiguous(), group=_GROUP)
    return torch.cat(out), sizes


def all_reduce_grads(params) -> None:
    """Sum gradients over all ranks, then divide by the number of gather groups (mean of the group losses)."""
    if world() == 1:
        return
    grads = [p.grad for p in params if p.grad is not None]
    if not grads:
        return
    flat = torch._utils._flatten_dense_tensors(grads)
    dist.all_reduce(flat)
    if n_groups() > 1:
        flat /= n_groups()
    for g, f in zip(grads, torch._utils._unflatten_dense_tensors(flat, grads)):
        g.copy_(f)
