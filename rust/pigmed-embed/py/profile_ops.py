"""Per-op time breakdown, torch (card runtime) vs ONNX Runtime (= the Rust runner's engine), FP32, batch 1.

torch: forward hooks on every module (exclusive time per category; hook overhead is ~us per module call).
ORT:   session profiling; node time classified by op type and tensor shapes.
Categories: GEMM attn/other projections, GEMM MLP, attention core (scores/softmax/context/mask), norms, RoPE
(torch only; ORT folds it into elementwise), embeddings/patch stem, elementwise+other, preprocessing.

  python profile_ops.py --case gemma-text --len 512 --threads 16 --onnx DIR --out DIR
  cases: gemma-text, neomme-text, gemma-image, neomme-image  (images use inputs/images/page00.png)
"""

from __future__ import annotations

import argparse
import collections
import json
import os
import shutil
import tempfile
import time
from pathlib import Path

import numpy as np
import torch

torch.set_grad_enabled(False)
INTERMEDIATE = {2048, 3072, 3584}  # gemma text MLP, gemma vision MLP, neomme MLP


def torch_breakdown(model, run, reps):
    cat_of = {}
    for name, m in model.named_modules():
        cls = type(m).__name__
        low = name.lower()
        if isinstance(m, torch.nn.Linear):
            if ".mlp." in low or "mlp" in low.split(".")[-2:]:
                cat_of[m] = "gemm_mlp"
            else:
                cat_of[m] = "gemm_proj"  # q/k/v/o, gates, PLE, patch/embedding projections, heads
        elif "RMSNorm" in cls or "LayerNorm" in cls:
            cat_of[m] = "norm"
        elif "Rotary" in cls:
            cat_of[m] = "rope"
        elif isinstance(m, torch.nn.Embedding):
            cat_of[m] = "embed"
        elif cls.endswith("Attention") and hasattr(m, "q_proj"):
            cat_of[m] = "attn_total"
    t = collections.Counter()
    stack = []

    def pre(m, *_):
        stack.append((m, time.perf_counter()))

    def post(m, *_):
        mm, s = stack.pop()
        dt = time.perf_counter() - s
        c = cat_of[m]
        t[c] += dt
        # inside an attention module, subtract children so attn_core is exclusive
        if c != "attn_total":
            for om, _ in stack:
                if cat_of.get(om) == "attn_total":
                    t["attn_children"] += dt
                    break

    hs = []
    for m in cat_of:
        hs.append(m.register_forward_pre_hook(pre))
        hs.append(m.register_forward_hook(post))
    run()
    t.clear()
    s = time.perf_counter()
    for _ in range(reps):
        run()
    total = (time.perf_counter() - s) / reps
    for h in hs:
        h.remove()
    t = {k: v / reps for k, v in t.items()}
    out = {k: t.get(k, 0.0) for k in ("gemm_proj", "gemm_mlp", "norm", "rope", "embed")}
    out["attn_core"] = t.get("attn_total", 0.0) - t.get("attn_children", 0.0)
    out["other"] = total - sum(out.values())
    return total, out


def _node_meta(path):
    """node name -> (module path, aten op) from the dynamo exporter's metadata_props."""
    import onnx

    m = onnx.load(str(path), load_external_data=False)
    meta = {}
    for n in m.graph.node:
        d = {p.key: p.value for p in n.metadata_props}
        ns = d.get("namespace", "")
        # keep the whole module path: an MLP projection's own module is e.g. "...mlp.down_proj"
        mod = "/".join(x.split(":")[0].strip() for x in ns.split("/")) if ns else ""
        fx = d.get("pkg.torch.onnx.fx_node", "")
        aten = fx.split("target=")[1].split("]")[0].split("(")[0] if "target=" in fx else ""
        meta[n.name] = (mod, aten)
    return meta


def _classify(op, mod, aten, shapes):
    m = mod.lower()
    leaf = m.split("/")[-1]
    if op in ("MatMul", "FusedMatMul", "Gemm", "MatMulNBits") or "linear" in aten or "addmm" in aten:
        if "linear" in aten or "addmm" in aten or (shapes and len(shapes) == 1) or (len(shapes) > 1 and len(shapes[1]) == 2):
            w = shapes[1] if len(shapes) > 1 else []
            mlp = any(k in m for k in (".mlp", "ffw", "feed_forward", "gate_proj", "up_proj", "down_proj")) or (
                not m and any(d in INTERMEDIATE for d in w))  # nodes ORT fused/renamed: tell MLP by its width
            return "gemm_mlp" if mlp else "gemm_proj"
        return "attn_core"  # activation x activation: scores / context
    if op == "Softmax" or "softmax" in aten:
        return "attn_core"
    if op in ("SimplifiedLayerNormalization", "SkipSimplifiedLayerNormalization", "LayerNormalization") or "norm" in leaf.split(".")[-1]:
        return "norm"
    if "rotary" in m or "rotary" in aten:
        return "rope"
    if "self_attn" in m or "attention" in leaf.split(".")[-1]:
        return "attn_core"  # masks, scaling, GQA expand, exclusive-attention terms inside attention modules
    if op == "Gather" or "embedding" in aten:
        return "embed"
    if "patch" in m:
        return "embed"
    return "other"


NO_ARENA = False


def ort_breakdown(path, feeds, threads, reps):
    import onnxruntime as ort

    meta = _node_meta(path)
    so = ort.SessionOptions()
    if NO_ARENA:
        so.enable_cpu_mem_arena = False
        so.enable_mem_pattern = False
    so.intra_op_num_threads = threads
    so.inter_op_num_threads = 1
    so.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL
    tmp = tempfile.mkdtemp(prefix="ortprof-")
    so.enable_profiling = True
    so.profile_file_prefix = os.path.join(tmp, "p")
    s = ort.InferenceSession(str(path), so, providers=["CPUExecutionProvider"])
    s.run(None, feeds)
    t0 = time.perf_counter()
    for _ in range(reps):
        s.run(None, feeds)
    wall = (time.perf_counter() - t0) / reps
    ev = json.load(open(s.end_profiling()))
    shutil.rmtree(tmp, ignore_errors=True)
    runs = [e for e in ev if e.get("cat") == "Session" and e["name"] == "model_run"]
    keep_after = runs[1]["ts"] if len(runs) > 1 else 0  # drop the warmup run
    cat = collections.Counter()
    unmatched = 0
    for e in ev:
        if e.get("cat") != "Node" or not e["name"].endswith("_kernel_time") or e["ts"] < keep_after:
            continue
        name = e["name"][: -len("_kernel_time")]
        op = e["args"].get("op_name", "")
        shapes = [list(d.values())[0] for d in e["args"].get("input_type_shape", [])]
        mod, aten = meta.get(name, ("", ""))
        if name not in meta:
            unmatched += e["dur"]
        cat[_classify(op, mod, aten, shapes)] += e["dur"] / 1e6
    cat = {k: v / reps for k, v in cat.items()}
    out = {k: cat.get(k, 0.0) for k in ("gemm_proj", "gemm_mlp", "norm", "rope", "embed", "attn_core")}
    out["other"] = wall - sum(out.values())  # elementwise ops not inside a categorised module + ORT overhead
    out["unmatched_node_time_s"] = unmatched / 1e6 / reps  # fused nodes renamed by ORT (classified by op/shape)
    return wall, out


def rust_breakdown(a, model_name, mod, length):
    """Run the Rust runner on one real input of this length with ORT profiling; classify node time per category.
    Includes the Rust custom ops (FlashAttention / AudioChunkAttention -> attn_core)."""
    import glob
    import subprocess

    snap = json.load(open(Path(os.environ["EMBED_ROOT"]) / "models.json"))[
        "google/embeddinggemma-2" if model_name == "gemma" else "Hcompany/NeoMME-260M-Retriever"]["path"]
    tmp = Path(tempfile.mkdtemp(prefix="rsprof-"))
    if mod == "text":
        src = a.inputs / f"ctx-{model_name}-L{length}.jsonl"
        sel = ["--texts-file", str(src.relative_to(a.inputs)), "--max-len", "16384"]
    else:
        sel = ["--images-file", "images-res-2048.jsonl"]
    env = dict(os.environ, PIGMED_ORT_PROFILE=str(tmp / "prof"))
    out = subprocess.run([a.rust_bin, "bench", "--model", model_name, "--modality", mod, "--threads", str(a.threads),
                          "--batch", "1", "--n", "1", "--warmup", "2", "--inputs", str(a.inputs), "--onnx", str(a.onnx),
                          "--snapshot", snap, "--out", str(tmp), *sel], env=env, capture_output=True, text=True)
    js = [json.loads(l) for l in out.stdout.splitlines() if l.startswith("{")]
    if not js:
        return {"error": out.stderr[-400:]}
    res = {"total_s": js[0]["wall_s"], "infer_s": js[0]["infer_s"], "prep_post_s": js[0]["prep_post_s"]}
    cat = collections.Counter()
    import re

    # the thread-policy autotune builds one runner (= one profile file per graph) per candidate; the timed runner is
    # built last, so keep only the newest file per graph (ORT appends _<date>_<time with ms>.json)
    last = {}
    for pf in sorted(glob.glob(str(tmp / "prof*.json"))):
        g = re.match(r"prof-(.+?)_\d{4}-\d{2}-\d{2}_", Path(pf).name)
        last[g.group(1) if g else pf] = pf
    for pf in last.values():
        mm = re.match(r"prof-(.+?)_\d{4}-\d{2}-\d{2}_", Path(pf).name)  # ORT appends _<date>_<time>.json
        graph = Path(a.onnx) / f"{mm.group(1)}.onnx" if mm else None
        meta = _node_meta(graph) if graph and graph.exists() else {}
        ev = json.load(open(pf))
        runs = [e for e in ev if e.get("cat") == "Session" and e["name"] == "model_run"]
        keep_after = runs[-1]["ts"] if runs else 0  # last run = the timed one
        for e in ev:
            if e.get("cat") != "Node" or not e["name"].endswith("_kernel_time") or e["ts"] < keep_after:
                continue
            name = e["name"][: -len("_kernel_time")]
            op = e["args"].get("op_name", "")
            shapes = [list(d.values())[0] for d in e["args"].get("input_type_shape", [])]
            mod_, aten = meta.get(name, ("", ""))
            c = "attn_core" if op in ("FlashAttention", "AudioChunkAttention") else _classify(op, mod_, aten, shapes)
            cat[c] += e["dur"] / 1e6
    shutil.rmtree(tmp, ignore_errors=True)
    res.update({k: cat.get(k, 0.0) for k in ("gemm_proj", "gemm_mlp", "norm", "rope", "embed", "attn_core")})
    res["other_ort"] = res["infer_s"] - sum(cat.values())  # ORT framework overhead + unclassified gaps
    res["rust_prep_post"] = res["prep_post_s"]
    return res


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--case", required=True)
    ap.add_argument("--len", type=int, default=512)
    ap.add_argument("--threads", type=int, default=16)
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--onnx", type=Path, required=True)
    ap.add_argument("--inputs", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--skip-ort", action="store_true")
    ap.add_argument("--skip-torch", action="store_true")
    ap.add_argument("--no-arena", action="store_true", help="ORT without CPU memory arena / memory pattern")
    ap.add_argument("--rust-bin", default=None, help="profile the Rust runner (custom-op graphs) instead of python ORT")
    a = ap.parse_args()
    global NO_ARENA
    NO_ARENA = a.no_arena
    torch.set_num_threads(a.threads)
    models = json.load(open(Path(os.environ["EMBED_ROOT"]) / "models.json"))
    rng = np.random.default_rng(0)
    res = {"case": a.case, "len": a.len, "threads": a.threads}
    model_name, mod = a.case.split("-")
    if model_name == "gemma":
        from transformers import AutoModel, AutoProcessor

        snap = models["google/embeddinggemma-2"]["path"]
        proc = AutoProcessor.from_pretrained(snap)
        kw = {"audio_config": None} if mod == "image" else {"audio_config": None, "vision_config": None}
        model = AutoModel.from_pretrained(snap, dtype=torch.float32, **kw).eval()
        if mod == "text":
            ids = torch.from_numpy(rng.integers(1000, 200000, (1, a.len)))
            am = torch.ones_like(ids)
            tp = time.perf_counter()
            proc.tokenizer(["word " * (a.len - 2)], truncation=True, max_length=a.len, return_tensors="pt")
            res["prep_s"] = time.perf_counter() - tp
            run = lambda: model(input_ids=ids, attention_mask=am)
            feeds = [("gemma_lm.onnx", {"input_ids": ids.numpy(), "attention_mask": am.numpy(),
                                        "mm_embeds": np.zeros((1, a.len, 512), np.float32),
                                        "mm_mask": np.zeros((1, a.len), bool)})]
        else:
            from PIL import Image

            im = Image.open(a.inputs / "images/page00.png").convert("RGB")
            tp = time.perf_counter()
            enc = proc(images=[[im]], return_tensors="pt", padding=True)
            res["prep_s"] = time.perf_counter() - tp
            enc = {k: v for k, v in enc.items() if k != "num_soft_tokens_per_image"}
            run = lambda: model(**enc)
            res["len"] = int(enc["input_ids"].shape[1])
            feeds = [("gemma_vision.onnx", {"pixel_values": enc["pixel_values"].numpy(),
                                            "image_position_ids": enc["image_position_ids"].numpy()})]
            ids = enc["input_ids"].numpy()
            feeds.append(("gemma_lm.onnx", {"input_ids": ids, "attention_mask": enc["attention_mask"].numpy(),
                                            "mm_embeds": np.zeros((*ids.shape, 512), np.float32),
                                            "mm_mask": ids == model.config.image_token_id}))
    else:
        from transformers import NeoMMEForRetrieval, NeoMMEProcessor

        snap = models["Hcompany/NeoMME-260M-Retriever"]["path"]
        proc = NeoMMEProcessor.from_pretrained(snap)
        model = NeoMMEForRetrieval.from_pretrained(snap, dtype=torch.float32).eval()
        if mod == "text":
            ids = torch.from_numpy(rng.integers(1000, 120000, (1, a.len)))
            am = torch.ones_like(ids)
            tp = time.perf_counter()
            proc.apply_chat_template([[{"role": "user", "content": "word " * (a.len - 1)}]], task="document",
                                     tokenize=True, return_dict=True, return_tensors="pt",
                                     processor_kwargs={"truncation": True, "max_length": a.len})
            res["prep_s"] = time.perf_counter() - tp
            run = lambda: model(input_ids=ids, attention_mask=am)
            feeds = [("neomme_text.onnx", {"input_ids": ids.numpy(), "attention_mask": am.numpy()})]
        else:
            from PIL import Image

            im = Image.open(a.inputs / "images/page00.png").convert("RGB")
            tp = time.perf_counter()
            enc = proc.apply_chat_template([[{"role": "user", "content": [{"type": "image", "image": im}]}]],
                                           task="document", tokenize=True, return_dict=True, return_tensors="pt",
                                           processor_kwargs={"padding": "longest"})
            res["prep_s"] = time.perf_counter() - tp
            run = lambda: model(**enc)
            ids = enc["input_ids"]
            res["len"] = int(ids.shape[1])
            prev = torch.nn.functional.pad(ids[:, :-1], (1, 0), value=0)
            pm = (ids == model.config.image_token_id) & (prev != model.config.document_token_id)
            pat = np.zeros((*ids.shape, enc["pixel_values"].shape[-1]), np.float32)
            pat[pm.numpy()] = enc["pixel_values"].numpy()
            feeds = [("neomme_image.onnx", {"input_ids": ids.numpy(), "attention_mask": enc["attention_mask"].numpy(),
                                            "position_ids": enc["position_ids"].numpy(), "patches": pat,
                                            "patch_mask": pm.numpy()})]
    if not a.skip_torch:
        tot, br = torch_breakdown(model, run, a.reps)
        res["torch"] = {"total_s": tot, **br}
    res["ort_no_arena"] = a.no_arena
    if a.rust_bin and not a.skip_ort:
        res["rust"] = rust_breakdown(a, model_name, mod, a.len)
    elif not a.skip_ort:
        try:
            ot, ob = 0.0, collections.Counter()  # noqa
            for g, f in feeds:
                w, b = ort_breakdown(a.onnx / g, f, a.threads, a.reps)
                ot += w
                ob.update(b)
            res["ort"] = {"total_s": ot, **ob}
        except Exception as e:  # e.g. out of memory at very long context
            res["ort"] = {"error": f"{type(e).__name__}: {str(e)[:300]}"}
    a.out.mkdir(parents=True, exist_ok=True)
    sfx = ("-torch" if a.skip_ort else "") + ("-ort" if a.skip_torch else "") + ("-noarena" if a.no_arena else "")
    (a.out / f"ops-{a.case}-{res['len']}{sfx}.json").write_text(json.dumps(res, indent=1))
    print(json.dumps(res))


if __name__ == "__main__":
    main()
