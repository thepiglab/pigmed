# Third-party notices

PigMed's own code is MIT-licensed (see [LICENSE](LICENSE)). These files are ports of third-party code and keep
their original terms:

| File | Ported from | Licence |
|---|---|---|
| `rust/pigmed-embed/src/pil_resize.rs` | Pillow `libImaging/Resample.c` (`ImagingResample`) | MIT-CMU (below) |
| `rust/pigmed-embed/src/bin/candle_neomme.rs` | Hugging Face transformers 5.22.2 `modeling_neomme.py` (rewritten in Rust; changed) | Apache License 2.0, https://www.apache.org/licenses/LICENSE-2.0 |

Models used but not distributed here: EmbeddingGemma 2 (Apache 2.0; use subject to the Gemma Prohibited Use
Policy) and NeoMME-260M-Retriever (Apache 2.0). Datasets keep their own licences; see `datasets/`.

## Pillow (MIT-CMU)

```
The Python Imaging Library (PIL) is

    Copyright © 1997-2011 by Secret Labs AB
    Copyright © 1995-2011 by Fredrik Lundh and contributors

Pillow is the friendly PIL fork. It is

    Copyright © 2010 by Jeffrey 'Alex' Clark and contributors

Like PIL, Pillow is licensed under the open source MIT-CMU License:

By obtaining, using, and/or copying this software and/or its associated
documentation, you agree that you have read, understood, and will comply
with the following terms and conditions:

Permission to use, copy, modify and distribute this software and its
documentation for any purpose and without fee is hereby granted,
provided that the above copyright notice appears in all copies, and that
both that copyright notice and this permission notice appear in supporting
documentation, and that the name of Secret Labs AB or the author not be
used in advertising or publicity pertaining to distribution of the software
without specific, written prior permission.

SECRET LABS AB AND THE AUTHOR DISCLAIMS ALL WARRANTIES WITH REGARD TO THIS
SOFTWARE, INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS.
IN NO EVENT SHALL SECRET LABS AB OR THE AUTHOR BE LIABLE FOR ANY SPECIAL,
INDIRECT OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM
LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE
OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR
PERFORMANCE OF THIS SOFTWARE.
```
