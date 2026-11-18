from __future__ import annotations
import json
import re
import contextlib
import random
from pathlib import Path
from .common import atomic_json, read_jsonl
from .embeddings import NeoEncoder


def multipositive_loss(scores, positive_mask, allowed_mask, temperature=.05):
    import torch
    if bool((positive_mask & ~allowed_mask).any()):
        raise ValueError("All positive pairs must be allowed")
    if not bool(positive_mask.any(dim=1).all()):
        raise ValueError("Each query needs at least one positive")
    values = scores / temperature
    positives = values.masked_fill(~positive_mask, float("-inf"))
    denominator = values.masked_fill(~allowed_mask, float("-inf"))
    return (torch.logsumexp(denominator, dim=1) - torch.logsumexp(positives, dim=1)).mean()


def finetune_neomme(data: Path, output: Path, model_id: str, revision: str,
                    device="cuda", batch_size=2, epochs=1, lr=2e-5, seed=42,
                    accumulation=8, max_steps=1000, blocklist: Path | None = None):
    """Small single-device research trainer: joint dense and late-interaction loss.

    Not the authors' complete distributed training recipe. Gradient accumulation
    does NOT create a larger in-batch negative pool. Start from 260M Retriever.
    """
    import torch
    import torch.nn.functional as F
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("Training requires a pinned model commit SHA")
    if output.exists():
        raise FileExistsError("Use a new output path for each training run")
    if batch_size < 2 or accumulation < 1 or epochs < 1 or max_steps < 1:
        raise ValueError("Require batch>=2 and positive training settings")
    records = list(read_jsonl(data))  # deliberately pilot-size training data
    blocked = set(blocklist.read_text().splitlines()) if blocklist else set()
    for row in records:
        if row.get("split") != "train" or not row.get("group_id"):
            raise ValueError("Every row needs split=train and a paper/duplicate-cluster group_id")
        if row["group_id"] in blocked:
            raise ValueError("Held-out group found in training data")
        if "unit_id" not in row["positive"]:
            raise ValueError("Explicit positive unit_id required")
    if len(records) < batch_size:
        raise ValueError("Insufficient training pairs")
    random.seed(seed)
    torch.manual_seed(seed)
    encoder = NeoEncoder(model_id, revision, device)
    # Keep master parameters and Adam states in FP32; use BF16 autocast on supported GPUs.
    model = encoder.model.float().train()
    use_amp = device.startswith("cuda") and torch.cuda.is_bf16_supported()
    if hasattr(model, "gradient_checkpointing_enable"):
        model.gradient_checkpointing_enable()
    optimizer = torch.optim.AdamW(model.parameters(), lr=lr, weight_decay=.01)
    optimizer.zero_grad(set_to_none=True)
    output.mkdir(parents=True)
    atomic_json(output / "training_config.json", {"base_model": model_id, "base_revision": revision,
                "batch_size": batch_size, "epochs": epochs, "lr": lr, "seed": seed,
                "gradient_accumulation": accumulation, "max_optimizer_steps": max_steps,
                "loss": "0.5*dense_multi_positive + 0.5*mean_maxsim_multi_positive",
                "blocklist": str(blocklist) if blocklist else None})
    micro_step = optimizer_step = 0
    with (output / "metrics.jsonl").open("w") as log:
        for epoch in range(epochs):
            random.shuffle(records)
            for start in range(0, len(records), batch_size):
                batch = records[start:start+batch_size]
                if len(batch) < 2:
                    continue
                q_inputs = encoder.inputs([r["query"] for r in batch], "query")
                d_inputs = encoder.inputs([r["positive"] for r in batch], "document")
                with torch.autocast("cuda", dtype=torch.bfloat16) if use_amp else contextlib.nullcontext():
                    q, d = model(**q_inputs), model(**d_inputs)
                dense = F.normalize(q.dense_embeddings.float(), dim=-1) @ F.normalize(d.dense_embeddings.float(), dim=-1).T
                qmask, dmask = q_inputs["attention_mask"].bool(), d_inputs["attention_mask"].bool()
                late = torch.stack([torch.stack([
                    (F.normalize(q.embeddings[i][qmask[i]].float(), dim=-1) @ F.normalize(d.embeddings[j][dmask[j]].float(), dim=-1).T).max(dim=1).values.mean()
                    for j in range(len(batch))]) for i in range(len(batch))])
                known = [set(r.get("positive_ids", [])) | {r["positive"]["unit_id"]} for r in batch]
                positive = torch.tensor([[rj["positive"]["unit_id"] in known[i] for rj in batch]
                                         for i in range(len(batch))], device=device)
                # Same-paper passages are not automatically positives; mask uncertain negatives.
                allowed = torch.tensor([[ri["group_id"] != rj["group_id"] for rj in batch] for ri in batch], device=device) | positive
                if bool((allowed & ~positive).sum() == 0):
                    continue  # no learning signal; use a more diverse batch sampler
                loss = .5 * multipositive_loss(dense, positive, allowed) + .5 * multipositive_loss(late, positive, allowed)
                if not bool(torch.isfinite(loss)):
                    raise ValueError("Non-finite training loss")
                (loss / accumulation).backward()
                micro_step += 1
                if micro_step % accumulation == 0:
                    torch.nn.utils.clip_grad_norm_(model.parameters(), 1.0)
                    optimizer.step()
                    optimizer.zero_grad(set_to_none=True)
                    optimizer_step += 1
                    log.write(json.dumps({"optimizer_step": optimizer_step, "micro_step": micro_step,
                                          "loss": float(loss.detach()), "epoch": epoch}) + "\n")
                    log.flush()
                if optimizer_step >= max_steps:
                    break
            if optimizer_step >= max_steps:
                break
        # Do not silently drop final partial accumulation.
        remainder = micro_step % accumulation
        if remainder:
            for parameter in model.parameters():
                if parameter.grad is not None:
                    parameter.grad.mul_(accumulation / remainder)
            torch.nn.utils.clip_grad_norm_(model.parameters(), 1.0)
            optimizer.step()
            optimizer_step += 1
    model.save_pretrained(output / "checkpoint", safe_serialization=True)
    encoder.processor.save_pretrained(output / "checkpoint")
    return {"output": str(output), "optimizer_steps": optimizer_step,
            "notice": "Evaluate on held-out groups before corpus re-embedding. No performance claim is implied."}
