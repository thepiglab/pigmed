"""CPU checks of the S1 data path: Source (jsonl and compacted store), make_plan (+mix), Batches, pack round trip,
targets (gold + group masking). python -m tests.check_data --base SNAP --data DATA [--source squad]"""
import argparse
import json
from pathlib import Path

import numpy as np
import torch

from eg2late.model import Tok, pack
from eg2late.train_s1 import Batches, DocStore, Source, make_plan, targets


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", required=True)
    ap.add_argument("--data", type=Path, required=True)
    ap.add_argument("--source", default="squad")
    a = ap.parse_args()
    rep = {}
    tok = Tok(a.base)
    s = Source(a.data / a.source, None, 0)
    rep["n"] = len(s)
    v, t = s.subset(slice(0, 512)), s.subset(slice(512, None))
    rep["split"] = [len(v), len(t)]
    # gold CSR survives subset
    i = 3
    rep["gold_subset_ok"] = bool(np.array_equal(t.gold_ids[t.gold_ptr[i]:t.gold_ptr[i + 1]],
                                                s.gold_ids[s.gold_ptr[512 + i]:s.gold_ptr[513 + i]]))

    class A:
        batch, n_neg, neg_pool, q_len, d_len, pack_tokens, pack_tokens_q = 64, 7, 15, 64, 320, 8192, 2048

    plan = make_plan([t], A.batch, 0)
    mplan = make_plan([t, v], A.batch, 0, mix={t.name: 3, "x": 0}, steps=50)
    rep["plan"] = len(plan)
    rep["mix_plan"] = len(mplan)
    b = Batches([t], plan, tok, A)[0]
    rep["shapes"] = {"q": list(b["q"]["input_ids"].shape), "d": list(b["d"]["input_ids"].shape),
                     "gold": list(b["gold"].shape), "tq": list(b["tq"].shape) if b["tq"] is not None else None}
    # pack round trip: every real token lands back at its (row, col)
    for key, x in (("qp", b["q"]), ("dp", b["d"])):
        rec = torch.zeros_like(x["input_ids"])
        lm = torch.zeros_like(x["late_mask"])
        for p in b[key]:
            rec[p["tok_row"], p["tok_col"]] = p["ids"][: p["n_real"]]
            lm[p["tok_row"], p["tok_col"]] = p["late_mask"][: p["n_real"]]
            assert int(p["len"].sum()) == p["n_real"]
        am = x["attention_mask"].bool()
        rep[f"pack_{key}_ok"] = bool(torch.equal(rec[am], x["input_ids"][am]) and torch.equal(lm, x["late_mask"]))
        rep[f"pack_{key}_rows"] = len(b[key])
    B, N = b["gold"].shape[0], b["doc_hash"].shape[0]
    forbid, lab, teacher = targets(b["gold"], b["doc_hash"], B, N, b["tq"], b["td"], 1, b["qgroup"], b["dgroup"])
    rep["label_not_forbidden"] = bool((~forbid[torch.arange(B), lab]).all())
    rep["forbidden_per_row"] = float(forbid.sum(1).float().mean())
    if teacher is not None:
        rep["teacher_pos_rank1"] = float((teacher.argmax(1) == lab).float().mean())
    # compacted store (if a PMC source is present)
    for d in sorted(a.data.iterdir()):
        if (d / "docs.off.npy").exists():
            st = DocStore(d)
            rep[f"store_{d.name}"] = {"n": len(st), "first": st.text(0)[:60], "groups": st.group is not None}
    print(json.dumps(rep, indent=1))
    assert rep["gold_subset_ok"] and rep["pack_qp_ok"] and rep["pack_dp_ok"] and rep["label_not_forbidden"]
    print("CHECK_DATA_OK")


if __name__ == "__main__":
    main()
