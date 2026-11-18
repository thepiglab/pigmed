#!/usr/bin/env python3
"""Measure a planned citation baseline's compressed bytes from HTTP metadata.

No archives are saved. Run in an authorized transfer session; reports unknown
sizes instead of pretending an incomplete measurement is a complete total.
"""
import argparse
import glob
import json
from pathlib import Path
from pigmed.common import read_jsonl
from pigmed.net import Http
p=argparse.ArgumentParser()
p.add_argument('manifests',nargs='+')
p.add_argument('--contact',default='')
a=p.parse_args()
http=Http(a.contact,interval=.25)
known=0;count=0;unknown=[]
for pattern in a.manifests:
    for name in sorted(glob.glob(pattern)):
        for row in read_jsonl(Path(name)):
            if row.get('source')!='pubmed':raise ValueError('Expected PubMed archive plan')
            with http.get(row['url'],stream=True) as response:
                size=response.headers.get('Content-Length')
                if size and size.isdigit():known+=int(size)
                else:unknown.append(row['url'])
            count+=1
print(json.dumps({'archives':count,'known_compressed_bytes':known,
                  'known_compressed_GiB':known/2**30,'unknown_size_urls':unknown,
                  'complete_total':bool(count) and not unknown},indent=2))
