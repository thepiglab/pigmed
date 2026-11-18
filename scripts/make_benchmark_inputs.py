#!/usr/bin/env python3
"""Extract a bounded local benchmark subset from ONE verified archived-view bundle."""
import argparse
import json
import shutil
import tarfile
from pathlib import Path
from pigmed.common import read_jsonl,safe_member,stable_id,write_jsonl
from pigmed.shards import verify_bundle
p=argparse.ArgumentParser()
p.add_argument('--bundle',type=Path,required=True)
p.add_argument('--out',type=Path,required=True)
p.add_argument('--kind',choices=['text','caption','table_text','figure','page','audio','video'],required=True)
p.add_argument('--limit',type=int,default=100)
a=p.parse_args()
if a.limit<1: raise SystemExit('limit must be positive')
verify_bundle(a.bundle)
a.out.mkdir(parents=True,exist_ok=False)
rows=[]
with tarfile.open(a.bundle/'assets.tar') as tar:
    for row in read_jsonl(a.bundle/'items.jsonl.gz'):
        if row['kind'] != a.kind: continue
        if a.kind in {'figure','page','audio','video'}:
            member=safe_member(row['asset_member'])
            target=a.out/(stable_id(member)+Path(member).suffix)
            info=tar.getmember(member)
            if not info.isfile(): raise ValueError('Expected a regular media file')
            with tar.extractfile(info) as src,target.open('wb') as dst: shutil.copyfileobj(src,dst)
            row['media_path']=str(target.resolve())
        rows.append(row)
        if len(rows)>=a.limit: break
if not rows: raise SystemExit('No matching units in the bundle')
write_jsonl(a.out/'inputs.jsonl',rows)
print(a.out/'inputs.jsonl')
