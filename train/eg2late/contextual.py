"""S2 contextual training: late chunking of whole PMC articles, both heads.

Input (one article): the document prompt with the real title, the abstract as a shared header, then the body chunks
    text = "title: {title} | text: {abstract}\n\n{heading}\n{chunk}\n\n{heading}\n{chunk}..."
    prefix_chars = len("title: ... | text: {abstract}\n\n")   spans = char ranges of every body chunk
`late_windows` (a port of flashembed-gpu `late_doc`, so serving and training agree) cuts it into windows of <= wmax
tokens with `overlap`, the header repeated at the top of every window; each span is pooled in the window where it is
most central. Chunk vectors:
    dense: mean of the backbone's (projected) token outputs over the span, L2-normalised  (= flashembed text_late)
    late : the late head on the span's tokens (128-d each), scored with mean-over-query MaxSim

Loss (InSeNT, Conti et al. 2025; pplx-embed-context, Perplexity 2026), per head h in {dense, late}:
    local_h = lam_seq * CE(q -> chunks of its own article only) + (1 - lam_seq) * CE(q -> all chunks in the batch)
              (`seq_in_batch`: "exclude" drops same-article chunks from the batch term as ConTEB does; "include" keeps
              them as pplx v1 does)
    doc_h   = CE(q -> articles), article score = max over its chunks (pplx v2's MaxSim over chunk vectors)
    total   = sum_h w_h * [beta * doc_h + (1 - beta) * local_h] + w_align * KL(dense <-> late over chunks)
              + w_kd * KL(teacher || student) over chunks (teacher logits [Q, C] from any hook: base-EG2 chunk
              scores, or a query-aware token-relevance teacher as in pplx v2; temperature tau_t)
Gold chunks other than the label are forbidden (several chunks may answer a query).

GradCache: chunk reps for the whole (gathered) batch are computed without grad, the loss is taken on them, then every
window is re-encoded with grad and its spans' cached gradients are backpropagated (`ctx_train_step`).
"""
from __future__ import annotations

import bisect

import torch
import torch.nn.functional as F

from .model import DOC_PROMPT  # noqa: F401  (the plain prompt; articles use the real title)

ARTICLE_PROMPT = "title: {title} | text: "


def late_windows(tk, text, prefix, spans, wmax=8192, overlap=1024):
    """Port of flashembed-gpu `late_doc`: windows (token ids) and per span (window, lo, hi) token ranges."""
    enc = tk(text, return_offsets_mapping=True, return_special_tokens_mask=True)
    ids, offs, sp = enc["input_ids"], enc["offset_mapping"], enc["special_tokens_mask"]
    n = len(ids)
    lead = 0
    while lead < n and sp[lead]:
        lead += 1
    trail = 0
    while trail < n - lead and sp[n - 1 - trail]:
        trail += 1
    content = list(range(lead, n - trail))
    hdr = []
    for k in content:  # take_while, as in the Rust
        if offs[k][0] >= prefix:
            break
        hdr.append(k)
    body = content[len(hdr):]
    starts_c = [offs[k][0] for k in body]

    placed = []
    for s in spans:
        c0, c1 = s["start"], s["end"]
        b0, b1 = bisect.bisect_left(starts_c, c0), bisect.bisect_left(starts_c, c1)
        if b0 < b1:
            placed.append((b0, b1))
        else:
            b = b0 - 1 if b0 > 0 and offs[body[b0 - 1]][1] > c0 else min(b0, len(body) - 1)
            placed.append((b, b + 1))
    if n <= wmax:
        return [ids], [(0, body[b0], body[b1 - 1] + 1) for b0, b1 in placed]
    head = [ids[k] for k in range(lead)] + [ids[k] for k in hdr]
    tail = [ids[k] for k in range(n - trail, n)]
    cap = wmax - len(head) - len(tail)
    if cap < max(8, wmax // 8):  # 1k of an 8k window
        raise ValueError(f"header of {len(head)} tokens leaves {cap} of a {wmax}-token window (cap the abstract)")
    ov = min(overlap, cap // 2)
    stride = cap - ov
    starts = [0]
    while starts[-1] + cap < len(body):
        starts.append(starts[-1] + stride)
    wins = [head + [ids[k] for k in body[s0:min(s0 + cap, len(body))]] + tail for s0 in starts]
    out = []
    for b0, b1 in placed:
        end = lambda s0: min(s0 + cap, len(body))
        best = None
        for w, s0 in enumerate(starts):
            if s0 <= b0 and b1 <= end(s0):
                v = min(b0 - s0, end(s0) - b1)
                if best is None or v > best[1]:
                    best = (w, v)
        if best is None:
            for w, s0 in enumerate(starts):
                v = max(0, min(b1, end(s0)) - max(b0, s0))
                if best is None or v > best[1]:
                    best = (w, v)
        w = best[0]
        s0 = starts[w]
        x0, x1 = max(b0, s0), min(b1, end(s0))
        out.append((w, len(head) + x0 - s0, len(head) + x1 - s0))
    return wins, out

def article_input(title: str, abstract: str, chunks: list[dict], sep: str = "\n\n",
                  max_header_chars: int = 4000) -> dict:
    """chunks: [{"id", "text", "heading"?}] in reading order (heading = the section heading, given on every chunk) -> {"text", "prefix_chars", "spans": [{"id","start","end"}]}
    (flashembed text_late row format). The header (prompt + title + abstract) is context only, never pooled."""
    abstract = (abstract or "").strip()
    if len(abstract) > max_header_chars:  # the header repeats in every window: cap it (~1k tokens), at a word break
        abstract = abstract[:max_header_chars].rsplit(" ", 1)[0] + " ..."
    head = ARTICLE_PROMPT.format(title=(title or "none").strip()[:500]) + abstract
    text = head + sep
    prefix = len(text)
    spans = []
    prev = None
    for c in chunks:  # a heading is rendered only where it changes (natural reading order), inside the chunk's span
        h = c.get("heading")
        body = (h + "\n" if h and h != prev else "") + c["text"]
        prev = h or prev
        spans.append({"id": c["id"], "start": len(text), "end": len(text) + len(body)})
        text += body + sep
    return {"text": text[: -len(sep)] if chunks else text, "prefix_chars": prefix, "spans": spans}


def build_article(tk, row: dict, wmax: int = 8192, overlap: int = 1024) -> dict:
    """Tokenised article: windows (lists of ids) and per chunk (window, lo, hi), in the order of row["spans"]."""
    wins, place = late_windows(tk, row["text"], row["prefix_chars"], row["spans"], wmax, overlap)
    return {"wins": wins, "place": place, "ids": [s["id"] for s in row["spans"]]}


def _groups(wins, place, budget):
    by_win: dict[int, list[int]] = {}
    for c, (w, _, _) in enumerate(place):
        by_win.setdefault(w, []).append(c)
    order = sorted(by_win, key=lambda w: -len(wins[w]))
    i = 0
    while i < len(order):
        L = len(wins[order[i]])
        n = max(1, min(len(order) - i, budget // L))
        yield L, order[i:i + n], by_win
        i += n


def encode_chunks(model, wins: list[torch.Tensor], place: list[tuple[int, int, int]], late_len: int,
                  grad: bool = False, budget: int = 16384, grads=None, dev=None):
    """wins: token-id tensors (one per window); place[c] = (window, lo, hi) for chunk c.
    Returns dense [C, D] fp32, late [C, late_len, 128] fp32 (zero past each span), mask [C, late_len].
    Windows are batched up to `budget` padded tokens per forward. With `grads` = (g_dense [C, D], g_late
    [C, late_len, 128]) it instead backpropagates sum(rep * grad) one window group at a time (GradCache replay) and
    returns None, so only one group's activations are alive at once. grad=True without `grads` keeps the whole graph
    (tests / direct reference)."""
    dev = dev or next(model.parameters()).device
    C = len(place)
    out_d, out_l = [None] * C, [None] * C
    for L, ws, by_win in _groups(wins, place, budget):
        n = len(ws)
        ids = torch.zeros(n, L, dtype=torch.long)
        am = torch.zeros(n, L, dtype=torch.long)
        for r, w in enumerate(ws):
            ids[r, :len(wins[w])] = wins[w]
            am[r, :len(wins[w])] = 1
        ctx = torch.autocast("cuda", dtype=torch.bfloat16) if dev.type == "cuda" else _null()
        with torch.set_grad_enabled(grad or grads is not None), ctx:
            h = model.token_outputs(ids.to(dev), am.to(dev))
            sur = 0.0
            for r, w in enumerate(ws):
                for c in by_win[w]:
                    _, lo, hi = place[c]
                    t = h[r, lo:hi]
                    dn = F.normalize(t.float().mean(0), dim=-1)
                    lt = model.late(t[:late_len]).float()
                    if grads is not None:
                        sur = sur + (dn * grads[0][c]).sum() + (lt * grads[1][c, :lt.shape[0]]).sum()
                    else:
                        out_d[c], out_l[c] = dn, lt
        if grads is not None:
            sur.backward()
            del h, sur
    if grads is not None:
        return None
    dense = torch.stack(out_d)
    late = torch.stack([F.pad(x, (0, 0, 0, late_len - x.shape[0])) for x in out_l])
    mask = torch.zeros(C, late_len, dtype=torch.bool, device=dev)
    for c in range(C):
        mask[c, :min(late_len, place[c][2] - place[c][1])] = True
    return dense, late, mask


class _null:
    def __enter__(self):
        return self

    def __exit__(self, *a):
        return False


def ctx_loss(qd, ql, qm, cd, cl, cm, lab, chunk_doc, q_doc, forbid, a, teacher=None, maxsim=None, valid=None):
    """qd [Q, D], ql/qm query late; cd [C, D], cl/cm chunk late; lab [Q] label chunk; chunk_doc [C] / q_doc [Q] article
    ids (same id space); forbid [Q, C] gold chunks other than the label. a: lam_seq, beta, seq_in_batch,
    tau_d, tau_l, tau_t, w_dense, w_late, w_align, w_kd. Returns a dict of terms with "total"."""
    big = -1e4
    if maxsim is None:
        from .train_s1 import maxsim_train as maxsim
    same = q_doc[:, None] == chunk_doc[None, :]  # [Q, C]
    docs, inv = torch.unique(chunk_doc, return_inverse=True)  # article index per chunk
    q_lab_doc = torch.searchsorted(docs, q_doc)
    out, total = {}, 0.0
    heads = {"dense": (qd @ cd.T, a.tau_d, a.w_dense), "late": (maxsim(ql, qm, cl, cm).float(), a.tau_l, a.w_late)}
    logp = {}
    for name, (s, tau, w) in heads.items():
        z = (s / tau).masked_fill(forbid, big)
        zb = z.masked_fill(same, big) if a.seq_in_batch == "exclude" else z
        zb = zb.scatter(1, lab[:, None], z.gather(1, lab[:, None]))  # the label always stays in
        zs = z.masked_fill(~same, big)
        local = a.lam_seq * F.cross_entropy(zs, lab) + (1 - a.lam_seq) * F.cross_entropy(zb, lab)
        # article score = max over its chunks (gold chunks of other queries are not masked here: article level)
        sd = torch.full((s.shape[0], len(docs)), big, device=s.device, dtype=s.dtype)
        sv = (s / tau) if valid is None else (s / tau).masked_fill(~valid[None, :], big)
        sd = sd.scatter_reduce(1, inv[None, :].expand(s.shape[0], -1), sv, reduce="amax")
        doc = F.cross_entropy(sd, q_lab_doc)
        out[name] = local
        out[f"{name}_doc"] = doc
        total = total + w * (a.beta * doc + (1 - a.beta) * local)
        logp[name] = F.log_softmax(zb, -1)
        with torch.no_grad():
            out[f"acc_{name[0]}"] = (z.argmax(-1) == lab).float().mean()
            out[f"acc_{name[0]}_seq"] = (zs.argmax(-1) == lab).float().mean()
    kl = lambda p, q: (p.exp() * (p - q)).sum(-1).mean()  # noqa: E731
    out["align"] = 0.5 * (kl(logp["dense"], logp["late"]) + kl(logp["late"], logp["dense"]))
    total = total + a.w_align * out["align"]
    if teacher is not None and a.w_kd > 0:
        pt = F.log_softmax((teacher / a.tau_t).masked_fill(forbid, big), -1)
        out["kd"] = kl(pt, logp["dense"]) + kl(pt, logp["late"])
        total = total + a.w_kd * out["kd"]
    out["total"] = total
    return out


def ctx_targets(lab, gold, chunk_hash, chunk_valid):
    """forbid [Q, C]: gold chunks other than the label, and padding chunks (from uneven gathers)."""
    forbid = (gold[:, :, None] == chunk_hash[None, None, :]).any(1) | ~chunk_valid[None, :]
    forbid[torch.arange(len(lab), device=lab.device), lab] = False
    return forbid


def ctx_train_step(model, batch, a, dev=None):
    """One GradCache step on a contextual batch (gradients accumulate in .grad; the caller clips and steps).
    batch: q (tokenised queries {input_ids, attention_mask, late_mask}), wins [token-id tensors], place
    [(w, lo, hi)] per chunk, chunk_doc [C] / q_doc [Q] article hashes, chunk_hash [C], lab [Q] (local chunk index),
    gold [Q, G] chunk hashes (-1 pad); teacher: tq [Q, Dt] + tc [C, Dt] vectors, or a [Q, C] score matrix over
    this rank's chunks ("teacher", e.g. a token-relevance hook), or neither. Shares chunks across the gather group."""
    from .dist_utils import all_reduce_grads, gather_cat, gather_var, grank, gworld
    from .train_s1 import encode
    dev = dev or next(model.parameters()).device
    q = batch["q"]
    with torch.no_grad():
        qd, ql = encode(model, q, a.mb_q, dev=dev.type)
        qm = q["late_mask"].to(dev)
        cd, cl, cm = encode_chunks(model, batch["wins"], batch["place"], a.chunk_len, dev=dev)
    Q, C, Lq = qd.shape[0], cd.shape[0], ql.shape[1]
    _, qsizes = gather_var(torch.zeros(Q, 1, device=dev))
    if len(set(qsizes)) > 1:
        raise ValueError(f"every rank needs the same number of queries per step, got {qsizes}")
    Qd, Ql, Qm = gather_cat(qd), gather_cat(ql.float(), 1), gather_cat(qm.to(torch.uint8), 1).bool()
    Cd, sizes = gather_var(cd)
    Cl, _ = gather_var(cl)
    Cm, _ = gather_var(cm.to(torch.uint8))
    Cm = Cm.bool()
    cmax = Cd.shape[0] // gworld()
    valid = torch.zeros(Cd.shape[0], dtype=torch.bool, device=dev)
    for r, n in enumerate(sizes):
        valid[r * cmax: r * cmax + n] = True
    ch, _ = gather_var(batch["chunk_hash"].to(dev), value=-2)
    cdoc, _ = gather_var(batch["chunk_doc"].to(dev), value=-3)
    qdoc = gather_cat(batch["q_doc"].to(dev))
    gold = gather_cat(batch["gold"].to(dev))
    lab = torch.cat([gather_cat(batch["lab"].to(dev) + grank() * cmax)])
    teacher = None
    if batch.get("tq") is not None:  # teacher vectors (e.g. base EG2 query / plain-chunk vectors): scores over the group
        Tc, _ = gather_var(batch["tc"].to(dev).float())
        teacher = gather_cat(batch["tq"].to(dev).float()) @ Tc.T
    elif batch.get("teacher") is not None:
        t = batch["teacher"].to(dev).float()  # [Q, C_local]: pad to cmax, gather; other ranks' chunks get -inf-ish
        full = torch.full((Q, Cd.shape[0]), -1e4 * a.tau_t, device=dev)
        full[:, grank() * cmax: grank() * cmax + C] = t
        teacher = gather_cat(full)
    forbid = ctx_targets(lab, gold, ch, valid)
    leaves = [x.detach().requires_grad_(True) for x in (Qd, Ql, Cd, Cl)]
    out = ctx_loss(leaves[0], leaves[1], Qm, leaves[2], leaves[3], Cm, lab, cdoc, qdoc, forbid, a, teacher,
                   valid=valid)
    out["total"].backward()
    r = grank()
    gqd, gql = leaves[0].grad[r * Q:(r + 1) * Q], leaves[1].grad[r * Q:(r + 1) * Q, :Lq]
    gcd, gcl = leaves[2].grad[r * cmax: r * cmax + C], leaves[3].grad[r * cmax: r * cmax + C]
    from .train_s1 import slice_batch
    for s0 in range(0, Q, a.mb_q):
        b = {k: v.to(dev) for k, v in slice_batch(q, s0, s0 + a.mb_q).items()}
        ctx = torch.autocast("cuda", dtype=torch.bfloat16) if dev.type == "cuda" else _null()
        with ctx:
            dn, lt = model(b["input_ids"], b["attention_mask"], b["late_mask"])
        ((dn.float() * gqd[s0:s0 + a.mb_q]).sum() + (lt.float() * gql[s0:s0 + a.mb_q]).sum()).backward()
    encode_chunks(model, batch["wins"], batch["place"], a.chunk_len, grads=(gcd, gcl), dev=dev)
    all_reduce_grads(model.parameters())
    return {k: float(v.detach()) for k, v in out.items()}
