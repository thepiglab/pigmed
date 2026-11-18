import hashlib
import json
import shutil
from pathlib import Path
import pytest
from pigmed.common import digest_file, write_jsonl
from pigmed.net import s3_url

class FakeHttp:
    def __init__(self, mapping):
        self.mapping = mapping
        self.calls = []
    def get(self, url):
        self.calls.append(url)
        value = self.mapping[url]
        return value.read_bytes() if isinstance(value, Path) else value
    def json(self, url):
        return json.loads(self.get(url))
    def download(self, url, destination, max_bytes, md5=None):
        value = self.get(url)
        if len(value) > max_bytes:
            raise ValueError('Asset limit exceeded')
        if md5 and hashlib.md5(value).hexdigest() != md5:
            raise ValueError('MD5 mismatch')
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(value)
        return {'sha256': digest_file(destination), 'bytes': len(value)}

@pytest.fixture
def corpus(tmp_path):
    from PIL import Image, ImageDraw
    from reportlab.pdfgen import canvas
    fixture = Path(__file__).parent / 'fixtures/article.xml'
    image = tmp_path / 'figure1.jpg'
    im = Image.new('RGB', (320, 200), 'white')
    ImageDraw.Draw(im).line([(20,180),(80,40),(150,120),(280,170)], fill='black', width=3)
    im.save(image)
    pdf = tmp_path / 'article.pdf'
    c = canvas.Canvas(str(pdf))
    c.drawString(50, 780, 'Illustrative study: page one')
    c.showPage()
    c.drawString(50, 780, 'Results and figure: page two')
    c.save()
    prefix = 'https://pmc-oa-opendata.s3.amazonaws.com/PMC123.1/'
    meta = {'pmcid': 'PMC123', 'version': 1, 'pmid': '123', 'doi': '10.1/example',
            'license_code': 'CC BY', 'is_retracted': False, 'title': 'Example',
            'xml_url': prefix+'PMC123.1.xml', 'pdf_url': prefix+'article.pdf',
            'media_urls': [prefix+'figure1.jpg']}
    body = json.dumps(meta).encode()
    mapping = {s3_url('metadata/PMC123.1.json'): body, meta['xml_url']: fixture,
               meta['pdf_url']: pdf, meta['media_urls'][0]: image}
    manifest = tmp_path / 'part-00000.jsonl'
    write_jsonl(manifest, [{'source': 'pmc', 'pmcid': 'PMC123', 'version': 1,
        'key': 'metadata/PMC123.1.json', 'etag': hashlib.md5(body).hexdigest(), 'snapshot': 'fixture-snapshot'}])
    return {'manifest': manifest, 'http': FakeHttp(mapping), 'metadata': meta,
            'image': image, 'pdf': pdf, 'xml': fixture}
