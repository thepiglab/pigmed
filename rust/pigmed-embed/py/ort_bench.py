"""Control row: the same exported ONNX graphs driven from Python onnxruntime, with the *Python* preprocessing
(the HF processors). It separates "ONNX Runtime vs PyTorch" from "Rust vs Python glue":

  py (torch)  vs  py-ort  -> effect of the inference engine
  py-ort      vs  rs      -> effect of moving preprocessing / pooling / glue to Rust

Same CLI and output format as py_bench.py; tag prefix `pyort-`.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import resource
import time
from pathlib import Path

import numpy as np

from py_bench import load_inputs, lscpu_model


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--model", choices=["gemma", "neomme"], required=True)
    ap.add_argument("--modality", choices=["text", "image"], required=True)
    ap.add_argument("--threads", type=int, required=True)
    ap.add_argument("--batch", type=int, required=True)
    ap.add_argument("--n", type=int, default=32)
    ap.add_argument("--warmup", type=int, default=1)
    ap.add_argument("--max-len", type=int, default=512)
    ap.add_argument("--inputs", type=Path, required=True)
    ap.add_argument("--onnx", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--save", action="store_true")
    args = ap.parse_args()

    import onnxruntime as ort
    import torch  # only for the HF processors' tensor outputs

    torch.set_num_threads(args.threads)
    so = ort.SessionOptions()
    so.intra_op_num_threads = args.threads
    so.inter_op_num_threads = 1
    so.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL

    def sess(name):
        return ort.InferenceSession(str(args.onnx / name), so, providers=["CPUExecutionProvider"])

    models = json.load(open(Path(os.environ["EMBED_ROOT"]) / "models.json"))
    ids, items = load_inputs(args.inputs, args.modality, args.n)
    tag = f"pyort-{args.model}-{args.modality}-t{args.threads}-b{args.batch}"
    t_load = time.perf_counter()

    if args.model == "gemma":
        from transformers import AutoProcessor

        proc = AutoProcessor.from_pretrained(models["google/embeddinggemma-2"]["path"])
        lm = sess("gemma_lm.onnx")
        vis = sess("gemma_vision.onnx") if args.modality == "image" else None
        image_id = proc.tokenizer.convert_tokens_to_ids("<|image|>")

        def run(batch):
            if args.modality == "text":
                enc = proc.tokenizer(batch, padding=True, truncation=True, max_length=args.max_len, return_tensors="np")
                ids_, am = enc["input_ids"].astype(np.int64), enc["attention_mask"].astype(np.int64)
                mm = np.zeros((*ids_.shape, 512), np.float32)
                mmask = np.zeros(ids_.shape, bool)
            else:
                enc = proc(images=[[im] for im in batch], return_tensors="np", padding=True)
                ids_, am = enc["input_ids"].astype(np.int64), enc["attention_mask"].astype(np.int64)
                soft, smask = vis.run(None, {"pixel_values": enc["pixel_values"].astype(np.float32),
                                             "image_position_ids": enc["image_position_ids"].astype(np.int64)})
                mmask = ids_ == image_id
                mm = np.zeros((*ids_.shape, 512), np.float32)
                for b in range(len(batch)):
                    mm[b, mmask[b]] = soft[b][smask[b]]
            (h,) = lm.run(None, {"input_ids": ids_, "attention_mask": am, "mm_embeds": mm, "mm_mask": mmask})
            m = am[..., None].astype(np.float32)
            v = (h * m).sum(1) / np.clip(m.sum(1), 1e-9, None)
            v /= np.clip(np.linalg.norm(v, axis=-1, keepdims=True), 1e-12, None)
            return {"dense": v, "ntok": int(am.sum())}
    else:
        from transformers import NeoMMEProcessor

        proc = NeoMMEProcessor.from_pretrained(models["Hcompany/NeoMME-260M-Retriever"]["path"])
        s = sess("neomme_text.onnx" if args.modality == "text" else "neomme_image.onnx")
        img_id, doc_id, pad_id = (proc.tokenizer.convert_tokens_to_ids(t) for t in ("<img>", "<doc>", "<pad>"))

        def run(batch):
            if args.modality == "text":
                msgs = [[{"role": "user", "content": t}] for t in batch]
                kw = {"padding": "longest", "truncation": True, "max_length": args.max_len}
            else:
                msgs = [[{"role": "user", "content": [{"type": "image", "image": im}]}] for im in batch]
                kw = {"padding": "longest"}
            enc = proc.apply_chat_template(msgs, task="document", tokenize=True, return_dict=True,
                                           return_tensors="np", processor_kwargs=kw)
            ids_, am = enc["input_ids"].astype(np.int64), enc["attention_mask"].astype(np.int64)
            feeds = {"input_ids": ids_, "attention_mask": am}
            if args.modality == "image":
                prev = np.pad(ids_[:, :-1], ((0, 0), (1, 0)), constant_values=pad_id)
                pm = (ids_ == img_id) & (prev != doc_id)
                pat = np.zeros((*ids_.shape, enc["pixel_values"].shape[-1]), np.float32)
                pat[pm] = enc["pixel_values"]
                feeds.update(position_ids=np.asarray(enc["position_ids"], np.int64), patches=pat, patch_mask=pm)
            dense, late = s.run(None, feeds)
            return {"dense": dense, "late": late, "mask": am, "ntok": int(am.sum())}

    load_s = time.perf_counter() - t_load
    batches = [items[i:i + args.batch] for i in range(0, len(items), args.batch)]
    for b in batches[: args.warmup]:
        run(b)
    outs, lat = [], []
    t0 = time.perf_counter()
    for b in batches:
        st = time.perf_counter()
        outs.append(run(b))
        lat.append(time.perf_counter() - st)
    wall = time.perf_counter() - t0
    res = {
        "tag": tag, "runtime": "python-ort", "model": args.model, "modality": args.modality,
        "threads": args.threads, "batch": args.batch, "n": len(items), "warmup_batches": args.warmup,
        "wall_s": wall, "items_per_s": len(items) / wall, "batch_latency_s": lat, "load_s": load_s,
        "peak_rss_mb": resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024,
        "tokens_total": sum(o["ntok"] for o in outs), "dtype": "float32", "onnxruntime": ort.__version__,
        "cpu": lscpu_model(), "host": platform.node(), "job": os.environ.get("SLURM_JOB_ID"),
    }
    args.out.mkdir(parents=True, exist_ok=True)
    if args.save:
        np.save(args.out / f"{tag}.dense.npy", np.concatenate([o["dense"] for o in outs]).astype(np.float32))
        if "late" in outs[0]:
            late, offs = [], [0]
            for o in outs:
                for v, m in zip(o["late"], o["mask"]):
                    keep = v[m.astype(bool)]
                    late.append(keep)
                    offs.append(offs[-1] + len(keep))
            np.save(args.out / f"{tag}.late.npy", np.concatenate(late).astype(np.float32))
            np.save(args.out / f"{tag}.late_offsets.npy", np.array(offs, dtype=np.int64))
        json.dump(ids, open(args.out / f"{tag}.ids.json", "w"))
    json.dump(res, open(args.out / f"{tag}.json", "w"), indent=1)
    print(json.dumps({k: v for k, v in res.items() if k != "batch_latency_s"}))


if __name__ == "__main__":
    main()
