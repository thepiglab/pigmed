import gzip
import json
from pathlib import Path
import numpy as np
import pytest
from pigmed.common import normal_license, allowed_license, safe_member, exclusive_file, Backpressure, read_jsonl, write_jsonl
from pigmed.documents import TokenCounter, split_text, parse_jats, text_units, resolve_asset, iter_pubmed
from pigmed.embeddings import MockEncoder, mean_maxsim, quantize_int8, normalized, build_encoder
from pigmed.evaluation import assign_split, evaluate, reciprocal_rank_fusion, wilson
from pigmed.budget import Budget
from pigmed.media import time_windows, image_view, render_pdf, media_kind
from pigmed.net import s3_url
from pigmed.citations import materialize

@pytest.mark.parametrize('code', ['CC BY', 'CC-BY-4.0', 'CC_BY', 'CC0'])
def test_allowed_licenses(code):
    assert allowed_license(code)

@pytest.mark.parametrize('code', ['CC-BY-NC', 'TDM', None, 'unknown'])
def test_conservative_license_policy(code):
    assert not allowed_license(code)

@pytest.mark.parametrize('path', ['../escape', '/absolute', 'a/../../b', 'a\\b'])
def test_archive_paths(path):
    with pytest.raises(ValueError): safe_member(path)

def test_word_chunks_cover_text():
    text = ' '.join('β'+str(i) for i in range(37))
    chunks = list(split_text(text, TokenCounter(), 8, 2))
    seen = set()
    for part, a, b, n in chunks:
        assert part == text[a:b] and n <= 8
        seen.update(part.split())
    assert seen == set(text.split())
    assert chunks[-1][2] == len(text)

@pytest.mark.parametrize('budget,overlap', [(0,0),(4,4),(4,-1)])
def test_bad_chunk_settings(budget, overlap):
    with pytest.raises(ValueError): list(split_text('hello', TokenCounter(), budget, overlap))

def test_model_token_offsets():
    class Tiny:
        def __call__(self, text, **kwargs):
            return {'offset_mapping': [(i, i+1) for i in range(len(text))]}
    parts = list(split_text('abcdéf', TokenCounter(Tiny(), 'characters'), 3, 1))
    assert [x[0] for x in parts] == ['abc','cdé','éf']

def test_jats_structure(corpus):
    a = parse_jats(corpus['xml'], corpus['metadata'])
    assert a['article_id'] == 'PMC123.1'
    assert len(a['figures']) == 1 and not a['figures'][0]['rights_review_required']
    texts = [x['text'] for x in a['blocks'] if x['kind'] == 'p']
    assert 'Amplitude over time after intervention.' not in texts
    units = list(text_units(a, TokenCounter(), 20, 3))
    assert {'caption','table_text','text'} <= {x['kind'] for x in units}
    assert any(x['locator'].get('section') == ['Methods','Recording'] for x in units)

def test_asset_resolution():
    urls = ['https://a.test/figure1.jpg', 'https://a.test/other.png']
    assert resolve_asset('figure1', urls) == urls[0]
    assert resolve_asset('missing', urls) is None
    assert resolve_asset('figure1', urls+['https://a.test/figure1.png']) is None

def test_xxe_not_expanded(tmp_path, corpus):
    secret = tmp_path/'secret'
    secret.write_text('SECRET-CONTENT')
    xml = tmp_path/'xxe.xml'
    xml.write_text(f'<!DOCTYPE article [<!ENTITY x SYSTEM "{secret.as_uri()}">]><article><body><p>&x;</p></body></article>')
    a = parse_jats(xml, corpus['metadata'])
    assert 'SECRET-CONTENT' not in json.dumps(a)

def test_quantization_and_maxsim():
    x = normalized(np.random.default_rng(7).normal(size=(9,128)))
    codes, scales = quantize_int8(x)
    approx = codes.astype(np.float32) * scales[:,None]
    assert np.max(np.abs(approx-x)) < .003
    assert mean_maxsim(x, x, tile=2) == pytest.approx(1, abs=1e-6)
    assert mean_maxsim(x, approx) > .999

def test_invalid_vectors():
    for v in ([0,0], [1, float('nan')]):
        with pytest.raises(ValueError): normalized(v)

def test_real_model_requires_pin():
    with pytest.raises(ValueError): build_encoder('neomme', revision='main')

def test_split_group_stability():
    assert assign_split({'pmcid':'PMC123.1'}) == assign_split({'pmcid':'PMC123.2'})
    assert assign_split({'duplicate_cluster':'same', 'pmcid':'PMC1'}) == assign_split({'duplicate_cluster':'same','pmcid':'PMC2'})

def test_retrieval_metrics():
    m = evaluate({'q':{'a':2,'b':1}}, {'q':['a','a','b']}, 2)
    assert m['recall'] == 1 and m['ndcg'] == 1 and m['mrr'] == 1
    assert reciprocal_rank_fusion([['a','b'], ['b','a']]) == ['a','b']
    lo, hi = wilson(10,100)
    assert 0 < lo < .1 < hi < 1

def test_budget(tmp_path):
    b = Budget(tmp_path,100)
    b.reserve('a',60)
    b.reserve('a',60)
    with pytest.raises(Backpressure): b.reserve('b',50)
    b.release('a')
    b.reserve('b',50)

def test_exclusive_lock(tmp_path):
    path = tmp_path/'lock'
    with exclusive_file(path):
        with pytest.raises(RuntimeError, match='live process'):
            with exclusive_file(path): pass
    with exclusive_file(path): pass  # released on exit


def test_exclusive_lock_survives_killed_holder(tmp_path):
    """A holder killed with SIGKILL leaves the file behind but not the lock: a restart must not be blocked."""
    import subprocess, sys, signal, time as _t
    path = tmp_path/'lock'
    code = ("import sys,time; sys.path.insert(0, %r); from pigmed.common import exclusive_file; from pathlib import Path\n"
            "with exclusive_file(Path(%r)):\n    print('held', flush=True); time.sleep(60)") % (str(Path(__file__).parents[1]/'src'), str(path))
    p = subprocess.Popen([sys.executable, '-c', code], stdout=subprocess.PIPE, text=True)
    assert p.stdout.readline().strip() == 'held'
    with pytest.raises(RuntimeError, match='live process'):
        with exclusive_file(path): pass
    p.send_signal(signal.SIGKILL); p.wait()
    assert path.exists()
    with exclusive_file(path): pass

def test_media(corpus,tmp_path):
    assert list(time_windows(45,20,2)) == [(0,20),(18,38),(36,45)]
    assert media_kind('https://x/f.MP4?md5=abc') == 'video'
    assert media_kind('x.wav') == 'audio'
    out = tmp_path/'view.png'
    image_view(corpus['image'],out,128)
    from PIL import Image
    assert max(Image.open(out).size) == 128
    pages = list(render_pdf(corpus['pdf'],tmp_path/'pages',256,10))
    assert len(pages) == 2
    assert max(Image.open(pages[0][0]).size) <= 256

def test_pubmed_changes(tmp_path):
    path = tmp_path/'pubmed.xml.gz'
    with gzip.open(path,'wt') as f:
        f.write('<PubmedArticleSet><PubmedArticle><MedlineCitation Status="MEDLINE"><PMID>1</PMID><Article><ArticleTitle>Old title</ArticleTitle><Abstract><AbstractText Label="A">Text</AbstractText></Abstract></Article></MedlineCitation></PubmedArticle><DeleteCitation><PMID>2</PMID></DeleteCitation></PubmedArticleSet>')
    events = list(iter_pubmed(path, {'name':'pubmed26n0001.xml.gz','sequence':1,'phase':'baseline'}))
    assert events[0]['title'] == 'Old title' and events[1]['op'] == 'delete'
    events = [{**x,'kind':'citation'} for x in events]
    baseline=tmp_path/'baseline.jsonl'; write_jsonl(baseline,events)
    update=tmp_path/'update.jsonl'; write_jsonl(update,[{**events[0], 'phase':'update','title':'New title'}])
    result = materialize([update,baseline],tmp_path/'db.sqlite',tmp_path/'current.jsonl.gz')
    assert result['tombstones'] == 1
    assert list(read_jsonl(tmp_path/'current.jsonl.gz'))[0]['title'] == 'New title'
