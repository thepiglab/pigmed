"""S2 contextual pieces on a tiny random EG2 (CPU, fp32): article windows and spans, chunk pooling, the InSeNT /
pplx loss, and the contextual GradCache step == a direct backward. Run from train/: python -m pytest -q tests/test_contextual.py"""
import math
from types import SimpleNamespace

import pytest
import torch
import torch.nn.functional as F

from tests.tiny import make_tiny, real_snapshot

pytestmark = pytest.mark.skipif(real_snapshot() is None, reason="no EG2 snapshot for tokenizer files")

WORDS = ("alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau upsilon "
         "phi chi psi omega").split()


def text(seed, n):
    g = torch.Generator().manual_seed(seed)
    return " ".join(WORDS[int(i)] for i in torch.randint(0, len(WORDS), (n,), generator=g))


@pytest.fixture(scope="module")
def tiny(tmp_path_factory):
    return make_tiny(tmp_path_factory.mktemp("eg2") / "tiny", vision=False)


@pytest.fixture(scope="module")
def model(tiny):
    from eg2late.model import EG2Dual
    torch.manual_seed(0)
    m = EG2Dual.load(tiny)
    m.late = type(m.late)(d_in=m.backbone.config.embedding_dim)  # random (non-zero) late head
    torch.nn.init.normal_(m.late.down.weight, std=0.1)
    return m.train()


def article(i, n_chunks=4, words=12):
    from eg2late.contextual import article_input
    chunks = [{"id": f"a{i}c{j}", "text": text(100 * i + j, words), "heading": "Methods" if j == 1 else None}
              for j in range(n_chunks)]
    return article_input(f"Title {i}", text(999 + i, 10), chunks), chunks


def test_spans_and_windows(tiny):
    from eg2late.contextual import build_article
    from eg2late.model import Tok
    tk = Tok(tiny).tk
    row, chunks = article(0, n_chunks=6, words=20)
    assert row["text"].startswith("title: Title 0 | text: ")
    for s, c in zip(row["spans"], chunks):
        assert row["text"][s["start"]:s["end"]].endswith(c["text"])
    one = build_article(tk, row)  # fits one window
    assert len(one["wins"]) == 1
    for (w, lo, hi), c in zip(one["place"], chunks):
        assert c["text"].split()[-1] in tk.decode(one["wins"][w][lo:hi])
    many = build_article(tk, row, wmax=64, overlap=16)  # forces several windows, header repeated in each
    assert len(many["wins"]) > 1
    hdr = tk(row["text"][:row["prefix_chars"]], add_special_tokens=False)["input_ids"]
    for w in many["wins"]:
        assert len(w) <= 64
        assert tk.decode(w[1:1 + len(hdr) - 2]).strip() in tk.decode(w)  # header (title + abstract) on top
    for (w, lo, hi), c in zip(many["place"], chunks):
        assert hi > lo and hi <= len(many["wins"][w])


def test_chunk_pooling_matches_manual(model, tiny):
    from eg2late.contextual import build_article, encode_chunks
    from eg2late.model import Tok
    tk = Tok(tiny).tk
    row, _ = article(1)
    art = build_article(tk, row)
    wins = [torch.tensor(w) for w in art["wins"]]
    with torch.no_grad():
        d, lt, m = encode_chunks(model, wins, art["place"], late_len=16)
        h = model.token_outputs(wins[0][None], torch.ones(1, len(wins[0]), dtype=torch.long))[0]
    for c, (_, lo, hi) in enumerate(art["place"]):
        assert torch.allclose(d[c], F.normalize(h[lo:hi].mean(0), dim=-1), atol=1e-5)
        assert int(m[c].sum()) == min(16, hi - lo)


def _args(**kw):
    a = dict(lam_seq=0.1, beta=0.3, seq_in_batch="exclude", tau_d=0.05, tau_l=0.05, tau_t=0.05, w_dense=1.0,
             w_late=1.0, w_align=0.1, w_kd=0.5, mb_q=2, chunk_len=16)
    a.update(kw)
    return SimpleNamespace(**a)


def _batch(tiny, n_art=3, start=0, qj=(0, 2)):
    from eg2late.contextual import build_article
    from eg2late.model import QUERY_PROMPT, Tok
    from eg2late.train_s1 import sid
    tok = Tok(tiny)
    wins, place, cdoc, chash, qs, lab, qdoc, gold = [], [], [], [], [], [], [], []
    for i in range(start, start + n_art):
        row, chunks = article(i)
        art = build_article(tok.tk, row, wmax=48, overlap=8)
        base = len(wins)
        wins += [torch.tensor(w) for w in art["wins"]]
        for (w, lo, hi), cid in zip(art["place"], art["ids"]):
            place.append((base + w, lo, hi))
            cdoc.append(sid(f"art{i}"))
            chash.append(sid(cid))
        for j in qj:  # queries per article, each answered by one chunk
            qs.append(QUERY_PROMPT + " ".join(chunks[j]["text"].split()[:4]))
            lab.append(len(chash) - len(chunks) + j)
            qdoc.append(sid(f"art{i}"))
            gold.append([chash[lab[-1]]] + [-1] * 3)
    q = tok(qs, len(QUERY_PROMPT), 32)
    q.pop("offsets")
    g = torch.Generator().manual_seed(1)
    return {"q": q, "wins": wins, "place": place, "chunk_doc": torch.tensor(cdoc), "chunk_hash": torch.tensor(chash),
            "q_doc": torch.tensor(qdoc), "lab": torch.tensor(lab), "gold": torch.tensor(gold),
            "tq": torch.randn(len(qs), 8, generator=g), "tc": torch.randn(len(chash), 8, generator=g)}


def test_loss_reduces_to_plain_ce():
    from eg2late.contextual import ctx_loss
    torch.manual_seed(0)
    Q, C, D = 4, 6, 8
    qd, cd = F.normalize(torch.randn(Q, D), dim=-1), F.normalize(torch.randn(C, D), dim=-1)
    ql = F.normalize(torch.randn(Q, 3, D), dim=-1)
    cl = F.normalize(torch.randn(C, 5, D), dim=-1)
    qm, cm = torch.ones(Q, 3, dtype=torch.bool), torch.ones(C, 5, dtype=torch.bool)
    lab = torch.tensor([0, 2, 3, 5])
    cdoc = torch.tensor([7, 7, 7, 9, 9, 9])
    qdoc = torch.tensor([7, 7, 9, 9])
    forbid = torch.zeros(Q, C, dtype=torch.bool)
    a = _args(lam_seq=0.0, beta=0.0, seq_in_batch="include", w_late=0.0, w_align=0.0)
    out = ctx_loss(qd, ql, qm, cd, cl, cm, lab, cdoc, qdoc, forbid, a)
    assert torch.allclose(out["dense"], F.cross_entropy(qd @ cd.T / a.tau_d, lab))
    a = _args(lam_seq=1.0, beta=0.0, w_late=0.0, w_align=0.0)
    out = ctx_loss(qd, ql, qm, cd, cl, cm, lab, cdoc, qdoc, forbid, a)
    z = (qd @ cd.T / a.tau_d).masked_fill(qdoc[:, None] != cdoc[None, :], -1e4)
    assert torch.allclose(out["dense"], F.cross_entropy(z, lab))
    a = _args(beta=1.0, w_late=0.0, w_align=0.0)
    out = ctx_loss(qd, ql, qm, cd, cl, cm, lab, cdoc, qdoc, forbid, a)
    s = qd @ cd.T / a.tau_d
    docs = torch.stack([s[:, :3].amax(1), s[:, 3:].amax(1)], 1)
    assert torch.allclose(out["dense_doc"], F.cross_entropy(docs, torch.tensor([0, 0, 1, 1])))


def test_gradcache_equals_direct(model, tiny):
    from eg2late.contextual import ctx_loss, ctx_targets, encode_chunks, ctx_train_step
    from eg2late.train_s1 import encode
    batch = _batch(tiny)
    a = _args()
    params = [p for p in model.parameters() if p.requires_grad]
    model.zero_grad()
    q = batch["q"]
    qd, ql = encode(model, q, 64, grad=True, dev="cpu")
    cd, cl, cm = encode_chunks(model, batch["wins"], batch["place"], a.chunk_len, grad=True)
    forbid = ctx_targets(batch["lab"], batch["gold"], batch["chunk_hash"], torch.ones(len(cd), dtype=torch.bool))
    out = ctx_loss(qd, ql.float(), q["late_mask"], cd, cl, cm, batch["lab"], batch["chunk_doc"], batch["q_doc"],
                   forbid, a, batch["tq"] @ batch["tc"].T)
    out["total"].backward()
    ref = [p.grad.clone() for p in params]
    model.zero_grad()
    got = ctx_train_step(model, batch, a)
    assert abs(got["total"] - float(out["total"])) < 1e-4
    num = sum(float((p.grad - r).norm() ** 2) for p, r in zip(params, ref))
    den = sum(float(r.norm() ** 2) for r in ref)
    assert (num / den) ** 0.5 < 1e-4, (num / den) ** 0.5


def test_ctx_data_and_step(model, tiny, tmp_path):
    """train_s2's on-disk format -> plan -> CtxData batch with exactly q_per_rank queries -> one GradCache step."""
    import gzip
    import json
    from eg2late.contextual import ctx_train_step
    from eg2late.model import Tok
    from eg2late.train_s2 import CtxData, make_ctx_plan
    root = tmp_path / "ctx"
    root.mkdir()
    with gzip.open(root / "articles.jsonl.gz", "wt") as f, open(root / "ctx_queries.jsonl", "w") as fq:
        for i in range(4):
            chunks = [{"id": f"a{i}c{j}", "heading": None, "text": text(50 * i + j, 15)} for j in range(5)]
            f.write(json.dumps({"article_id": f"a{i}", "group": f"g{i}", "title": f"T{i}", "abstract": text(7 + i, 8),
                                "chunks": chunks}) + "\n")
            for j in range(3):
                fq.write(json.dumps({"id": f"q{i}{j}", "text": " ".join(chunks[j]["text"].split()[:4]),
                                     "article_id": f"a{i}", "pos": [f"a{i}c{j}", "missing"]}) + "\n")
    a = _args(q_per_rank=5, q_per_article=2, wmax=64, overlap=16, q_len=32)
    probe = CtxData(root, Tok(tiny), a, [])
    qc = {k: len(v) for k, v in probe.qs.items()}
    assert qc == {f"a{i}": 3 for i in range(4)}
    plan = make_ctx_plan(probe.order, qc, a.q_per_rank, a.q_per_article, 3, 0)
    assert all(sum(min(2, qc[x]) for x in arts) >= 5 for arts, _ in plan)
    ds = CtxData(root, Tok(tiny), a, plan)
    b = ds[0]
    assert b["q"]["input_ids"].shape[0] == 5 and len(b["lab"]) == 5
    assert all(0 <= int(x) < len(b["place"]) for x in b["lab"])
    model.zero_grad()
    out = ctx_train_step(model, b, a)
    assert math.isfinite(out["total"]) and any(p.grad is not None for p in model.parameters())


def test_ids_only_articles_join_text(tmp_path):
    pa = pytest.importorskip("pyarrow")
    import pyarrow.parquet as pq
    from eg2late.train_s2 import join_chunk_text
    (tmp_path / "chunks").mkdir()
    pq.write_table(pa.table({"unit_id": ["u1", "u2", "u3"], "text": ["one", "two", "three"], "kind": ["text"] * 3}),
                   tmp_path / "chunks" / "s0.parquet")
    arts = {"a": {"chunks": [{"id": "u1"}, {"id": "u3", "text": "kept"}]}, "b": {"chunks": [{"id": "u2"}]}}
    assert join_chunk_text(arts, tmp_path / "chunks") == 2
    assert [c["text"] for c in arts["a"]["chunks"]] == ["one", "kept"] and arts["b"]["chunks"][0]["text"] == "two"
    with pytest.raises(ValueError):
        join_chunk_text({"c": {"chunks": [{"id": "nope"}]}}, tmp_path / "chunks")


def test_ctx_sharded_layout(tiny, tmp_path):
    """ctx/shards/<name>/{articles.jsonl.gz (ids-only), chunk_text.parquet, ctx_queries.jsonl.gz}: per-shard join."""
    pa = pytest.importorskip("pyarrow")
    import gzip
    import json
    import pyarrow.parquet as pq
    from eg2late.model import Tok
    from eg2late.train_s2 import CtxData
    for s in ("pmcv2-a-00001", "pmcv2-b-00002"):
        d = tmp_path / "shards" / s
        d.mkdir(parents=True)
        ids, texts = [], []
        with gzip.open(d / "articles.jsonl.gz", "wt") as f, gzip.open(d / "ctx_queries.jsonl.gz", "wt") as fq:
            for i in range(2):
                ch = [{"id": f"{s}-{i}-{j}", "heading": "Results", "section": "Results > A"} for j in range(3)]
                ids += [c["id"] for c in ch]
                texts += [text(len(ids) + j, 10) for j in range(3)]
                f.write(json.dumps({"article_id": f"{s}-{i}", "group": f"{s}-{i}", "title": "T", "abstract": "A b c.",
                                    "chunks": ch}) + "\n")
                fq.write(json.dumps({"id": f"q-{s}-{i}", "text": "alpha beta", "article_id": f"{s}-{i}",
                                     "pos": [ch[1]["id"]], "kind": "abssent"}) + "\n")
        pq.write_table(pa.table({"unit_id": ids, "text": texts}), d / "chunk_text.parquet")
    a = _args(q_per_rank=2, q_per_article=1, wmax=256, overlap=32, q_len=16)
    ds = CtxData(tmp_path / "shards", Tok(tiny), a, [])
    assert len(ds.arts) == 4 and len(ds.order) == 4
    assert all(c["text"] for r in ds.arts.values() for c in r["chunks"])
    with gzip.open(tmp_path / "ctx_queries.jsonl.gz", "wt") as f:  # root-level queries (e.g. kind=abssent)
        f.write(json.dumps({"id": "qroot", "text": "gamma", "article_id": "pmcv2-a-00001-0",
                            "pos": ["pmcv2-a-00001-0-2"], "kind": "abssent"}) + "\n")
    whole = CtxData(tmp_path, Tok(tiny), a, [])  # ctx/ root: shards/ + root queries
    assert len(whole.arts) == 4 and len(whole.qs["pmcv2-a-00001-0"]) == 2
    a.ctx_shards = "1"
    assert len(CtxData(tmp_path / "shards", Tok(tiny), a, []).arts) == 2
