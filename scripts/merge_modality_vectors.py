"""Replace one modality's rows in per-bundle vector shards with re-embedded ones (e.g. videos after a decoder fix).

For each bundle in NEW/<model>/ that has meta.json: drop the rows of MODALITY from OLD/<model>/<bundle>/ (dense.npy,
ids.json), append the new rows, update meta.json (modality entry, `reembedded` note), and write atomically. Prints
the list of changed bundles (feed it to hf_update_vectors.py).

usage: python -I scripts/merge_modality_vectors.py --old V --new V2 --model gemma --modality video --note TEXT
"""
import argparse
import json
import os
import shutil
from pathlib import Path

import numpy as np


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--old", type=Path, required=True)
    ap.add_argument("--new", type=Path, required=True)
    ap.add_argument("--model", required=True)
    ap.add_argument("--modality", required=True)
    ap.add_argument("--note", default="")
    ap.add_argument("--out-list", type=Path)
    a = ap.parse_args()
    changed, stats = [], {"bundles": 0, "old_rows": 0, "new_rows": 0}
    for nm in sorted((a.new / a.model).glob("*/meta.json")):
        b = nm.parent.name
        od, nd = a.old / a.model / b, nm.parent
        if not (od / "meta.json").exists():
            continue
        ids = json.loads((od / "ids.json").read_text())
        dense = np.load(od / "dense.npy")
        keep = [i for i, m in enumerate(ids["modality"]) if m != a.modality]
        nids = json.loads((nd / "ids.json").read_text())
        ndense = np.load(nd / "dense.npy") if len(nids["unit_id"]) else np.zeros((0, dense.shape[1]), dense.dtype)
        sel = [i for i, m in enumerate(nids["modality"]) if m == a.modality]
        if not sel and len(ids["unit_id"]) == len(keep):
            continue  # bundle has no rows of this modality at all: nothing to change or re-upload
        if not sel and len(ids["unit_id"]) - len(keep) > 0:
            stats.setdefault("kept_old_no_new_rows", []).append(b)  # re-embed got nothing (e.g. 404 today): keep old
            continue
        out_ids = {"unit_id": [ids["unit_id"][i] for i in keep] + [nids["unit_id"][i] for i in sel],
                   "modality": [ids["modality"][i] for i in keep] + [a.modality] * len(sel)}
        parts = [dense[keep]] + ([ndense[sel].astype(dense.dtype)] if sel else [])
        out_dense = np.concatenate(parts) if len(parts) > 1 else parts[0]
        meta = json.loads((od / "meta.json").read_text())
        nmeta = json.loads(nm.read_text())
        meta["modalities"][a.modality] = {**nmeta["modalities"].get(a.modality, {"items": 0}), "reembedded": a.note}
        meta.setdefault("reembedded", []).append({"modality": a.modality, "note": a.note,
                                                   "old_rows": len(ids["unit_id"]) - len(keep), "new_rows": len(sel)})
        tmp = od.with_name(od.name + f".tmp{os.getpid()}")
        shutil.rmtree(tmp, ignore_errors=True)
        shutil.copytree(od, tmp)
        np.save(tmp / "dense.npy", out_dense)
        (tmp / "ids.json").write_text(json.dumps(out_ids))
        (tmp / "meta.json").write_text(json.dumps(meta))
        bak = od.with_name(od.name + f".old{os.getpid()}")
        os.replace(od, bak)
        os.replace(tmp, od)
        shutil.rmtree(bak, ignore_errors=True)
        changed.append(b)
        stats["bundles"] += 1
        stats["old_rows"] += len(ids["unit_id"]) - len(keep)
        stats["new_rows"] += len(sel)
    if a.out_list:
        a.out_list.write_text("\n".join(changed) + "\n")
    print(json.dumps(stats))


if __name__ == "__main__":
    main()
