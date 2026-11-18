"""Diagnose the audio-tower ONNX export: time torch.export (dynamic vs static) and the ONNX translation."""
import json, os, sys, time
from pathlib import Path
import numpy as np, torch
sys.path.insert(0, str(Path(__file__).parent))
import export_onnx as E
torch.set_grad_enabled(False)
from transformers import AutoModel, AutoProcessor
snap = json.load(open(Path(os.environ["EMBED_ROOT"]) / "models.json"))["google/embeddinggemma-2"]["path"]
proc = AutoProcessor.from_pretrained(snap)
model = AutoModel.from_pretrained(snap, dtype=torch.float32, attn_implementation="eager", vision_config=None).eval()
t = np.arange(16000 * 7) / 16000
wav = (0.3 * np.sin(2 * np.pi * (200 + 150 * t) * t)).astype(np.float32)
f = proc.feature_extractor([wav], return_tensors="pt")
au = E.GemmaAudio(model).eval()
args = (f["input_features"], f["input_features_mask"])
for mode in sys.argv[1:]:
    t0 = time.time()
    if mode == "static-export":
        ep = torch.export.export(au, args, strict=False)
        print("torch.export static ok", round(time.time() - t0, 1), "s", flush=True)
    elif mode == "dynamic-export":
        T = torch.export.Dim("frames", min=int(os.environ.get("TMIN", "49")), max=int(os.environ.get("TMAX", "200000")))
        ep = torch.export.export(au, args, dynamic_shapes=({1: T}, {1: T}), strict=False)
        print("torch.export dynamic ok", round(time.time() - t0, 1), "s", flush=True)
    elif mode == "static-onnx":
        prog = torch.onnx.export(au, args, dynamo=True, input_names=["input_features", "input_features_mask"],
                                 output_names=["soft_tokens", "soft_mask"], optimize=True, external_data=True)
        print("onnx static ok", round(time.time() - t0, 1), "s", flush=True)
if "dynamic-onnx" in sys.argv[1:]:
    t0 = time.time()
    T = torch.export.Dim("frames", min=int(os.environ.get("TMIN", "49")), max=int(os.environ.get("TMAX", "200000")))
    prog = torch.onnx.export(au, args, dynamo=True, input_names=["input_features", "input_features_mask"],
                             output_names=["soft_tokens", "soft_mask"], dynamic_shapes=({1: T}, {1: T}),
                             optimize=True, external_data=True)
    print("onnx dynamic ok", round(time.time() - t0, 1), "s", flush=True)
if "dynamic48" in sys.argv[1:]:
    t0 = time.time()
    K = torch.export.Dim("k48", min=1, max=4000)
    T = 48 * K + 1
    # pad the traced example to 48k+1 frames with masked zero frames
    x, m = args
    n = x.shape[1]; tgt = ((n - 1 + 47) // 48) * 48 + 1
    xp = torch.nn.functional.pad(x, (0, 0, 0, tgt - n)); mp = torch.nn.functional.pad(m, (0, tgt - n), value=False)
    ref = au(x, m); got = au(xp, mp)
    nv = int(ref[1].sum())
    print("pad invariance: max|d| over valid soft tokens", float((ref[0][ref[1]] - got[0][:, :ref[0].shape[1]][ref[1]]).abs().max()), "valid", nv, int(got[1].sum()), flush=True)
    prog = torch.onnx.export(au, (xp, mp), dynamo=True, input_names=["input_features", "input_features_mask"],
                             output_names=["soft_tokens", "soft_mask"], dynamic_shapes=({1: T}, {1: T}),
                             optimize=True, external_data=True)
    print("onnx dynamic48 ok", round(time.time() - t0, 1), "s", flush=True)
