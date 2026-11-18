"""Which resize does the Gemma 4 processor actually do? Compare its pixel_values with candidate re-implementations."""
import json, sys, numpy as np, torch
from pathlib import Path
from PIL import Image
from transformers import AutoProcessor
sys.path.insert(0, ".")
root = Path(sys.argv[1]); snap = sys.argv[2]
proc = AutoProcessor.from_pretrained(snap)
ip = proc.image_processor
print("image processor:", type(ip).__name__, "resample:", ip.resample)
rows = [json.loads(l) for l in open(root / "images.jsonl")]
import torchvision.transforms.v2.functional as tvF
from torchvision.transforms import InterpolationMode
from transformers.models.gemma4.image_processing_gemma4 import get_aspect_ratio_preserving_size
for r in rows[:2] + rows[8:10]:
    im = Image.open(root / r["path"]).convert("RGB")
    enc = ip(images=[im], return_tensors="pt")
    pv = enc["pixel_values"][0]
    th, tw = get_aspect_ratio_preserving_size(im.height, im.width, 16, 2520, 3)
    n = (th // 16) * (tw // 16)
    def patches(arr):  # arr HWC float in [0,1]
        t = torch.from_numpy(arr).permute(2, 0, 1)
        c, h, w = t.shape
        return t.reshape(c, h // 16, 16, w // 16, 16).permute(1, 3, 2, 4, 0).reshape(-1, 768)
    u8 = torch.from_numpy(np.asarray(im)).permute(2, 0, 1)
    cands = {
        "PIL BICUBIC u8": np.asarray(im.resize((tw, th), Image.Resampling.BICUBIC)).astype(np.float32) / 255,
        "tv bicubic aa on u8": tvF.resize(u8, [th, tw], interpolation=InterpolationMode.BICUBIC, antialias=True).permute(1, 2, 0).numpy().astype(np.float32) / 255,
        "tv bicubic aa on float": tvF.resize(u8.float(), [th, tw], interpolation=InterpolationMode.BICUBIC, antialias=True).permute(1, 2, 0).numpy() / 255,
        "tv bicubic aa on float/255": tvF.resize(u8.float() / 255, [th, tw], interpolation=InterpolationMode.BICUBIC, antialias=True).permute(1, 2, 0).numpy(),
    }
    print(f"{r['id']} {im.width}x{im.height} -> {tw}x{th}")
    for k, a in cands.items():
        d = (patches(a) - pv[:n]).abs()
        print(f"   {k:28s} max|d|={d.max():.5f} mean|d|={d.mean():.6f}")
