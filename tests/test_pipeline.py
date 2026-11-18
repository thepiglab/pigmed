import shutil
import json
from pathlib import Path
import numpy as np
import pytest
from pigmed.common import read_jsonl, digest_file
from pigmed.pipeline import RunConfig, run
from pigmed.shards import verify_bundle, read_dense, read_late
from pigmed.publish import publish, LocalSink
from pigmed.embeddings import MockEncoder
from pigmed.reembed import reembed
from pigmed.sources import plan_pmc

def ready(root):
    return sorted(root.glob('runs/*/ready/*/manifest.json'))

def test_pipeline_resume_publish(corpus,tmp_path):
    root = tmp_path/'outbox'
    cfg = RunConfig(backend='mock', render_pages=True, longest_side=256, chunk_tokens=20, overlap_tokens=3, batch_size=2)
    run(corpus['manifest'],root,cfg,corpus['http'])
    paths = ready(root)
    assert len(paths) == 1
    bundle = paths[0].parent
    manifest = verify_bundle(bundle)
    rows = list(read_jsonl(bundle/'items.jsonl.gz'))
    assert sum(x['kind']=='page' for x in rows) == 2
    assert sum(x['kind']=='figure' for x in rows) == 1
    assert not any('media_path' in x for x in rows)
    embedded = [x for x in rows if 'dense' in x]
    assert embedded
    assert read_dense(bundle,embedded[0]['dense']).shape == (32,)
    assert read_late(bundle,embedded[0]['late']).shape == (4,128)
    run(corpus['manifest'],root,cfg,corpus['http'])
    assert len(ready(root)) == 1
    sink = LocalSink(tmp_path/'remote')
    with pytest.raises(ValueError, match='mock'): publish(root,sink)
    result = publish(root,sink,delete=True,allow_mock=True)
    assert result['published_or_verified'] == 1 and not ready(root)
    assert not list((root/'budget').glob('*.json'))
    # A deleted local shard must not be regenerated on resume.
    run(corpus['manifest'],root,cfg,corpus['http'])
    assert not ready(root)

def test_failed_remote_verification_preserves_source(corpus,tmp_path):
    root=tmp_path/'outbox'
    run(corpus['manifest'],root,RunConfig(),corpus['http'])
    class Broken(LocalSink):
        def verify(self,*args): raise ValueError('network verification failure')
    with pytest.raises(ValueError): publish(root,Broken(tmp_path/'remote'),delete=True)
    assert len(ready(root)) == 1
    assert list((root/'budget').glob('*.json'))
    assert not list((root/'receipts').glob('*/*.json'))

def test_corruption_detected(corpus,tmp_path):
    root=tmp_path/'outbox'
    run(corpus['manifest'],root,RunConfig(),corpus['http'])
    bundle=ready(root)[0].parent
    with (bundle/'assets.tar').open('ab') as f: f.write(b'bad')
    with pytest.raises(ValueError,match='checksum'): verify_bundle(bundle)

def test_reembed_archived_views(corpus,tmp_path):
    root=tmp_path/'raw'
    run(corpus['manifest'],root,RunConfig(render_pages=True,longest_side=256),corpus['http'])
    result=reembed(ready(root)[0].parent,tmp_path/'embedded',MockEncoder(16),batch_size=2,chunk_tokens=12,overlap_tokens=2)
    bundle=Path(result['bundle']);verify_bundle(bundle)
    rows=list(read_jsonl(bundle/'items.jsonl.gz'))
    assert sum(x['kind']=='page' and 'dense' in x for x in rows)==2
    assert all(x['dense']['dimensions']==16 for x in rows if 'dense' in x)
    assert any(x.get('chunk_recipe',{}).get('budget')==12 for x in rows)
    again=reembed(ready(root)[0].parent,tmp_path/'embedded',MockEncoder(16),batch_size=2,chunk_tokens=12,overlap_tokens=2)
    assert again['already_complete']

def test_license_exclusion_downloads_no_article(corpus,tmp_path):
    import hashlib
    from pigmed.common import write_jsonl
    from pigmed.net import s3_url
    meta={**corpus['metadata'],'license_code':'TDM'}
    body=json.dumps(meta).encode(); key=s3_url('metadata/PMC123.1.json')
    corpus['http'].mapping[key]=body
    rows=list(read_jsonl(corpus['manifest']));rows[0]['etag']=hashlib.md5(body).hexdigest();write_jsonl(corpus['manifest'],rows)
    root=tmp_path/'raw';run(corpus['manifest'],root,RunConfig(),corpus['http'])
    assert corpus['http'].calls == [key]
    output=list(read_jsonl(ready(root)[0].parent/'items.jsonl.gz'))
    assert output[0]['status']=='excluded-by-license-policy'

def test_inventory_latest_versions(tmp_path):
    import gzip,csv,io,hashlib
    from conftest import FakeHttp
    from pigmed.net import s3_url
    csvtext=io.StringIO(); writer=csv.writer(csvtext)
    for pmc,version in [('PMC1',1),('PMC1',2),('PMC2',1)]:
        writer.writerow(['pmc-oa-opendata',f'metadata/{pmc}.{version}.json','2026-11-11','a'*32])
    data=gzip.compress(csvtext.getvalue().encode())
    url=s3_url('inventory-reports/fixture/manifest.json')
    manifest={'fileSchema':'Bucket, Key, LastModifiedDate, ETag', 'files':[{'key':'inventory-reports/fixture/data.csv.gz','MD5checksum':hashlib.md5(data).hexdigest()}]}
    http=FakeHttp({url:json.dumps(manifest).encode(),s3_url('inventory-reports/fixture/data.csv.gz'):data})
    out=tmp_path/'plan';r=plan_pmc(out,http,manifest_url=url,per_part=1)
    assert r['rows']==2 and r['parts']==2 and r['scanned_versions']==3
    first=list(read_jsonl(out/'part-00000.jsonl'))[0]
    assert first['pmcid']=='PMC1' and first['version']==2

def test_training_loss_gradient():
    torch=pytest.importorskip('torch')
    from pigmed.training import multipositive_loss
    scores=torch.tensor([[2.,1.,0.],[0.,2.,1.]],requires_grad=True)
    positive=torch.tensor([[True,True,False],[False,True,False]])
    allowed=torch.ones_like(positive)
    loss=multipositive_loss(scores,positive,allowed,1.)
    loss.backward()
    assert torch.isfinite(loss) and torch.isfinite(scores.grad).all()
    assert scores.grad[0,2]>0

def test_commit_survives_checkpoint_write_failure(corpus,tmp_path,monkeypatch):
    import pigmed.pipeline as pipeline
    root=tmp_path/'raw'
    original=pipeline.atomic_json
    def broken(path,value):
        if path.name=='state.json': raise OSError('simulated state-write failure')
        return original(path,value)
    monkeypatch.setattr(pipeline,'atomic_json',broken)
    with pytest.raises(OSError): run(corpus['manifest'],root,RunConfig(),corpus['http'])
    assert len(ready(root))==1
    assert list((root/'budget').glob('*.json'))
    monkeypatch.setattr(pipeline,'atomic_json',original)
    run(corpus['manifest'],root,RunConfig(),corpus['http'])
    assert len(ready(root))==1

def test_metadata_identity_mismatch(corpus,tmp_path):
    import hashlib
    from pigmed.common import write_jsonl
    from pigmed.net import s3_url
    meta={**corpus['metadata'],'pmcid':'PMC999'}
    body=json.dumps(meta).encode()
    corpus['http'].mapping[s3_url('metadata/PMC123.1.json')]=body
    rows=list(read_jsonl(corpus['manifest']));rows[0]['etag']=hashlib.md5(body).hexdigest();write_jsonl(corpus['manifest'],rows)
    root=tmp_path/'outbox'
    with pytest.raises(ValueError,match='identity'): run(corpus['manifest'],root,RunConfig(),corpus['http'])
    assert not ready(root) and not list((root/'budget').glob('*.json'))

def test_remote_receipt_resume_without_state(corpus,tmp_path):
    root=tmp_path/'raw';cfg=RunConfig()
    run(corpus['manifest'],root,cfg,corpus['http'])
    publish(root,LocalSink(tmp_path/'remote'),delete=True)
    for p in root.glob('runs/*/state.json'): p.unlink()
    for p in root.glob('runs/*/complete.json'): p.unlink()
    run(corpus['manifest'],root,cfg,corpus['http'])
    assert not ready(root)

def test_prototype_search(corpus,tmp_path):
    from pigmed.research import search
    root=tmp_path/'raw'
    run(corpus['manifest'],root,RunConfig(backend='mock'),corpus['http'])
    bundle=ready(root)[0].parent
    row=next(r for r in read_jsonl(bundle/'items.jsonl.gz') if r['kind']=='text')
    result=search([bundle],MockEncoder(),{'kind':'text','text':row['text']},k=1)
    assert result['hits'][0]['unit_id']==row['unit_id']
    with pytest.raises(ValueError,match='spaces'):search([bundle],MockEncoder(16),{'kind':'text','text':'test'})


def test_publisher_skips_partial_bundles(corpus,tmp_path):
    """A bundle whose manifest exists but whose commit rename has not happened is still owned by a worker."""
    root = tmp_path/'outbox'
    cfg = RunConfig(backend='none', render_pages=False, figures=False)
    run(corpus['manifest'],root,cfg,corpus['http'])
    bundle = ready(root)[0].parent
    partial = bundle.with_name(bundle.name + '-000000099.partial')
    shutil.copytree(bundle, partial)
    (partial/'items.jsonl.gz').write_bytes(b'truncated')  # would fail verification if picked up
    result = publish(root, LocalSink(tmp_path/'remote'))
    assert result['published_or_verified'] == 1
    assert partial.exists()


def test_license_policy_all_keeps_licence_on_rows(corpus,tmp_path):
    root = tmp_path/'outbox'
    run(corpus['manifest'],root,RunConfig(backend='none', figures=False, license_policy='all'),corpus['http'])
    rows = [r for p in ready(root) for r in read_jsonl(p.parent/'items.jsonl.gz')]
    content = [r for r in rows if r['kind'] != 'audit']
    assert content and all('license' in r and 'redistributable' in r for r in content)
    assert all(r['export_approved'] for r in content)  # mixed licences are publishable
    with pytest.raises(ValueError): RunConfig(license_policy='nc').validate()


def test_publisher_waits_for_vectors_and_uploads_them(corpus,tmp_path):
    """With require_vectors, a bundle stays local until every model embedded it; then bundle + dense vectors go
    up in one commit, are verified, and only the bundle is deleted (vectors stay for evaluation)."""
    root, vec = tmp_path/'outbox', tmp_path/'vectors'
    run(corpus['manifest'],root,RunConfig(backend='none', render_pages=False, figures=False),corpus['http'])
    bundle = ready(root)[0].parent
    sink = LocalSink(tmp_path/'remote')
    def shard(model):
        d = vec/model/bundle.name; d.mkdir(parents=True)
        np.save(d/'dense.npy', np.ones((2,4),np.float16)); np.save(d/'late.npy', np.ones((3,4),np.float16))
        (d/'ids.json').write_text('{"unit_id":["a","b"]}'); (d/'meta.json').write_text('{}')
    shard('gemma')
    r = publish(root, sink, delete=True, vectors_root=vec, require_vectors=['gemma','neomme'])
    assert r['published_or_verified'] == 0 and bundle.exists()
    shard('neomme')
    r = publish(root, sink, delete=True, vectors_root=vec, require_vectors=['gemma','neomme'])
    assert r['published_or_verified'] == 1 and not bundle.exists()
    up = sorted(p.relative_to(tmp_path/'remote').as_posix() for p in (tmp_path/'remote'/'vectors').rglob('*') if p.is_file())
    assert len(up) == 6 and not any('late' in p for p in up)  # dense/ids/meta per model; late stays local
    assert (vec/'neomme'/bundle.name/'late.npy').exists()
    receipt = json.loads(next((root/'receipts').glob('*/*.json')).read_text())
    assert len(receipt['vector_files']) == 6 and receipt['vector_bytes'] > 0
