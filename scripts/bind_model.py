#!/usr/bin/env python3
"""Bind a reviewed JSON configuration to the SHA returned by `pigmed lock-model`."""
import argparse
import json
import re
from pathlib import Path
p=argparse.ArgumentParser()
p.add_argument('--template',type=Path,required=True)
p.add_argument('--lock',type=Path,required=True)
p.add_argument('--out',type=Path,required=True)
a=p.parse_args()
cfg=json.loads(a.template.read_text());lock=json.loads(a.lock.read_text())
if cfg['model_id'] != lock['model_id'] or not re.fullmatch(r'[0-9a-f]{40}',lock['revision']):
    raise SystemExit('Model identity / commit mismatch')
if a.out.exists(): raise SystemExit('Refusing to overwrite a run configuration')
a.out.parent.mkdir(parents=True,exist_ok=True)
cfg['revision']=lock['revision']
a.out.write_text(json.dumps(cfg,indent=2)+'\n')
print(a.out)
