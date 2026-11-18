"""S3 image path on a tiny random EG2 (CPU): EG2Dual.forward_mm == transformers' EmbeddingGemma2Model, LoRA on the
vision tower (identity at init, merged export == adapted model), export/reload round trip with vision weights.
Run from train/: python -m pytest -q tests/test_mm.py"""
import pytest
import torch
import torch.nn.functional as F

from tests.tiny import make_tiny, real_snapshot

pytestmark = pytest.mark.skipif(real_snapshot() is None, reason="no EG2 snapshot for tokenizer/processor files")


@pytest.fixture(scope="module")
def tiny(tmp_path_factory):
    return make_tiny(tmp_path_factory.mktemp("eg2") / "tiny")


def _inputs(tiny):
    from PIL import Image
    from transformers import AutoProcessor
    p = AutoProcessor.from_pretrained(tiny)
    g = torch.Generator().manual_seed(0)
    imgs = [Image.fromarray((torch.rand(96, 128, 3, generator=g) * 255).byte().numpy()),
            Image.fromarray((torch.rand(150, 60, 3, generator=g) * 255).byte().numpy())]
    tok = p.tokenizer
    x = p(text=[tok.image_token, "a figure caption " + tok.image_token], images=[[imgs[0]], [imgs[1]]], return_tensors="pt")
    return p, x


def test_forward_mm_matches_transformers(tiny):
    from transformers import AutoModel
    from eg2late.model import EG2Dual
    p, x = _inputs(tiny)
    ref = AutoModel.from_pretrained(tiny, dtype=torch.float32).eval()
    m = EG2Dual.load(tiny, vision=True).eval()
    lm = x["input_ids"] == p.image_token_id
    with torch.no_grad():
        h = ref(input_ids=x["input_ids"], attention_mask=x["attention_mask"], pixel_values=x["pixel_values"],
                image_position_ids=x["image_position_ids"]).last_hidden_state
        am = x["attention_mask"].unsqueeze(-1).float()
        ref_dense = F.normalize((h * am).sum(1) / am.sum(1), dim=-1)
        dense, late = m.forward_mm(x["input_ids"], x["attention_mask"], lm, x["pixel_values"], x["image_position_ids"])
    assert torch.allclose(dense, ref_dense, atol=1e-5), (dense - ref_dense).abs().max()
    assert late.shape[-1] == 128 and torch.equal((late.abs().sum(-1) > 0), lm)
    # the image actually matters
    with torch.no_grad():
        d2, _ = m.forward_mm(x["input_ids"], x["attention_mask"], lm, x["pixel_values"] * 0.5, x["image_position_ids"])
    assert (d2 - dense).abs().max() > 1e-4


def test_lora_identity_then_merged_export(tiny, tmp_path):
    from eg2late.lora import lora_parameters
    from eg2late.model import EG2Dual
    p, x = _inputs(tiny)
    m = EG2Dual.load(tiny, vision=True).eval()
    lm = x["input_ids"] == p.image_token_id
    args = (x["input_ids"], x["attention_mask"], lm, x["pixel_values"], x["image_position_ids"])
    with torch.no_grad():
        d0, _ = m.forward_mm(*args)
        hits = m.vision_mode("lora", r=4, alpha=8)
        assert len(hits) == 7  # q/k/v/o + gate/up/down in the single tiny layer
        d1, _ = m.forward_mm(*args)
        assert torch.allclose(d0, d1, atol=1e-6)  # B = 0 at init
        for q in lora_parameters(m.vision):
            q.normal_(std=0.05)
        d2, l2 = m.forward_mm(*args)
    assert (d2 - d0).abs().max() > 1e-4
    trainable = {n for n, q in m.vision.named_parameters() if q.requires_grad}
    assert trainable and all(n.endswith((".A", ".B")) for n in trainable)
    assert all(q.requires_grad for q in m.embed_vision.parameters())
    out = m.export_snapshot(tmp_path / "exp", {"t": 1})
    m2 = EG2Dual.load(out, vision=True).eval()
    with torch.no_grad():
        d3, l3 = m2.forward_mm(*args)
    assert torch.allclose(d2, d3, atol=2e-2), (d2 - d3).abs().max()  # bf16 export
    assert F.cosine_similarity(d2, d3).min() > 0.999


def test_text_only_export_untouched_vision(tiny, tmp_path):
    from safetensors import safe_open
    from eg2late.model import EG2Dual
    m = EG2Dual.load(tiny)  # text only
    out = m.export_snapshot(tmp_path / "exp")
    with safe_open(tiny / "model.safetensors", "pt") as f0, safe_open(out / "model.safetensors", "pt") as f1:
        ks = [k for k in f0.keys() if k.startswith("vision_tower.")]
        assert ks and all(torch.equal(f0.get_tensor(k), f1.get_tensor(k)) for k in ks)


def _figure_source(root, n=6):
    """A tiny on-disk S3 source: figures.jsonl + JPEGs, caption queries, triples with 2 negatives."""
    import json
    from PIL import Image
    from eg2late.model import QUERY_PROMPT
    (root / "img").mkdir(parents=True)
    g = torch.Generator().manual_seed(3)
    with open(root / "figures.jsonl", "w") as f, open(root / "queries.jsonl", "w") as fq, \
            open(root / "triples.jsonl", "w") as ft:
        for i in range(n):
            h, w = (64 + 16 * i, 96)
            Image.fromarray((torch.rand(h, w, 3, generator=g) * 255).byte().numpy()).save(root / "img" / f"{i}.jpg")
            f.write(json.dumps({"id": f"fig{i}", "article_id": f"a{i // 2}", "group": f"g{i // 2}",
                                "image": f"img/{i}.jpg", "caption": f"figure {i} shows a western blot"}) + "\n")
            fq.write(json.dumps({"id": f"q{i}", "text": QUERY_PROMPT + f"western blot panel {i}"}) + "\n")
            ft.write(json.dumps({"q": f"q{i}", "pos": f"fig{i}", "negs": [f"fig{(i + 1) % n}", f"fig{(i + 3) % n}"],
                                 "group": f"g{i // 2}"}) + "\n")
    (root / "source.json").write_text(json.dumps({"symmetric": True}))
    return root


def _a(tiny, **kw):
    from types import SimpleNamespace
    a = dict(base=str(tiny), n_neg=2, neg_pool=15, q_len=32, d_len=64, mb_q=2, mb_d=3, tau_d=0.05, tau_l=0.05,
             tau_t=0.05, w_dense=1.0, w_late=1.0, w_align=0.1, w_kd_dense=0.0, w_kd_late=0.0, w_rev=1.0, qq_neg=0.0,
             qq_margin=0.1, pack_tokens=0, image_doc_text=None)
    a.update(kw)
    return SimpleNamespace(**a)


def test_image_source_batches_and_gradcache(tiny, tmp_path):
    from eg2late import train_s1 as ts
    from eg2late.model import EG2Dual, Tok
    src = ts.Source(_figure_source(tmp_path / "figs"), None, 0)
    assert src.symmetric and getattr(src.store, "is_image", False) and len(src) == 6
    a = _a(tiny)
    plan = ts.make_plan([src], 3, 0)
    b = ts.Batches([src], plan, Tok(tiny), a)[0]
    d = b["d"]
    assert d["pixel_values"].shape[0] == 9 and torch.equal(d["late_mask"], d["input_ids"] == 258880)
    assert bool(b["sym"].all())
    torch.manual_seed(0)
    m = EG2Dual.load(tiny, vision=True)
    m.vision_mode("lora", r=4, alpha=8)
    torch.nn.init.normal_(m.late.down.weight, std=0.1)
    m.train()
    params = [p for p in m.parameters() if p.requires_grad]
    # direct reference
    m.zero_grad()
    qd, ql = ts.encode(m, b["q"], 64, grad=True, dev="cpu")
    dd, dl = ts.encode_img(m, d, 64, grad=True, dev="cpu")
    forbid, lab, _ = ts.targets(b["gold"], b["doc_hash"], 3, 9, None, None, 1, b["qgroup"], b["dgroup"])
    out = ts.loss_fn(qd, ql, b["q"]["late_mask"], dd, dl, d["late_mask"], forbid, None, lab, a, b["sym"], b["qgroup"])
    assert "rev" in out
    out["total"].backward()
    ref = [p.grad.clone() for p in params]
    m.zero_grad()
    got, _ = ts.train_step(m, b, a, dev="cpu")
    assert abs(got["total"] - float(out["total"])) < 1e-3
    num = sum(float((p.grad - r).norm() ** 2) for p, r in zip(params, ref))
    den = sum(float(r.norm() ** 2) for r in ref)
    assert den > 0 and (num / den) ** 0.5 < 2e-3, (num / den) ** 0.5  # bf16 late-rep cast, see test_dist
    lora_g = [p.grad for n, p in m.vision.named_parameters() if n.endswith(".B")]
    assert lora_g and all(g is not None and g.abs().sum() > 0 for g in lora_g)


def test_figures_eval_runs_on_cpu(tiny, tmp_path, monkeypatch):
    import json
    from types import SimpleNamespace
    from eg2late import evaluate as ev
    monkeypatch.setattr(ev, "DEV", "cpu")
    root = _figure_source(tmp_path / "figs")
    rows = [json.loads(x) for x in open(root / "figures.jsonl")]
    rows[0]["mentions"] = ["as shown in the blot", "the blot in panel A"]
    (root / "figures.jsonl").write_text("".join(json.dumps(r) + "\n" for r in rows))
    a = SimpleNamespace(model=str(tiny), manifest=root / "figures.jsonl", out=tmp_path / "f.json", limit=0, q_len=32,
                        d_len=64, image_doc_text=None)
    with torch.no_grad():
        ev.cmd_figures(a)
    res = json.loads(a.out.read_text())
    assert res["figures"] == 6 and res["mention->figure"]["queries"] == 2
    for k in ("caption->figure", "figure->caption"):
        for h in ("dense", "late"):
            assert 0.0 <= res[k][h]["R@1"] <= res[k][h]["R@10"] <= 1.0
    assert res["caption->figure"]["dense"]["R@10"] == 1.0  # 6 candidates: everything is in the top 10


def test_prep_figs_build_mine_mentions(tiny, tmp_path):
    """pigmed-ctx figure shards -> prep_figs build -> mine.py -> prep_figs mentions -> both load as train_s1 sources."""
    import gzip
    import json
    import subprocess
    import sys
    import numpy as np
    from PIL import Image
    from eg2late import train_s1 as ts
    from eg2late.model import Tok
    g = np.random.default_rng(0)
    for s in ("pmcv2-a-00001", "pmcv2-b-00002"):
        d = tmp_path / "ctx" / "shards" / s
        (d / "img").mkdir(parents=True)
        ids = []
        with gzip.open(d / "figures.jsonl.gz", "wt") as f:
            for i in range(6):
                fid = f"{s}-f{i}"
                ids.append(fid)
                Image.fromarray(g.integers(0, 255, (40, 60, 3), dtype=np.uint8)).save(d / "img" / f"{i}.jpg")
                f.write(json.dumps({"id": fid, "article_id": f"{s}-a{i // 2}", "group": f"{s}-g{i // 2}",
                                    "md5": "dup" if i in (0, 1) else f"{s}{i}",
                                    "image": f"img/{i}.jpg", "caption": f"Figure {i}. western blot of sample {i}",
                                    "mentions": [f"as the blot in Figure {i} shows, the band is clearly visible"]}) + "\n")
        for sub in ("figvec", "capvec"):
            (d / sub).mkdir()
            np.save(d / sub / "embeddings.dense.npy", g.standard_normal((6, 768)).astype(np.float16))
            (d / sub / "embeddings.ids.json").write_text(json.dumps(ids))
    out = tmp_path / "data"
    run = lambda *x: subprocess.run([sys.executable, "-m", *x], check=True, capture_output=True, text=True).stdout  # noqa: E731
    st = json.loads(run("eg2late.prep_figs", "build", "--ctx", str(tmp_path / "ctx"), "--out", str(out), "--check-images"))
    assert st["figures"] == 12 and st["caption_queries"] == 12
    run("eg2late.mine", "--src", str(out / "pmc_fig_caption"), "--top", "8", "--perc", "1.5", "--keep", "4")
    st = json.loads(run("eg2late.prep_figs", "mentions", "--out", str(out)))
    assert st["mentions"] == 12
    a = _a(tiny)
    for name in ("pmc_fig_caption", "pmc_fig_mention"):
        src = ts.Source(out / name, None, 0)
        assert src.symmetric and src.store.is_image and len(src) == 12
        b = ts.Batches([src], ts.make_plan([src], 2, 0), Tok(tiny), a)[0]
        assert b["d"]["pixel_values"].shape[0] == 6
        assert (b["tq"] is not None) == (name == "pmc_fig_caption")
        # same-article figures stay as negatives (no group masking); md5 duplicates are gold (masked)
        assert (b["qgroup"] == -1).all()
    pairs = [json.loads(x) for x in open(out / "pmc_fig_caption" / "pairs.jsonl")]
    dup = [p for p in pairs if p["pos"].endswith("-f0")]
    assert all(len(p["gold"]) == 4 for p in dup)  # f0 and f1 of both shards share md5 "dup"
