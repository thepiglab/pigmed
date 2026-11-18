# Rust CPU embedding runner (EmbeddingGemma 2, NeoMME-260M-Retriever)

Status 2026-11-12. Code: `rust/pigmed-embed/` (standalone crate) and `examples/slurm/embed-rs-*.sbatch`. Cluster state lives
under `$EMBED_ROOT` (set in an untracked site env file; see `configs/site.env.example`). All numbers below were
measured in Slurm jobs on three AMD EPYC generations (Zen 2 and Zen 3: AVX2 + FMA; Zen 5: AVX-512), FP32, never on a
laptop.

## TL;DR

- **What**: `pigmed-embed`, a Rust CPU runner for EmbeddingGemma 2 (text, images, audio, video) and
  NeoMME-260M-Retriever (text and images; 1024-d dense + 128-d late vectors), FP32. The model forward runs in ONNX
  Runtime 1.31 (the same library the Python `onnxruntime` wheel ships) with three Rust custom kernels: fused
  sliding-window GQA attention, Gemma 4 audio chunk attention, and channel LayerNorm. Tokenization, image / audio /
  video decoding and preprocessing, batching and pooling are Rust.
- **Parity**: cosine >= 0.9999999 (min over items) against the model-card Python runtime (sentence-transformers /
  transformers 5.22, PyTorch 2.15) in every one of the 353 measured cases: text 128-16,384 tokens,
  PNG/JPEG/GIF/WebP and real PMC figures (TIFF/BMP: Rust only; the card runtime rejects them),
  WAV/MP3/FLAC/Ogg/Opus/M4A and real PMC recordings, MP4/MOV/AVI/MKV/WebM and real PMC videos, NeoMME documents and
  queries, dense and late vectors.
- **Speed**: Rust is faster in 351 of 353 cases on three EPYC generations (median 1.82x).
  - Text: 1.1-1.7x (one Zen 2 32-thread NeoMME row reads 25x because the Python runtime stalled at 0.2-0.3 items/s in 3 of 5 reps; its healthy reps were 6.3-6.6 vs Rust's 6.0-8.4) at 128 tokens; 2.2-3.1x at Gemma's 8,192 tokens and 4.4-6.1x at NeoMME's 16,384, where the
    fused attention skips key tiles outside the sliding windows.
  - Media: images 1.4x median in the format/size groups and 1.9x in the batch grid; video 2.2x median (1.3-2.8x);
    audio 1.0-8.2x on Zen 3 / Zen 5, with 30 s clips at the low end. On the loaded Zen 2 nodes the card runtime's
    audio path stalled at 15-45 s per clip in a rep, so audio ratios above 5x there say more about that node than
    about the runtimes.
  - The 2 exception(s) are 0.97-0.99x: neomme res1024 on Zen 2 (0.97x); gemma res512 on Zen 3 (0.99x). 8-rep reruns trace sub-1.0x results to heavily oversubscribed shared nodes, not to a slower code path (see 'Close cases and shared-node noise' under the per-op breakdown).
- **Memory**: Rust's peak RSS is lower in every case, median 0.59x the Python runtime's (0.34-0.35x at NeoMME 16k
  tokens). There are no S x S score tensors or dense masks, and no allocator arena.
- **Coverage**: 16 threads on all three CPUs (2-socket Zen 2 and Zen 3 nodes, 1-socket Zen 5 with AVX-512), plus a
  32-thread grid and context sweep on the 2-socket Zen 2 nodes (the hardest case for thread scaling). The 32-thread
  jobs on the Zen 3 / Zen 5 partitions were dropped: the shared queue's start estimates were a day out.

| CPU | benchmark | cases | Rust / Python speed: median (min - max) | Rust slower in | peak RSS Rust / Python: median (max) |
|---|---|---|---|---|---|
| EPYC 7702 (Zen 2, 2 sockets) | text + image, thread x batch grid | 44 | 1.67x (1.16 - 3.58) | 0 | 0.59 (0.87) |
| EPYC 7702 (Zen 2, 2 sockets) | context length 128 ... max | 36 | 2.04x (1.05 - 25.32) | 0 | 0.53 (0.78) |
| EPYC 7702 (Zen 2, 2 sockets) | modalities / file types / sizes | 53 | 1.88x (0.97 - 4.81) | 1 | 0.61 (0.90) |
| EPYC 75F3 (Zen 3, 2 sockets) | text + image, thread x batch grid | 36 | 1.44x (1.08 - 4.07) | 0 | 0.58 (0.84) |
| EPYC 75F3 (Zen 3, 2 sockets) | context length 128 ... max | 21 | 1.67x (1.05 - 5.59) | 0 | 0.53 (0.80) |
| EPYC 75F3 (Zen 3, 2 sockets) | modalities / file types / sizes | 53 | 1.41x (0.99 - 8.22) | 1 | 0.62 (0.92) |
| EPYC 9655P (Zen 5, 1 socket, AVX-512) | text + image, thread x batch grid | 36 | 2.18x (1.37 - 2.92) | 0 | 0.57 (0.85) |
| EPYC 9655P (Zen 5, 1 socket, AVX-512) | context length 128 ... max | 21 | 2.39x (1.52 - 7.08) | 0 | 0.53 (0.82) |
| EPYC 9655P (Zen 5, 1 socket, AVX-512) | modalities / file types / sizes | 53 | 2.02x (1.57 - 5.03) | 0 | 0.61 (0.90) |

Summary over 383 measured cases

- Rust / Python speed: min 0.97x, median 1.82x, max 25.32x; Rust slower in 2 case(s).
- Rust / Python peak RSS: median 0.59; Rust heavier in 0 case(s).
- Parity: worst dense cosine 1.0000000, worst late cosine 1.0000000; below 0.999 in 0 case(s).
  - Rust slower: rome mm neomme res1024 (speed 0.97x, RSS ratio 0.50, cos 1.00000)
  - Rust slower: milan mm gemma res512 (speed 0.99x, RSS ratio 0.57, cos 1.00000)

## What runs where

```
                 Python baseline (model-card runtime)             Rust runner (this crate)
text      sentence-transformers / NeoMMEProcessor            tokenizers crate (same tokenizer.json)
image     HF processors (torchvision uint8 resize)           src/prep.rs + src/pil_resize.rs (bit-compatible resize)
audio     torchcodec/librosa + Gemma4AudioFeatureExtractor   ffmpeg -> src/media.rs (log-mel port)
video     torchcodec + EmbeddingGemma2VideoProcessor         ffprobe/ffmpeg -> src/media.rs (same frame sampling)
decoders  torchvision (libjpeg-turbo), Pillow, FFmpeg 9 libs     mozjpeg (libjpeg-turbo), image + libwebp crates, FFmpeg 9 CLI
model     PyTorch 2.15 eager (SDPA attention)                ONNX Runtime 1.31 graphs + Rust attention ops
pooling   ST Pooling(mean)+Normalize / NeoMME heads          src/main.rs
```

The model forward runs in ONNX Runtime (C++/MLAS), loaded by the Rust binary through the `ort` crate
(`load-dynamic`: the very `libonnxruntime.so.1.31.1` shipped in the Python wheel). Attention is not ONNX Runtime's:
every attention call is a custom operator `pigmed.ai::FlashAttention` implemented in Rust (`src/flash.rs`).

## Backend choice (evidence)

Three options were measured, not assumed (all FP32, same nodes, interleaved runs):

| option | result | verdict |
|---|---|---|
| **candle** (pure Rust), NeoMME text encoder hand-ported (`src/bin/candle_neomme.rs`) | exact parity (cos 1.0000000), but **0.4-0.5x torch** (1.0-1.3 vs 2.1-2.9 items/s, 512 tokens): candle's CPU elementwise/broadcast ops are unfused and mostly single-threaded, so its fast GEMM (`gemm` crate, ~0.9 TFLOP/s on 16 cores) does not help end to end; would also mean hand-porting 3 novel architectures | rejected |
| **ONNX Runtime via `ort`**, graphs from `torch.onnx` (dynamo), stock attention | exact parity; 1.0-2.1x torch at <= 512 tokens, but **0.6-0.8x at 1-2k tokens, 2.6x torch's RSS at 8k, OOM at NeoMME 16k**: the exported attention materialises every S x S score matrix and per-layer dense masks | not sufficient alone |
| **ONNX Runtime + Rust fused attention custom ops** (`pigmed.ai::FlashAttention`, `pigmed.ai::AudioChunkAttention`, `src/flash.rs`) | exact parity; faster and lighter than torch across token ranges (tables below) | **chosen** |

Why ORT for the rest of the graph: MLAS GEMMs are on par with torch's MKL/oneDNN on these CPUs (GEMM microbenchmark:
`py/gemm_bench.py` + `gemm-bench`), all four model graphs export with exact parity, and ORT's graph optimizer already fuses
RMSNorm (after a one-line rewrite of HF's `x * pow(ms, -0.5)` into `x / sqrt(ms)` so the fusion pattern matches) and
transposed MatMuls. What ORT lacks for these models is a memory-efficient attention for bidirectional sliding-window
GQA and for Gemma 4's chunked audio attention; that is the part written in Rust. The ONNX graphs, tokenizer, image /
audio / video preprocessing, batching, pooling and the attention kernels are Rust; GEMMs and elementwise ops run in
ONNX Runtime's C++ (MLAS) library, the same build Python's `onnxruntime` 1.31.1 ships.

## The fused attention kernel (`src/flash.rs`)

`FlashAttention(Q[B,Hq,S,D], K[B,Hkv,S,D], V[B,Hkv,S,Dv], key_mask[B,S]; scale, window) -> O[B,S,Hq,Dv]`
reproduces transformers' eager attention for EmbeddingGemma 2 (text and vision) and NeoMME exactly: score scale, GQA
(kv head = h / (Hq/Hkv)), key padding, symmetric sliding window |i-j| <= w (HF passes w+1, flash convention), fp32
softmax. It is computed FlashAttention-style: 64-query x 256-key tiles with an online softmax, so no S x S tensor or
dense mask ever exists (peak extra memory per task: two 64 x 256 tiles), and key tiles outside a query tile's window
are skipped (4 of 5 Gemma layers and 14 of 17 NeoMME layers are sliding-window, so long inputs cost O(S x window) there
instead of O(S^2)). Tile GEMMs use the `gemm` crate (runtime AVX2/AVX-512 dispatch); the softmax uses AVX2/FMA or
AVX-512 intrinsics (8/16-wide Cephes expf, rel. error ~1e-7); tasks run on ONNX Runtime's own intra-op thread pool.
`flash-check` vs an f64 reference: max |diff| <= 4e-7 at every layout; vs torch's fused SDPA at the same shapes
(16 threads, Zen 3): Gemma sliding 8.5 vs 32 ms, Gemma global (d=512) 18 vs 26 ms, NeoMME sliding 3.8 vs 29 ms,
NeoMME global 5.5 vs 18 ms, vision (2520 patches) 26 vs 33 ms.

`AudioChunkAttention(Q, K, V[B,T,H,D], RelK[13,H,D], key_mask[B,T]; left, softcap, invalid)` is Gemma 4's audio
conformer attention (12-frame chunks, 12-frame left context, relative-position logits, logit soft-cap 50) as one op.
Besides speed, it is what makes the audio tower exportable with a dynamic length: torch.export never finished
(> 2 h) on HF's `unfold`/relative-shift chunking under symbolic shapes; with the op the export takes 25 s.

Two more rewrites target ops that ONNX Runtime runs single-threaded, both in the Gemma 4 audio tower (profiled on a
30 s clip, where it made the audio path 0.82x torch before): Gemma 4's "clippable" linears clamp with buffer bounds,
which export as 216 `Min`/`Max` nodes per forward (23 % of the audio tower); reading the bounds once at export emits
`Clip` with constant bounds instead (multithreaded, identical values). And the subsampling convs normalise channels as
permute(0,2,3,1) -> LayerNorm -> permute(0,3,1,2) -> ReLU, whose 48 MB channel-last transpose took 190 ms alone;
`pigmed.ai::ChannelLayerNorm` (Rust, `src/flash.rs`) normalises over the channel axis of the NCHW tensor directly,
ReLU fused. Together with stopping the audio decode at 30 s (the extractor's cap), 30 s clips went from 0.82x to
1.1-1.9x torch.

The graphs are exported by `py/export_onnx.py --attn pigmed`: a `torch.library` op with a dense reference
implementation is registered as a transformers attention backend (`AttentionInterface`), so the same HF modules run
either the reference (for the wrapper-vs-HF checks) or emit the custom ONNX node (for export). Because the backend is
not one of transformers' mask types, transformers also stops building the dense [B,1,S,S] masks at all.

Runner policies (all measured; overridable by env):

- **Vision patch trimming**: the HF processors pad every image to the full patch budget (2,520 patches for 280 soft
  tokens); padded patches are masked keys and pool to nothing, so the runner feeds the vision graph only up to the
  batch's largest real patch count. Output is unchanged (cos 0.9999999-1.0); a square image is 9 % fewer patches, a
  16:9 video frame 14 % fewer. `PIGMED_VISION_FULL=1` pads like the processor.
- **No ORT memory arena** (`PIGMED_ORT_ARENA=1` restores it): same speed, 25-45 % lower peak RSS on batched long inputs.
- **Length-sorted batching** (longest first, outputs restored to input order), as sentence-transformers does; mirrored
  in the Python NeoMME baseline so both sides pad the same way.
- **Thread pool**: dynamic block scheduling always, and the runner **autotunes the pool at startup**: it times a
  batch (best of up to 3 runs) under each candidate, one candidate alive at a time, and keeps the fastest (a few
  seconds, outside the timed loop; choice, thread count and timings are in each result JSON). It times the largest
  batch, and all candidates are non-spinning: all allotted threads pinned to the job's CPUs; half of them, largest
  socket's CPUs first; and on CPU sets spanning sockets, only the largest socket's CPUs. Spinning pools (ORT's default)
  won short tuning windows on shared nodes and then ran 2-3x slower once the node's load shifted, so they are only
  used when forced. Why: a forward pass is ~1,000 small ORT ops, each a thread-pool sync. On shared
  nodes (load 50-130 of 128 cores on Rome) spin-waiting threads made short inputs 1.5-3x slower, also on the
  single-socket Turin nodes (Gemma vision at 70 soft tokens: 2.6 items/s spinning vs 4.9 pinned), and on 2-socket
  32-CPU sets 16 pinned threads beat 32 for short inputs (NeoMME 128 tokens: 7.4-9.6 vs 5.7-7.2 items/s; torch with
  32 threads: 5.7-6.4). So "threads" in the tables is the CPUs given to both runtimes; Rust may use fewer of them
  (torch's per-op kernels also pick their own thread counts). `PIGMED_ORT_POLICY=<name prefix>` forces one.

## Parity (cosine vs the Python reference, same inputs)

Worst cosine between the Rust runner's and the Python card runtime's vectors for the same inputs, over every
benchmark case below (all three CPUs, all thread/batch configs, lengths and file types; FP32 both sides):

| model | modality | cases (all CPUs, configs, file types) | worst dense cosine | worst late cosine (NeoMME) |
|---|---|---|---|---|
| gemma | audio | 42 | 1.0000000 | - |
| gemma | image | 74 | 1.0000000 | - |
| gemma | text | 66 | 1.0000000 | - |
| gemma | video | 39 | 1.0000000 | - |
| neomme | image | 62 | 1.0000000 | 1.0000000 |
| neomme | text | 70 | 1.0000000 | 1.0000000 |

## Throughput and peak RSS: text and images (batch/thread grid)

<details><summary><b>EPYC 7702 (Zen 2, 2 sockets)</b></summary>


**bench-gemma-image-t16** (EPYC 7702 (Zen 2, 2 sockets))

| threads | batch | Python items/s | Rust items/s | Rust / Python | Python RSS MB | Rust RSS MB | Rust / Python RSS | parity (dense min cos / late min cos) |
|---|---|---|---|---|---|---|---|---|
| 4 | 1 | 0.10 | 0.18 | **1.84x** | 2986 | 1639 | 0.55 | 1.0000000 / n/a |
| 4 | 2 | 0.10 | 0.19 | **1.87x** | 2989 | 1861 | 0.62 | 1.0000000 / n/a |
| 4 | 4 | 0.10 | 0.17 | **1.65x** | 3468 | 2249 | 0.65 | 1.0000000 / n/a |
| 8 | 1 | 0.20 | 0.30 | **1.49x** | 2987 | 1668 | 0.56 | 1.0000000 / n/a |
| 8 | 2 | 0.19 | 0.32 | **1.69x** | 2991 | 1896 | 0.63 | 1.0000000 / n/a |
| 8 | 4 | 0.19 | 0.27 | **1.38x** | 3380 | 2298 | 0.68 | 1.0000000 / n/a |
| 16 | 1 | 0.22 | 0.40 | **1.79x** | 2990 | 1718 | 0.57 | 1.0000000 / n/a |
| 16 | 2 | 0.26 | 0.50 | **1.93x** | 2991 | 1933 | 0.65 | 1.0000000 / n/a |
| 16 | 4 | 0.23 | 0.51 | **2.25x** | 3462 | 2333 | 0.67 | 1.0000000 / n/a |

**bench-gemma-image-t32** (EPYC 7702 (Zen 2, 2 sockets))

| threads | batch | Python items/s | Rust items/s | Rust / Python | Python RSS MB | Rust RSS MB | Rust / Python RSS | parity (dense min cos / late min cos) |
|---|---|---|---|---|---|---|---|---|
| 32 | 1 | 0.30 | 0.58 | **1.93x** | 2979 | 1787 | 0.60 | 1.0000000 / n/a |
| 32 | 4 | 0.22 | 0.80 | **3.58x** | 3431 | 2415 | 0.70 | 1.0000000 / n/a |

**bench-gemma-text-t16** (EPYC 7702 (Zen 2, 2 sockets))

| threads | batch | Python items/s | Rust items/s | Rust / Python | Python RSS MB | Rust RSS MB | Rust / Python RSS | parity (dense min cos / late min cos) |
|---|---|---|---|---|---|---|---|---|
| 4 | 1 | 0.69 | 1.20 | **1.73x** | 1938 | 892 | 0.46 | 1.0000000 / n/a |
| 4 | 2 | 0.73 | 1.27 | **1.74x** | 1947 | 955 | 0.49 | 1.0000000 / n/a |
| 4 | 4 | 0.71 | 1.36 | **1.92x** | 2162 | 1108 | 0.51 | 1.0000000 / n/a |
| 8 | 1 | 1.30 | 1.76 | **1.36x** | 1937 | 904 | 0.47 | 1.0000000 / n/a |
| 8 | 2 | 1.42 | 2.04 | **1.44x** | 1953 | 978 | 0.50 | 1.0000000 / n/a |
| 8 | 4 | 1.55 | 2.30 | **1.49x** | 2162 | 1136 | 0.53 | 1.0000000 / n/a |
| 16 | 1 | 1.46 | 2.04 | **1.40x** | 1938 | 939 | 0.48 | 1.0000000 / n/a |
| 16 | 2 | 1.93 | 2.78 | **1.44x** | 1957 | 1008 | 0.52 | 1.0000000 / n/a |
| 16 | 4 | 2.24 | 3.24 | **1.45x** | 2199 | 1162 | 0.53 | 1.0000000 / n/a |

**bench-gemma-text-t32** (EPYC 7702 (Zen 2, 2 sockets))

| threads | batch | Python items/s | Rust items/s | Rust / Python | Python RSS MB | Rust RSS MB | Rust / Python RSS | parity (dense min cos / late min cos) |
|---|---|---|---|---|---|---|---|---|
| 32 | 1 | 2.11 | 2.65 | **1.26x** | 1927 | 1000 | 0.52 | 1.0000000 / n/a |
| 32 | 4 | 2.93 | 3.75 | **1.28x** | 2165 | 1216 | 0.56 | 1.0000000 / n/a |

**bench-neomme-image-t16** (EPYC 7702 (Zen 2, 2 sockets))

| threads | batch | Python items/s | Rust items/s | Rust / Python | Python RSS MB | Rust RSS MB | Rust / Python RSS | parity (dense min cos / late min cos) |
|---|---|---|---|---|---|---|---|---|
| 4 | 1 | 0.16 | 0.37 | **2.32x** | 2301 | 1479 | 0.64 | 1.0000000 / 1.0000000 |
| 4 | 2 | 0.13 | 0.35 | **2.65x** | 2596 | 1839 | 0.71 | 1.0000000 / 1.0000000 |
| 4 | 4 | 0.13 | 0.34 | **2.69x** | 2861 | 2481 | 0.87 | 1.0000000 / 1.0000000 |
| 8 | 1 | 0.30 | 0.55 | **1.84x** | 2263 | 1463 | 0.65 | 1.0000000 / 1.0000000 |
| 8 | 2 | 0.29 | 0.58 | **1.99x** | 2720 | 1859 | 0.68 | 1.0000000 / 1.0000000 |
| 8 | 4 | 0.26 | 0.55 | **2.11x** | 2926 | 2479 | 0.85 | 1.0000000 / 1.0000000 |
| 16 | 1 | 0.40 | 0.68 | **1.70x** | 2365 | 1475 | 0.62 | 1.0000000 / 1.0000000 |
| 16 | 2 | 0.43 | 0.81 | **1.88x** | 2582 | 1889 | 0.73 | 1.0000000 / 1.0000000 |
| 16 | 4 | 0.35 | 0.80 | **2.29x** | 2979 | 2480 | 0.83 | 1.0000000 / 1.0000000 |

**bench-neomme-image-t32** (EPYC 7702 (Zen 2, 2 sockets))

| threads | batch | Python items/s | Rust items/s | Rust / Python | Python RSS MB | Rust RSS MB | Rust / Python RSS | parity (dense min cos / late min cos) |
|---|---|---|---|---|---|---|---|---|
| 32 | 1 | 0.83 | 1.02 | **1.24x** | 2353 | 1533 | 0.65 | 1.0000000 / 1.0000000 |
| 32 | 4 | 0.60 | 1.48 | **2.46x** | 2965 | 2505 | 0.84 | 1.0000000 / 1.0000000 |

**bench-neomme-text-t16** (EPYC 7702 (Zen 2, 2 sockets))

| threads | batch | Python items/s | Rust items/s | Rust / Python | Python RSS MB | Rust RSS MB | Rust / Python RSS | parity (dense min cos / late min cos) |
|---|---|---|---|---|---|---|---|---|
| 4 | 1 | 0.77 | 1.25 | **1.64x** | 1958 | 931 | 0.48 | 1.0000000 / 1.0000000 |
| 4 | 2 | 0.76 | 1.28 | **1.69x** | 1960 | 1009 | 0.51 | 1.0000000 / 1.0000000 |
| 4 | 4 | 0.76 | 1.26 | **1.65x** | 1959 | 1120 | 0.57 | 1.0000000 / 1.0000000 |
| 8 | 1 | 1.63 | 2.01 | **1.23x** | 1963 | 961 | 0.49 | 1.0000000 / 1.0000000 |
| 8 | 2 | 1.60 | 2.11 | **1.32x** | 1960 | 1059 | 0.54 | 1.0000000 / 1.0000000 |
| 8 | 4 | 1.63 | 2.20 | **1.35x** | 1958 | 1182 | 0.60 | 1.0000000 / 1.0000000 |
| 16 | 1 | 2.32 | 2.74 | **1.18x** | 1962 | 954 | 0.49 | 1.0000000 / 1.0000000 |
| 16 | 2 | 2.36 | 3.01 | **1.27x** | 1962 | 1062 | 0.54 | 1.0000000 / 1.0000000 |
| 16 | 4 | 2.46 | 3.42 | **1.39x** | 1963 | 1176 | 0.60 | 1.0000000 / 1.0000000 |

**bench-neomme-text-t32** (EPYC 7702 (Zen 2, 2 sockets))

| threads | batch | Python items/s | Rust items/s | Rust / Python | Python RSS MB | Rust RSS MB | Rust / Python RSS | parity (dense min cos / late min cos) |
|---|---|---|---|---|---|---|---|---|
| 32 | 1 | 2.18 | 2.53 | **1.16x** | 1953 | 964 | 0.49 | 1.0000000 / 1.0000000 |
| 32 | 4 | 2.91 | 3.61 | **1.24x** | 1952 | 1204 | 0.62 | 1.0000000 / 1.0000000 |

</details>

<details><summary><b>EPYC 75F3 (Zen 3, 2 sockets)</b></summary>


**bench-gemma-image-t16** (EPYC 75F3 (Zen 3, 2 sockets))

| threads | batch | Python items/s | Rust items/s | Rust / Python | Python RSS MB | Rust RSS MB | Rust / Python RSS | parity (dense min cos / late min cos) |
|---|---|---|---|---|---|---|---|---|
| 4 | 1 | 0.20 | 0.32 | **1.66x** | 2995 | 1637 | 0.55 | 1.0000000 / n/a |
| 4 | 2 | 0.21 | 0.33 | **1.61x** | 2993 | 1849 | 0.62 | 1.0000000 / n/a |
| 4 | 4 | 0.22 | 0.30 | **1.36x** | 3349 | 2248 | 0.67 | 1.0000000 / n/a |
| 8 | 1 | 0.41 | 0.47 | **1.15x** | 2994 | 1684 | 0.56 | 1.0000000 / n/a |
| 8 | 2 | 0.39 | 0.52 | **1.33x** | 2995 | 1899 | 0.63 | 1.0000000 / n/a |
| 8 | 4 | 0.39 | 0.53 | **1.35x** | 3397 | 2296 | 0.68 | 1.0000000 / n/a |
| 16 | 1 | 0.60 | 0.64 | **1.08x** | 2996 | 1723 | 0.58 | 1.0000000 / n/a |
| 16 | 2 | 0.60 | 0.74 | **1.23x** | 2997 | 1951 | 0.65 | 1.0000000 / n/a |
| 16 | 4 | 0.59 | 0.80 | **1.35x** | 3452 | 2330 | 0.67 | 1.0000000 / n/a |

**bench-gemma-text-t16** (EPYC 75F3 (Zen 3, 2 sockets))

| threads | batch | Python items/s | Rust items/s | Rust / Python | Python RSS MB | Rust RSS MB | Rust / Python RSS | parity (dense min cos / late min cos) |
|---|---|---|---|---|---|---|---|---|
| 4 | 1 | 1.16 | 1.82 | **1.56x** | 1944 | 907 | 0.47 | 1.0000000 / n/a |
| 4 | 2 | 1.23 | 1.90 | **1.54x** | 1949 | 951 | 0.49 | 1.0000000 / n/a |
| 4 | 4 | 1.30 | 2.17 | **1.67x** | 2173 | 1106 | 0.51 | 1.0000000 / n/a |
| 8 | 1 | 1.82 | 2.43 | **1.34x** | 1945 | 935 | 0.48 | 1.0000000 / n/a |
| 8 | 2 | 2.12 | 2.77 | **1.31x** | 1968 | 974 | 0.50 | 1.0000000 / n/a |
| 8 | 4 | 2.40 | 3.45 | **1.44x** | 2189 | 1140 | 0.52 | 1.0000000 / n/a |
| 16 | 1 | 1.89 | 2.34 | **1.24x** | 1947 | 947 | 0.49 | 1.0000000 / n/a |
| 16 | 2 | 2.49 | 3.71 | **1.49x** | 1972 | 1025 | 0.52 | 1.0000000 / n/a |
| 16 | 4 | 3.36 | 4.82 | **1.43x** | 2185 | 1176 | 0.54 | 1.0000000 / n/a |

**bench-neomme-image-t16** (EPYC 75F3 (Zen 3, 2 sockets))

| threads | batch | Python items/s | Rust items/s | Rust / Python | Python RSS MB | Rust RSS MB | Rust / Python RSS | parity (dense min cos / late min cos) |
|---|---|---|---|---|---|---|---|---|
| 4 | 1 | 0.34 | 0.54 | **1.59x** | 2238 | 1476 | 0.66 | 1.0000000 / 1.0000000 |
| 4 | 2 | 0.30 | 0.54 | **1.79x** | 2637 | 1804 | 0.68 | 1.0000000 / 1.0000000 |
| 4 | 4 | 0.30 | 0.52 | **1.73x** | 2996 | 2496 | 0.83 | 1.0000000 / 1.0000000 |
| 8 | 1 | 0.59 | 0.96 | **1.63x** | 2348 | 1520 | 0.65 | 1.0000000 / 1.0000000 |
| 8 | 2 | 0.70 | 0.90 | **1.28x** | 2601 | 1841 | 0.71 | 1.0000000 / 1.0000000 |
| 8 | 4 | 0.53 | 0.94 | **1.79x** | 2947 | 2488 | 0.84 | 1.0000000 / 1.0000000 |
| 16 | 1 | 0.29 | 1.16 | **4.07x** | 2287 | 1479 | 0.65 | 1.0000000 / 1.0000000 |
| 16 | 2 | 0.74 | 1.10 | **1.48x** | 2494 | 1860 | 0.75 | 1.0000000 / 1.0000000 |
| 16 | 4 | 0.61 | 1.45 | **2.38x** | 3025 | 2494 | 0.82 | 1.0000000 / 1.0000000 |

**bench-neomme-text-t16** (EPYC 75F3 (Zen 3, 2 sockets))

| threads | batch | Python items/s | Rust items/s | Rust / Python | Python RSS MB | Rust RSS MB | Rust / Python RSS | parity (dense min cos / late min cos) |
|---|---|---|---|---|---|---|---|---|
| 4 | 1 | 1.12 | 1.63 | **1.47x** | 1968 | 971 | 0.49 | 1.0000000 / 1.0000000 |
| 4 | 2 | 1.16 | 1.75 | **1.51x** | 1967 | 1045 | 0.53 | 1.0000000 / 1.0000000 |
| 4 | 4 | 1.23 | 1.87 | **1.52x** | 1968 | 1155 | 0.59 | 1.0000000 / 1.0000000 |
| 8 | 1 | 2.07 | 2.85 | **1.38x** | 1970 | 966 | 0.49 | 1.0000000 / 1.0000000 |
| 8 | 2 | 2.46 | 2.76 | **1.12x** | 1967 | 1073 | 0.55 | 1.0000000 / 1.0000000 |
| 8 | 4 | 2.63 | 3.19 | **1.21x** | 1968 | 1198 | 0.61 | 1.0000000 / 1.0000000 |
| 16 | 1 | 2.53 | 3.21 | **1.27x** | 1970 | 965 | 0.49 | 1.0000000 / 1.0000000 |
| 16 | 2 | 2.95 | 3.80 | **1.29x** | 1972 | 1062 | 0.54 | 1.0000000 / 1.0000000 |
| 16 | 4 | 3.42 | 4.53 | **1.32x** | 1970 | 1191 | 0.60 | 1.0000000 / 1.0000000 |

</details>

<details><summary><b>EPYC 9655P (Zen 5, 1 socket, AVX-512)</b></summary>


**bench-gemma-image-t16** (EPYC 9655P (Zen 5, 1 socket, AVX-512))

| threads | batch | Python items/s | Rust items/s | Rust / Python | Python RSS MB | Rust RSS MB | Rust / Python RSS | parity (dense min cos / late min cos) |
|---|---|---|---|---|---|---|---|---|
| 4 | 1 | 0.24 | 0.62 | **2.56x** | 2991 | 1568 | 0.52 | 1.0000000 / n/a |
| 4 | 2 | 0.23 | 0.61 | **2.62x** | 2991 | 1780 | 0.60 | 1.0000000 / n/a |
| 4 | 4 | 0.23 | 0.59 | **2.55x** | 3404 | 2178 | 0.64 | 1.0000000 / n/a |
| 8 | 1 | 0.50 | 0.97 | **1.96x** | 2992 | 1668 | 0.56 | 1.0000000 / n/a |
| 8 | 2 | 0.49 | 1.00 | **2.03x** | 2991 | 1874 | 0.63 | 1.0000000 / n/a |
| 8 | 4 | 0.51 | 0.95 | **1.85x** | 3442 | 2271 | 0.66 | 1.0000000 / n/a |
| 16 | 1 | 0.78 | 1.41 | **1.82x** | 2992 | 1720 | 0.58 | 1.0000000 / n/a |
| 16 | 2 | 0.79 | 1.63 | **2.05x** | 2994 | 1923 | 0.64 | 1.0000000 / n/a |
| 16 | 4 | 0.80 | 1.62 | **2.03x** | 3456 | 2317 | 0.67 | 1.0000000 / n/a |

**bench-gemma-text-t16** (EPYC 9655P (Zen 5, 1 socket, AVX-512))

| threads | batch | Python items/s | Rust items/s | Rust / Python | Python RSS MB | Rust RSS MB | Rust / Python RSS | parity (dense min cos / late min cos) |
|---|---|---|---|---|---|---|---|---|
| 4 | 1 | 1.64 | 4.21 | **2.58x** | 1942 | 802 | 0.41 | 1.0000000 / n/a |
| 4 | 2 | 1.66 | 4.32 | **2.61x** | 1946 | 885 | 0.45 | 1.0000000 / n/a |
| 4 | 4 | 1.67 | 4.28 | **2.55x** | 2156 | 1049 | 0.49 | 1.0000000 / n/a |
| 8 | 1 | 3.19 | 6.05 | **1.90x** | 1944 | 906 | 0.47 | 1.0000000 / n/a |
| 8 | 2 | 3.47 | 6.48 | **1.87x** | 1975 | 966 | 0.49 | 1.0000000 / n/a |
| 8 | 4 | 3.49 | 7.26 | **2.08x** | 2213 | 1129 | 0.51 | 1.0000000 / n/a |
| 16 | 1 | 4.44 | 6.10 | **1.37x** | 1944 | 946 | 0.49 | 1.0000000 / n/a |
| 16 | 2 | 5.03 | 8.07 | **1.60x** | 1968 | 1021 | 0.52 | 1.0000000 / n/a |
| 16 | 4 | 5.35 | 9.82 | **1.83x** | 2178 | 1174 | 0.54 | 1.0000000 / n/a |

**bench-neomme-image-t16** (EPYC 9655P (Zen 5, 1 socket, AVX-512))

| threads | batch | Python items/s | Rust items/s | Rust / Python | Python RSS MB | Rust RSS MB | Rust / Python RSS | parity (dense min cos / late min cos) |
|---|---|---|---|---|---|---|---|---|
| 4 | 1 | 0.35 | 0.99 | **2.84x** | 2313 | 1475 | 0.64 | 1.0000000 / 1.0000000 |
| 4 | 2 | 0.35 | 0.96 | **2.75x** | 2614 | 1731 | 0.66 | 1.0000000 / 1.0000000 |
| 4 | 4 | 0.31 | 0.84 | **2.69x** | 2921 | 2454 | 0.84 | 1.0000000 / 1.0000000 |
| 8 | 1 | 0.72 | 1.73 | **2.40x** | 2281 | 1478 | 0.65 | 1.0000000 / 1.0000000 |
| 8 | 2 | 0.72 | 1.84 | **2.55x** | 2476 | 1860 | 0.75 | 1.0000000 / 1.0000000 |
| 8 | 4 | 0.63 | 1.70 | **2.69x** | 2913 | 2478 | 0.85 | 1.0000000 / 1.0000000 |
| 16 | 1 | 1.19 | 2.70 | **2.28x** | 2342 | 1491 | 0.64 | 1.0000000 / 1.0000000 |
| 16 | 2 | 1.23 | 2.93 | **2.37x** | 2649 | 1844 | 0.70 | 1.0000000 / 1.0000000 |
| 16 | 4 | 0.97 | 2.83 | **2.92x** | 2955 | 2488 | 0.84 | 1.0000000 / 1.0000000 |

**bench-neomme-text-t16** (EPYC 9655P (Zen 5, 1 socket, AVX-512))

| threads | batch | Python items/s | Rust items/s | Rust / Python | Python RSS MB | Rust RSS MB | Rust / Python RSS | parity (dense min cos / late min cos) |
|---|---|---|---|---|---|---|---|---|
| 4 | 1 | 1.21 | 3.20 | **2.64x** | 1969 | 932 | 0.47 | 1.0000000 / 1.0000000 |
| 4 | 2 | 1.19 | 3.26 | **2.74x** | 1963 | 1008 | 0.51 | 1.0000000 / 1.0000000 |
| 4 | 4 | 1.22 | 3.34 | **2.75x** | 1964 | 1126 | 0.57 | 1.0000000 / 1.0000000 |
| 8 | 1 | 2.67 | 4.92 | **1.85x** | 1963 | 960 | 0.49 | 1.0000000 / 1.0000000 |
| 8 | 2 | 2.70 | 5.28 | **1.96x** | 1963 | 1088 | 0.55 | 1.0000000 / 1.0000000 |
| 8 | 4 | 2.75 | 5.56 | **2.02x** | 1965 | 1187 | 0.60 | 1.0000000 / 1.0000000 |
| 16 | 1 | 4.04 | 6.90 | **1.71x** | 1965 | 957 | 0.49 | 1.0000000 / 1.0000000 |
| 16 | 2 | 4.05 | 7.52 | **1.86x** | 1966 | 1059 | 0.54 | 1.0000000 / 1.0000000 |
| 16 | 4 | 4.42 | 8.33 | **1.88x** | 1967 | 1229 | 0.62 | 1.0000000 / 1.0000000 |

</details>

## Context length

<details><summary><b>EPYC 7702 (Zen 2, 2 sockets)</b></summary>


**gemma text, t16 threads** (EPYC 7702 (Zen 2, 2 sockets); batch 1 unless noted)

| input | batch | tokens/item | Python items/s | Rust items/s | Rust / Python | Python s/item | Rust s/item | Python RSS MB | Rust RSS MB | dense min cos | late min cos |
|---|---|---|---|---|---|---|---|---|---|---|---|
| L128 | b1 | 128 | 4.300 | 4.657 | **1.08x** | 0.23 | 0.21 | 1941 | 785 | 1.0000000 | n/a |
| L512 | b1 | 512 | 2.220 | 2.763 | **1.24x** | 0.45 | 0.36 | 1941 | 912 | 1.0000000 | n/a |
| L1024 | b1 | 1024 | 0.854 | 1.162 | **1.36x** | 1.17 | 0.86 | 1969 | 980 | 1.0000000 | n/a |
| L2048 | b1 | 2048 | 0.503 | 0.719 | **1.43x** | 1.99 | 1.39 | 2233 | 1146 | 1.0000000 | n/a |
| L4096 | b1 | 4096 | 0.161 | 0.314 | **1.95x** | 6.21 | 3.19 | 2571 | 1510 | 1.0000000 | n/a |
| L8192 | b1 | 8192 | 0.044 | 0.128 | **2.88x** | 22.49 | 7.80 | 3574 | 2208 | 1.0000000 | n/a |
| mixed | b1 | 1568 | 0.315 | 0.670 | **2.12x** | 3.17 | 1.49 | 2456 | 1487 | 1.0000000 | n/a |
| mixed | b4 | 1568 | 0.100 | 0.598 | **5.96x** | 9.96 | 1.67 | 4160 | 3251 | 1.0000000 | n/a |
| over | b1 | 8192 | 0.054 | 0.144 | **2.65x** | 18.42 | 6.95 | 3556 | 2247 | 1.0000000 | n/a |
| query | b1 | 121 | 1.898 | 4.768 | **2.51x** | 0.53 | 0.21 | 1943 | 838 | 1.0000000 | n/a |

**gemma text, t32 threads** (EPYC 7702 (Zen 2, 2 sockets); batch 1 unless noted)

| input | batch | tokens/item | Python items/s | Rust items/s | Rust / Python | Python s/item | Rust s/item | Python RSS MB | Rust RSS MB | dense min cos | late min cos |
|---|---|---|---|---|---|---|---|---|---|---|---|
| L128 | b1 | 128 | 4.113 | 6.481 | **1.58x** | 0.24 | 0.15 | 1928 | 786 | 1.0000000 | n/a |
| L512 | b1 | 512 | 1.424 | 1.772 | **1.24x** | 0.70 | 0.56 | 1931 | 1030 | 1.0000000 | n/a |
| L1024 | b1 | 1024 | 0.609 | 1.323 | **2.17x** | 1.64 | 0.76 | 1984 | 1031 | 1.0000000 | n/a |
| L2048 | b1 | 2048 | 0.543 | 0.823 | **1.51x** | 1.84 | 1.21 | 2203 | 1222 | 1.0000000 | n/a |
| L4096 | b1 | 4096 | 0.278 | 0.439 | **1.58x** | 3.59 | 2.28 | 2654 | 1551 | 1.0000000 | n/a |
| L8192 | b1 | 8192 | 0.088 | 0.208 | **2.36x** | 11.32 | 4.80 | 3637 | 2297 | 1.0000000 | n/a |
| over | b1 | 8192 | 0.080 | 0.203 | **2.54x** | 12.53 | 4.93 | 3525 | 2277 | 1.0000000 | n/a |

**neomme text, t16 threads** (EPYC 7702 (Zen 2, 2 sockets); batch 1 unless noted)

| input | batch | tokens/item | Python items/s | Rust items/s | Rust / Python | Python s/item | Rust s/item | Python RSS MB | Rust RSS MB | dense min cos | late min cos |
|---|---|---|---|---|---|---|---|---|---|---|---|
| L128 | b1 | 128 | 5.953 | 8.732 | **1.47x** | 0.17 | 0.11 | 1961 | 844 | 1.0000000 | 1.0000000 |
| L512 | b1 | 512 | 2.106 | 2.491 | **1.18x** | 0.47 | 0.40 | 1969 | 914 | 1.0000000 | 1.0000000 |
| L1024 | b1 | 1024 | 0.953 | 1.413 | **1.48x** | 1.05 | 0.71 | 1963 | 991 | 1.0000000 | 1.0000000 |
| L2048 | b1 | 2048 | 0.430 | 0.655 | **1.52x** | 2.33 | 1.53 | 1962 | 1163 | 1.0000000 | 1.0000000 |
| L4096 | b1 | 4096 | 0.155 | 0.375 | **2.42x** | 6.45 | 2.67 | 2429 | 1452 | 1.0000000 | 1.0000000 |
| L8192 | b1 | 8192 | 0.048 | 0.166 | **3.47x** | 20.86 | 6.02 | 3437 | 2011 | 1.0000000 | 1.0000000 |
| L16384 | b1 | 16384 | 0.015 | 0.066 | **4.39x** | 66.07 | 15.06 | 9163 | 3162 | 1.0000000 | 1.0000000 |
| mixed | b1 | 1568 | 0.517 | 0.903 | **1.75x** | 1.94 | 1.11 | 2344 | 1442 | 1.0000000 | 1.0000000 |
| mixed | b4 | 1568 | 0.289 | 0.736 | **2.55x** | 3.46 | 1.36 | 4002 | 3123 | 1.0000000 | 1.0000000 |
| over | b1 | 16384 | 0.013 | 0.061 | **4.56x** | 74.66 | 16.38 | 9035 | 3136 | 1.0000000 | 1.0000000 |
| query | b1 | 123 | 6.195 | 6.860 | **1.11x** | 0.16 | 0.15 | 1961 | 879 | 1.0000000 | 1.0000000 |

**neomme text, t32 threads** (EPYC 7702 (Zen 2, 2 sockets); batch 1 unless noted)

| input | batch | tokens/item | Python items/s | Rust items/s | Rust / Python | Python s/item | Rust s/item | Python RSS MB | Rust RSS MB | dense min cos | late min cos |
|---|---|---|---|---|---|---|---|---|---|---|---|
| L128 | b1 | 128 | 0.310 | 7.844 | **25.32x** | 3.23 | 0.13 | 1957 | 854 | 1.0000000 | 1.0000000 |
| L512 | b1 | 512 | 1.964 | 2.801 | **1.43x** | 0.51 | 0.36 | 1955 | 921 | 1.0000000 | 1.0000000 |
| L1024 | b1 | 1024 | 1.289 | 1.357 | **1.05x** | 0.78 | 0.74 | 1950 | 995 | 1.0000000 | 1.0000000 |
| L2048 | b1 | 2048 | 0.626 | 0.887 | **1.42x** | 1.60 | 1.13 | 1950 | 1138 | 1.0000000 | 1.0000000 |
| L4096 | b1 | 4096 | 0.225 | 0.535 | **2.38x** | 4.45 | 1.87 | 2409 | 1438 | 1.0000000 | 1.0000000 |
| L8192 | b1 | 8192 | 0.050 | 0.226 | **4.53x** | 20.06 | 4.42 | 3521 | 2019 | 1.0000000 | 1.0000000 |
| L16384 | b1 | 16384 | 0.018 | 0.111 | **6.08x** | 54.96 | 9.04 | 9128 | 3177 | 1.0000000 | 1.0000000 |
| over | b1 | 16384 | 0.023 | 0.122 | **5.36x** | 44.00 | 8.21 | 9050 | 3139 | 1.0000000 | 1.0000000 |

</details>

<details><summary><b>EPYC 75F3 (Zen 3, 2 sockets)</b></summary>


**gemma text, t16 threads** (EPYC 75F3 (Zen 3, 2 sockets); batch 1 unless noted)

| input | batch | tokens/item | Python items/s | Rust items/s | Rust / Python | Python s/item | Rust s/item | Python RSS MB | Rust RSS MB | dense min cos | late min cos |
|---|---|---|---|---|---|---|---|---|---|---|---|
| L128 | b1 | 128 | 5.837 | 7.440 | **1.27x** | 0.17 | 0.13 | 1947 | 790 | 1.0000000 | n/a |
| L512 | b1 | 512 | 1.961 | 2.056 | **1.05x** | 0.51 | 0.49 | 1946 | 933 | 1.0000000 | n/a |
| L1024 | b1 | 1024 | 1.261 | 1.721 | **1.36x** | 0.79 | 0.58 | 1971 | 987 | 1.0000000 | n/a |
| L2048 | b1 | 2048 | 0.707 | 0.951 | **1.35x** | 1.41 | 1.05 | 2228 | 1172 | 1.0000000 | n/a |
| L4096 | b1 | 4096 | 0.275 | 0.453 | **1.65x** | 3.63 | 2.21 | 2597 | 1512 | 1.0000000 | n/a |
| L8192 | b1 | 8192 | 0.095 | 0.206 | **2.18x** | 10.58 | 4.86 | 3570 | 2243 | 1.0000000 | n/a |
| mixed | b1 | 1568 | 0.697 | 1.100 | **1.58x** | 1.43 | 0.91 | 2526 | 1484 | 1.0000000 | n/a |
| mixed | b4 | 1568 | 0.176 | 0.980 | **5.59x** | 5.70 | 1.02 | 4141 | 3256 | 1.0000000 | n/a |
| over | b1 | 8192 | 0.088 | 0.196 | **2.23x** | 11.39 | 5.11 | 3485 | 2250 | 1.0000000 | n/a |
| query | b1 | 121 | 4.478 | 8.689 | **1.94x** | 0.22 | 0.12 | 1946 | 852 | 1.0000000 | n/a |

**neomme text, t16 threads** (EPYC 75F3 (Zen 3, 2 sockets); batch 1 unless noted)

| input | batch | tokens/item | Python items/s | Rust items/s | Rust / Python | Python s/item | Rust s/item | Python RSS MB | Rust RSS MB | dense min cos | late min cos |
|---|---|---|---|---|---|---|---|---|---|---|---|
| L128 | b1 | 128 | 5.678 | 6.978 | **1.23x** | 0.18 | 0.14 | 1969 | 862 | 1.0000000 | 1.0000000 |
| L512 | b1 | 512 | 2.354 | 3.176 | **1.35x** | 0.42 | 0.31 | 1969 | 938 | 1.0000000 | 1.0000000 |
| L1024 | b1 | 1024 | 1.290 | 1.985 | **1.54x** | 0.77 | 0.50 | 1969 | 990 | 1.0000000 | 1.0000000 |
| L2048 | b1 | 2048 | 0.696 | 0.984 | **1.41x** | 1.44 | 1.02 | 1969 | 1155 | 1.0000000 | 1.0000000 |
| L4096 | b1 | 4096 | 0.217 | 0.470 | **2.17x** | 4.61 | 2.13 | 2379 | 1446 | 1.0000000 | 1.0000000 |
| L8192 | b1 | 8192 | 0.061 | 0.224 | **3.68x** | 16.47 | 4.47 | 3470 | 2019 | 1.0000000 | 1.0000000 |
| L16384 | b1 | 16384 | 0.019 | 0.095 | **5.14x** | 53.93 | 10.49 | 9020 | 3175 | 1.0000000 | 1.0000000 |
| mixed | b1 | 1568 | 0.640 | 1.066 | **1.67x** | 1.56 | 0.94 | 2379 | 1435 | 1.0000000 | 1.0000000 |
| mixed | b4 | 1568 | 0.344 | 0.989 | **2.88x** | 2.91 | 1.01 | 3906 | 3137 | 1.0000000 | 1.0000000 |
| over | b1 | 16384 | 0.031 | 0.113 | **3.69x** | 32.71 | 8.86 | 9141 | 3145 | 1.0000000 | 1.0000000 |
| query | b1 | 123 | 4.409 | 8.426 | **1.91x** | 0.23 | 0.12 | 1972 | 883 | 1.0000000 | 1.0000000 |

</details>

<details><summary><b>EPYC 9655P (Zen 5, 1 socket, AVX-512)</b></summary>


**gemma text, t16 threads** (EPYC 9655P (Zen 5, 1 socket, AVX-512); batch 1 unless noted)

| input | batch | tokens/item | Python items/s | Rust items/s | Rust / Python | Python s/item | Rust s/item | Python RSS MB | Rust RSS MB | dense min cos | late min cos |
|---|---|---|---|---|---|---|---|---|---|---|---|
| L128 | b1 | 128 | 13.038 | 22.685 | **1.74x** | 0.08 | 0.04 | 1942 | 770 | 1.0000000 | n/a |
| L512 | b1 | 512 | 3.704 | 6.119 | **1.65x** | 0.27 | 0.16 | 1942 | 942 | 1.0000000 | n/a |
| L1024 | b1 | 1024 | 2.181 | 3.389 | **1.55x** | 0.46 | 0.30 | 1976 | 975 | 1.0000000 | n/a |
| L2048 | b1 | 2048 | 0.875 | 1.749 | **2.00x** | 1.14 | 0.57 | 2205 | 1171 | 1.0000000 | n/a |
| L4096 | b1 | 4096 | 0.352 | 0.899 | **2.56x** | 2.84 | 1.11 | 2618 | 1502 | 1.0000000 | n/a |
| L8192 | b1 | 8192 | 0.096 | 0.300 | **3.13x** | 10.43 | 3.33 | 3543 | 2231 | 1.0000000 | n/a |
| mixed | b1 | 1568 | 0.942 | 2.250 | **2.39x** | 1.06 | 0.44 | 2608 | 1469 | 1.0000000 | n/a |
| mixed | b4 | 1568 | 0.257 | 1.819 | **7.08x** | 3.89 | 0.55 | 4167 | 3233 | 1.0000000 | n/a |
| over | b1 | 8192 | 0.101 | 0.315 | **3.12x** | 9.91 | 3.18 | 3617 | 2213 | 1.0000000 | n/a |
| query | b1 | 121 | 11.429 | 21.181 | **1.85x** | 0.09 | 0.05 | 1944 | 822 | 1.0000000 | n/a |

**neomme text, t16 threads** (EPYC 9655P (Zen 5, 1 socket, AVX-512); batch 1 unless noted)

| input | batch | tokens/item | Python items/s | Rust items/s | Rust / Python | Python s/item | Rust s/item | Python RSS MB | Rust RSS MB | dense min cos | late min cos |
|---|---|---|---|---|---|---|---|---|---|---|---|
| L128 | b1 | 128 | 10.581 | 16.200 | **1.53x** | 0.09 | 0.06 | 1964 | 851 | 1.0000000 | 1.0000000 |
| L512 | b1 | 512 | 4.234 | 7.130 | **1.68x** | 0.24 | 0.14 | 1965 | 916 | 1.0000000 | 1.0000000 |
| L1024 | b1 | 1024 | 1.768 | 3.175 | **1.80x** | 0.57 | 0.31 | 1964 | 984 | 1.0000000 | 1.0000000 |
| L2048 | b1 | 2048 | 0.823 | 1.711 | **2.08x** | 1.21 | 0.58 | 1966 | 1141 | 1.0000000 | 1.0000000 |
| L4096 | b1 | 4096 | 0.384 | 1.090 | **2.84x** | 2.60 | 0.92 | 2416 | 1490 | 1.0000000 | 1.0000000 |
| L8192 | b1 | 8192 | 0.089 | 0.432 | **4.83x** | 11.18 | 2.31 | 3463 | 2018 | 1.0000000 | 1.0000000 |
| L16384 | b1 | 16384 | 0.035 | 0.208 | **5.99x** | 28.76 | 4.80 | 9182 | 3162 | 1.0000000 | 1.0000000 |
| mixed | b1 | 1568 | 0.976 | 2.641 | **2.71x** | 1.02 | 0.38 | 2415 | 1429 | 1.0000000 | 1.0000000 |
| mixed | b4 | 1568 | 0.561 | 1.984 | **3.54x** | 1.78 | 0.50 | 3811 | 3121 | 1.0000000 | 1.0000000 |
| over | b1 | 16384 | 0.032 | 0.175 | **5.44x** | 31.07 | 5.71 | 8994 | 3134 | 1.0000000 | 1.0000000 |
| query | b1 | 123 | 11.931 | 18.135 | **1.52x** | 0.08 | 0.06 | 1969 | 883 | 1.0000000 | 1.0000000 |

</details>

## Modalities and file types

<details><summary><b>EPYC 7702 (Zen 2, 2 sockets)</b></summary>


**gemma: modalities, file types, sizes** (EPYC 7702 (Zen 2, 2 sockets), 16 threads, batch 1)

| input group | modality | items | Python | Rust | Rust / Python | Python RSS MB | Rust RSS MB | dense min cos | late min cos |
|---|---|---|---|---|---|---|---|---|---|
| avi | video | 3 | 0.05 items/s | 0.13 items/s | **2.81x** | 3378 | 2392 | 1.0000000 | n/a |
| bmp | image | 3 | REJECTED: raise ValueError( | 0.49 items/s | **-** | - | 1638 | - | n/a |
| flac16k | audio | 3 | 0.93 items/s | 1.32 items/s | **1.42x** | 3698 | 2296 | 1.0000000 | n/a |
| gif | image | 3 | 0.31 items/s | 0.41 items/s | **1.36x** | 2905 | 1648 | 1.0000000 | n/a |
| jpg | image | 3 | 0.20 items/s | 0.48 items/s | **2.39x** | 2903 | 1635 | 1.0000000 | n/a |
| m4a | audio | 3 | 0.48 items/s | 1.40 items/s | **2.93x** | 3697 | 2240 | 1.0000000 | n/a |
| mov | video | 3 | 0.05 items/s | 0.13 items/s | **2.47x** | 3372 | 2387 | 1.0000000 | n/a |
| mp3 | audio | 3 | 0.46 items/s | 1.34 items/s | **2.94x** | 3697 | 2259 | 1.0000000 | n/a |
| mp4_10s | video | 7 | 0.06 items/s | 0.12 items/s | **1.88x** | 3433 | 2395 | 1.0000000 | n/a |
| mp4_12s | video | 2 | 0.04 items/s | 0.10 items/s | **2.26x** | 3554 | 2573 | 1.0000000 | n/a |
| mp4_20s | video | 3 | 0.03 items/s | 0.06 items/s | **2.39x** | 3854 | 3182 | 1.0000000 | n/a |
| mp4_40s | video | 1 | 0.02 items/s | 0.04 items/s | **2.09x** | 4850 | 4216 | 1.0000000 | n/a |
| mp4_4s | video | 3 | 0.17 items/s | 0.31 items/s | **1.81x** | 2926 | 1845 | 1.0000000 | n/a |
| mp4_9s | video | 1 | 0.06 items/s | 0.14 items/s | **2.26x** | 3258 | 2254 | 1.0000000 | n/a |
| nativeavi | video | 3 | 0.03 items/s | 0.07 items/s | **2.22x** | 4347 | 3687 | 1.0000000 | n/a |
| nativebmp | image | 1 | REJECTED: raise ValueError( | 0.49 items/s | **-** | - | 1670 | - | n/a |
| nativegif | image | 4 | REJECTED: ValueError: too many values to unpack (expected 3) | 0.44 items/s | **-** | - | 1682 | - | n/a |
| nativejpg | image | 8 | 0.33 items/s | 0.44 items/s | **1.34x** | 2901 | 1640 | 1.0000000 | n/a |
| nativem4v | video | 3 | 0.03 items/s | 0.06 items/s | **2.11x** | 4839 | 4356 | 1.0000000 | n/a |
| nativemkv | video | 1 | 0.06 items/s | 0.11 items/s | **1.97x** | 3456 | 2449 | 1.0000000 | n/a |
| nativemov | video | 1 | 0.02 items/s | 0.04 items/s | **2.21x** | 4719 | 4209 | 1.0000000 | n/a |
| nativemp3 | audio | 3 | 0.14 items/s | 0.48 items/s | **3.34x** | 3695 | 2306 | 1.0000000 | n/a |
| nativepng | image | 4 | 0.36 items/s | 0.50 items/s | **1.40x** | 2904 | 1630 | 1.0000000 | n/a |
| nativetif | image | 8 | REJECTED: raise ValueError( | 0.37 items/s | **-** | - | 2232 | - | n/a |
| nativewav | audio | 3 | 0.53 items/s | 1.58 items/s | **3.01x** | 3700 | 2245 | 1.0000000 | n/a |
| nativewebp | image | 4 | 0.35 items/s | 0.49 items/s | **1.39x** | 2903 | 1642 | 1.0000000 | n/a |
| ogg | audio | 3 | 0.47 items/s | 1.33 items/s | **2.84x** | 3695 | 2225 | 1.0000000 | n/a |
| opus | audio | 3 | 0.41 items/s | 1.26 items/s | **3.09x** | 3694 | 2213 | 1.0000000 | n/a |
| png | image | 3 | 0.35 items/s | 0.44 items/s | **1.25x** | 2903 | 1642 | 1.0000000 | n/a |
| res1024 | image | 4 | 0.38 items/s | 0.47 items/s | **1.26x** | 2911 | 1646 | 1.0000000 | n/a |
| res1536 | image | 4 | 0.34 items/s | 0.47 items/s | **1.38x** | 2924 | 1654 | 1.0000000 | n/a |
| res2048 | image | 4 | 0.33 items/s | 0.47 items/s | **1.43x** | 2940 | 1669 | 1.0000000 | n/a |
| res512 | image | 4 | 0.33 items/s | 0.48 items/s | **1.43x** | 2905 | 1641 | 1.0000000 | n/a |
| tiff | image | 3 | REJECTED: raise ValueError( | 0.48 items/s | **-** | - | 1637 | - | n/a |
| tok1120 | image | 16 | 0.05 items/s | 0.08 items/s | **1.57x** | 3569 | 2338 | 1.0000000 | n/a |
| tok140 | image | 16 | 0.74 items/s | 1.02 items/s | **1.38x** | 2991 | 1584 | 1.0000000 | n/a |
| tok560 | image | 16 | 0.13 items/s | 0.23 items/s | **1.77x** | 2991 | 1925 | 1.0000000 | n/a |
| tok70 | image | 16 | 1.55 items/s | 1.87 items/s | **1.21x** | 2994 | 1515 | 1.0000000 | n/a |
| wav | audio | 3 | 0.67 items/s | 1.33 items/s | **1.99x** | 3698 | 2283 | 1.0000000 | n/a |
| wav16k_0s | audio | 1 | 1.90 items/s | 9.13 items/s | **4.81x** | 3694 | 2107 | 1.0000000 | n/a |
| wav16k_10s | audio | 5 | 0.55 items/s | 1.36 items/s | **2.45x** | 3696 | 2264 | 1.0000000 | n/a |
| wav16k_25s | audio | 3 | 0.37 items/s | 0.62 items/s | **1.66x** | 3696 | 2345 | 1.0000000 | n/a |
| wav16k_3s | audio | 1 | 1.38 items/s | 3.09 items/s | **2.24x** | 3694 | 2146 | 1.0000000 | n/a |
| wav16k_40s | audio | 3 | 0.26 items/s | 0.50 items/s | **1.94x** | 3698 | 2291 | 1.0000000 | n/a |
| wav16k_4s | audio | 3 | 0.69 items/s | 2.92 items/s | **4.21x** | 3699 | 2129 | 1.0000000 | n/a |
| webm | video | 3 | 0.06 items/s | 0.12 items/s | **2.12x** | 3340 | 2386 | 1.0000000 | n/a |
| webp | image | 3 | 0.27 items/s | 0.49 items/s | **1.84x** | 2902 | 1642 | 1.0000000 | n/a |

**neomme: modalities, file types, sizes** (EPYC 7702 (Zen 2, 2 sockets), 16 threads, batch 1)

| input group | modality | items | Python | Rust | Rust / Python | Python RSS MB | Rust RSS MB | dense min cos | late min cos |
|---|---|---|---|---|---|---|---|---|---|
| bmp | image | 3 | REJECTED: raise ValueError( | 0.81 items/s | **-** | - | 1295 | - | n/a |
| gif | image | 3 | 0.58 items/s | 0.80 items/s | **1.37x** | 1967 | 1305 | 1.0000000 | 1.0000000 |
| jpg | image | 3 | 0.32 items/s | 0.79 items/s | **2.44x** | 1995 | 1307 | 1.0000000 | 1.0000000 |
| nativebmp | image | 1 | REJECTED: raise ValueError( | 0.70 items/s | **-** | - | 1110 | - | n/a |
| nativegif | image | 4 | REJECTED: ValueError: too many values to unpack (expected 4) | 7.65 items/s | **-** | - | 891 | - | n/a |
| nativejpg | image | 8 | 1.22 items/s | 1.32 items/s | **1.08x** | 1968 | 998 | 1.0000000 | 1.0000000 |
| nativepng | image | 4 | 1.03 items/s | 1.37 items/s | **1.33x** | 1967 | 1049 | 1.0000000 | 1.0000000 |
| nativetif | image | 8 | REJECTED: raise ValueError( | 0.35 items/s | **-** | - | 1822 | - | n/a |
| nativewebp | image | 4 | 1.67 items/s | 1.78 items/s | **1.06x** | 1967 | 972 | 1.0000000 | 1.0000000 |
| png | image | 3 | 0.52 items/s | 0.79 items/s | **1.52x** | 1994 | 1320 | 1.0000000 | 1.0000000 |
| res1024 | image | 4 | 1.16 items/s | 1.13 items/s | **0.97x** | 1975 | 997 | 1.0000000 | 1.0000000 |
| res1536 | image | 4 | 0.47 items/s | 0.58 items/s | **1.25x** | 1989 | 1176 | 1.0000000 | 1.0000000 |
| res2048 | image | 4 | 0.21 items/s | 0.41 items/s | **1.93x** | 2092 | 1408 | 1.0000000 | 1.0000000 |
| res512 | image | 4 | 3.02 items/s | 3.74 items/s | **1.24x** | 1979 | 909 | 1.0000000 | 1.0000000 |
| tiff | image | 3 | REJECTED: raise ValueError( | 0.83 items/s | **-** | - | 1290 | - | n/a |
| webp | image | 3 | 0.58 items/s | 0.81 items/s | **1.40x** | 1979 | 1306 | 1.0000000 | 1.0000000 |

</details>

<details><summary><b>EPYC 75F3 (Zen 3, 2 sockets)</b></summary>


**gemma: modalities, file types, sizes** (EPYC 75F3 (Zen 3, 2 sockets), 16 threads, batch 1)

| input group | modality | items | Python | Rust | Rust / Python | Python RSS MB | Rust RSS MB | dense min cos | late min cos |
|---|---|---|---|---|---|---|---|---|---|
| avi | video | 3 | 0.11 items/s | 0.18 items/s | **1.68x** | 3379 | 2405 | 1.0000000 | n/a |
| bmp | image | 3 | REJECTED: raise ValueError( | 0.63 items/s | **-** | - | 1646 | - | n/a |
| flac16k | audio | 3 | 0.59 items/s | 1.72 items/s | **2.93x** | 3702 | 2258 | 1.0000000 | n/a |
| gif | image | 3 | 0.59 items/s | 0.66 items/s | **1.12x** | 2910 | 1653 | 1.0000000 | n/a |
| jpg | image | 3 | 0.60 items/s | 0.65 items/s | **1.08x** | 2910 | 1649 | 1.0000000 | n/a |
| m4a | audio | 3 | 0.36 items/s | 1.53 items/s | **4.25x** | 3702 | 2278 | 1.0000000 | n/a |
| mov | video | 3 | 0.11 items/s | 0.17 items/s | **1.50x** | 3433 | 2404 | 1.0000000 | n/a |
| mp3 | audio | 3 | 0.76 items/s | 1.58 items/s | **2.08x** | 3702 | 2287 | 1.0000000 | n/a |
| mp4_10s | video | 7 | 0.11 items/s | 0.18 items/s | **1.64x** | 3385 | 2404 | 1.0000000 | n/a |
| mp4_12s | video | 2 | 0.09 items/s | 0.15 items/s | **1.75x** | 3526 | 2592 | 1.0000000 | n/a |
| mp4_20s | video | 3 | 0.04 items/s | 0.10 items/s | **2.16x** | 3869 | 3197 | 1.0000000 | n/a |
| mp4_40s | video | 1 | 0.03 items/s | 0.06 items/s | **2.03x** | 4867 | 4230 | 1.0000000 | n/a |
| mp4_4s | video | 3 | 0.31 items/s | 0.40 items/s | **1.27x** | 2951 | 1856 | 1.0000000 | n/a |
| mp4_9s | video | 1 | 0.12 items/s | 0.19 items/s | **1.63x** | 3280 | 2264 | 1.0000000 | n/a |
| nativeavi | video | 3 | 0.05 items/s | 0.10 items/s | **2.09x** | 4310 | 3698 | 1.0000000 | n/a |
| nativebmp | image | 1 | REJECTED: raise ValueError( | 0.68 items/s | **-** | - | 1677 | - | n/a |
| nativegif | image | 4 | REJECTED: ValueError: too many values to unpack (expected 3) | 0.67 items/s | **-** | - | 1693 | - | n/a |
| nativejpg | image | 8 | 0.59 items/s | 0.65 items/s | **1.10x** | 2910 | 1656 | 1.0000000 | n/a |
| nativem4v | video | 3 | 0.04 items/s | 0.09 items/s | **2.03x** | 4789 | 4384 | 1.0000000 | n/a |
| nativemkv | video | 1 | 0.09 items/s | 0.14 items/s | **1.51x** | 3548 | 2475 | 1.0000000 | n/a |
| nativemov | video | 1 | 0.03 items/s | 0.06 items/s | **2.14x** | 4696 | 4237 | 1.0000000 | n/a |
| nativemp3 | audio | 3 | 0.41 items/s | 0.54 items/s | **1.33x** | 3702 | 2381 | 1.0000000 | n/a |
| nativepng | image | 4 | 0.58 items/s | 0.66 items/s | **1.13x** | 2908 | 1644 | 1.0000000 | n/a |
| nativetif | image | 8 | REJECTED: raise ValueError( | 0.51 items/s | **-** | - | 2285 | - | n/a |
| nativewav | audio | 3 | 0.83 items/s | 1.83 items/s | **2.20x** | 3704 | 2215 | 1.0000000 | n/a |
| nativewebp | image | 4 | 0.57 items/s | 0.63 items/s | **1.10x** | 2910 | 1646 | 1.0000000 | n/a |
| ogg | audio | 3 | 0.95 items/s | 1.43 items/s | **1.51x** | 3702 | 2282 | 1.0000000 | n/a |
| opus | audio | 3 | 0.79 items/s | 1.56 items/s | **1.97x** | 3702 | 2300 | 1.0000000 | n/a |
| png | image | 3 | 0.54 items/s | 0.65 items/s | **1.21x** | 2909 | 1648 | 1.0000000 | n/a |
| res1024 | image | 4 | 0.59 items/s | 0.63 items/s | **1.07x** | 2920 | 1660 | 1.0000000 | n/a |
| res1536 | image | 4 | 0.56 items/s | 0.61 items/s | **1.09x** | 2931 | 1667 | 1.0000000 | n/a |
| res2048 | image | 4 | 0.57 items/s | 0.60 items/s | **1.06x** | 2949 | 1684 | 1.0000000 | n/a |
| res512 | image | 4 | 0.62 items/s | 0.61 items/s | **0.99x** | 2914 | 1646 | 1.0000000 | n/a |
| tiff | image | 3 | REJECTED: raise ValueError( | 0.64 items/s | **-** | - | 1649 | - | n/a |
| tok1120 | image | 16 | 0.09 items/s | 0.12 items/s | **1.41x** | 3637 | 2395 | 1.0000000 | n/a |
| tok140 | image | 16 | 1.20 items/s | 1.34 items/s | **1.12x** | 2997 | 1603 | 1.0000000 | n/a |
| tok560 | image | 16 | 0.25 items/s | 0.30 items/s | **1.24x** | 2997 | 1933 | 1.0000000 | n/a |
| tok70 | image | 16 | 1.75 items/s | 2.48 items/s | **1.41x** | 2996 | 1552 | 1.0000000 | n/a |
| wav | audio | 3 | 1.12 items/s | 1.63 items/s | **1.45x** | 3702 | 2290 | 1.0000000 | n/a |
| wav16k_0s | audio | 1 | 1.13 items/s | 9.26 items/s | **8.22x** | 3702 | 2136 | 1.0000000 | n/a |
| wav16k_10s | audio | 5 | 1.02 items/s | 1.63 items/s | **1.60x** | 3702 | 2263 | 1.0000000 | n/a |
| wav16k_25s | audio | 3 | 0.57 items/s | 0.69 items/s | **1.21x** | 3702 | 2355 | 1.0000000 | n/a |
| wav16k_3s | audio | 1 | 1.14 items/s | 3.47 items/s | **3.03x** | 3701 | 2187 | 1.0000000 | n/a |
| wav16k_40s | audio | 3 | 0.58 items/s | 0.59 items/s | **1.03x** | 3702 | 2343 | 1.0000000 | n/a |
| wav16k_4s | audio | 3 | 0.98 items/s | 3.51 items/s | **3.59x** | 3701 | 2157 | 1.0000000 | n/a |
| webm | video | 3 | 0.10 items/s | 0.18 items/s | **1.71x** | 3350 | 2397 | 1.0000000 | n/a |
| webp | image | 3 | 0.57 items/s | 0.63 items/s | **1.12x** | 2910 | 1648 | 1.0000000 | n/a |

**neomme: modalities, file types, sizes** (EPYC 75F3 (Zen 3, 2 sockets), 16 threads, batch 1)

| input group | modality | items | Python | Rust | Rust / Python | Python RSS MB | Rust RSS MB | dense min cos | late min cos |
|---|---|---|---|---|---|---|---|---|---|
| bmp | image | 3 | REJECTED: raise ValueError( | 1.43 items/s | **-** | - | 1322 | - | n/a |
| gif | image | 3 | 1.20 items/s | 1.44 items/s | **1.20x** | 1976 | 1342 | 1.0000000 | 1.0000000 |
| jpg | image | 3 | 1.18 items/s | 1.44 items/s | **1.22x** | 1969 | 1320 | 1.0000000 | 1.0000000 |
| nativebmp | image | 1 | REJECTED: raise ValueError( | 1.31 items/s | **-** | - | 1128 | - | n/a |
| nativegif | image | 4 | REJECTED: ValueError: too many values to unpack (expected 4) | 15.58 items/s | **-** | - | 898 | - | n/a |
| nativejpg | image | 8 | 2.21 items/s | 2.48 items/s | **1.12x** | 1969 | 999 | 1.0000000 | 1.0000000 |
| nativepng | image | 4 | 1.79 items/s | 2.30 items/s | **1.28x** | 1969 | 1061 | 1.0000000 | 1.0000000 |
| nativetif | image | 8 | REJECTED: raise ValueError( | 0.68 items/s | **-** | - | 1853 | - | n/a |
| nativewebp | image | 4 | 2.49 items/s | 3.28 items/s | **1.32x** | 1969 | 985 | 1.0000000 | 1.0000000 |
| png | image | 3 | 1.14 items/s | 1.42 items/s | **1.25x** | 1969 | 1316 | 1.0000000 | 1.0000000 |
| res1024 | image | 4 | 2.00 items/s | 2.21 items/s | **1.10x** | 1978 | 1001 | 1.0000000 | 1.0000000 |
| res1536 | image | 4 | 1.04 items/s | 1.36 items/s | **1.32x** | 1991 | 1169 | 1.0000000 | 1.0000000 |
| res2048 | image | 4 | 0.48 items/s | 0.77 items/s | **1.61x** | 2013 | 1387 | 1.0000000 | 1.0000000 |
| res512 | image | 4 | 4.78 items/s | 7.30 items/s | **1.53x** | 1972 | 915 | 1.0000000 | 1.0000000 |
| tiff | image | 3 | REJECTED: raise ValueError( | 1.45 items/s | **-** | - | 1311 | - | n/a |
| webp | image | 3 | 1.19 items/s | 1.38 items/s | **1.16x** | 1968 | 1306 | 1.0000000 | 1.0000000 |

</details>

<details><summary><b>EPYC 9655P (Zen 5, 1 socket, AVX-512)</b></summary>


**gemma: modalities, file types, sizes** (EPYC 9655P (Zen 5, 1 socket, AVX-512), 16 threads, batch 1)

| input group | modality | items | Python | Rust | Rust / Python | Python RSS MB | Rust RSS MB | dense min cos | late min cos |
|---|---|---|---|---|---|---|---|---|---|
| avi | video | 3 | 0.13 items/s | 0.32 items/s | **2.46x** | 3379 | 2409 | 1.0000000 | n/a |
| bmp | image | 3 | REJECTED: raise ValueError( | 1.30 items/s | **-** | - | 1650 | - | n/a |
| flac16k | audio | 3 | 1.59 items/s | 3.49 items/s | **2.20x** | 3700 | 2306 | 1.0000000 | n/a |
| gif | image | 3 | 0.68 items/s | 1.28 items/s | **1.87x** | 2906 | 1655 | 1.0000000 | n/a |
| jpg | image | 3 | 0.71 items/s | 1.29 items/s | **1.81x** | 2908 | 1651 | 1.0000000 | n/a |
| m4a | audio | 3 | 1.85 items/s | 3.55 items/s | **1.92x** | 3700 | 2243 | 1.0000000 | n/a |
| mov | video | 3 | 0.12 items/s | 0.31 items/s | **2.55x** | 3358 | 2408 | 1.0000000 | n/a |
| mp3 | audio | 3 | 1.57 items/s | 3.43 items/s | **2.19x** | 3700 | 2272 | 1.0000000 | n/a |
| mp4_10s | video | 7 | 0.12 items/s | 0.31 items/s | **2.58x** | 3439 | 2407 | 1.0000000 | n/a |
| mp4_12s | video | 2 | 0.11 items/s | 0.25 items/s | **2.29x** | 3607 | 2595 | 1.0000000 | n/a |
| mp4_20s | video | 3 | 0.06 items/s | 0.16 items/s | **2.63x** | 3924 | 3201 | 1.0000000 | n/a |
| mp4_40s | video | 1 | 0.04 items/s | 0.09 items/s | **2.54x** | 4724 | 4242 | 1.0000000 | n/a |
| mp4_4s | video | 3 | 0.39 items/s | 0.76 items/s | **1.95x** | 2962 | 1890 | 1.0000000 | n/a |
| mp4_9s | video | 1 | 0.14 items/s | 0.35 items/s | **2.58x** | 3281 | 2275 | 1.0000000 | n/a |
| nativeavi | video | 3 | 0.06 items/s | 0.17 items/s | **2.63x** | 4348 | 3699 | 1.0000000 | n/a |
| nativebmp | image | 1 | REJECTED: raise ValueError( | 1.35 items/s | **-** | - | 1679 | - | n/a |
| nativegif | image | 4 | REJECTED: ValueError: too many values to unpack (expected 3) | 1.30 items/s | **-** | - | 1690 | - | n/a |
| nativejpg | image | 8 | 0.69 items/s | 1.29 items/s | **1.87x** | 2906 | 1654 | 1.0000000 | n/a |
| nativem4v | video | 3 | 0.06 items/s | 0.15 items/s | **2.58x** | 4889 | 4384 | 1.0000000 | n/a |
| nativemkv | video | 1 | 0.12 items/s | 0.26 items/s | **2.22x** | 3490 | 2478 | 1.0000000 | n/a |
| nativemov | video | 1 | 0.04 items/s | 0.10 items/s | **2.62x** | 4802 | 4231 | 1.0000000 | n/a |
| nativemp3 | audio | 3 | 0.57 items/s | 1.47 items/s | **2.59x** | 3700 | 2367 | 1.0000000 | n/a |
| nativepng | image | 4 | 0.71 items/s | 1.31 items/s | **1.85x** | 2906 | 1645 | 1.0000000 | n/a |
| nativetif | image | 8 | REJECTED: raise ValueError( | 0.89 items/s | **-** | - | 2268 | - | n/a |
| nativewav | audio | 3 | 1.99 items/s | 3.80 items/s | **1.91x** | 3699 | 2224 | 1.0000000 | n/a |
| nativewebp | image | 4 | 0.71 items/s | 1.27 items/s | **1.79x** | 2905 | 1639 | 1.0000000 | n/a |
| ogg | audio | 3 | 1.81 items/s | 3.55 items/s | **1.96x** | 3700 | 2271 | 1.0000000 | n/a |
| opus | audio | 3 | 1.84 items/s | 3.51 items/s | **1.90x** | 3700 | 2251 | 1.0000000 | n/a |
| png | image | 3 | 0.69 items/s | 1.27 items/s | **1.84x** | 2906 | 1654 | 1.0000000 | n/a |
| res1024 | image | 4 | 0.72 items/s | 1.26 items/s | **1.74x** | 2918 | 1664 | 1.0000000 | n/a |
| res1536 | image | 4 | 0.72 items/s | 1.25 items/s | **1.73x** | 2927 | 1669 | 1.0000000 | n/a |
| res2048 | image | 4 | 0.71 items/s | 1.21 items/s | **1.72x** | 2946 | 1682 | 1.0000000 | n/a |
| res512 | image | 4 | 0.71 items/s | 1.26 items/s | **1.79x** | 2912 | 1643 | 1.0000000 | n/a |
| tiff | image | 3 | REJECTED: raise ValueError( | 1.32 items/s | **-** | - | 1654 | - | n/a |
| tok1120 | image | 16 | 0.10 items/s | 0.20 items/s | **2.05x** | 3692 | 2368 | 1.0000000 | n/a |
| tok140 | image | 16 | 1.62 items/s | 2.61 items/s | **1.61x** | 2995 | 1614 | 1.0000000 | n/a |
| tok560 | image | 16 | 0.27 items/s | 0.58 items/s | **2.10x** | 3000 | 1945 | 1.0000000 | n/a |
| tok70 | image | 16 | 2.99 items/s | 5.22 items/s | **1.74x** | 2993 | 1519 | 1.0000000 | n/a |
| wav | audio | 3 | 1.85 items/s | 3.44 items/s | **1.85x** | 3699 | 2265 | 1.0000000 | n/a |
| wav16k_0s | audio | 1 | 3.59 items/s | 18.04 items/s | **5.03x** | 3698 | 2151 | 1.0000000 | n/a |
| wav16k_10s | audio | 5 | 1.78 items/s | 3.58 items/s | **2.02x** | 3698 | 2265 | 1.0000000 | n/a |
| wav16k_25s | audio | 3 | 1.07 items/s | 1.77 items/s | **1.66x** | 3699 | 2354 | 1.0000000 | n/a |
| wav16k_3s | audio | 1 | 1.96 items/s | 6.57 items/s | **3.35x** | 3698 | 2166 | 1.0000000 | n/a |
| wav16k_40s | audio | 3 | 0.94 items/s | 1.50 items/s | **1.61x** | 3698 | 2367 | 1.0000000 | n/a |
| wav16k_4s | audio | 3 | 1.40 items/s | 6.50 items/s | **4.66x** | 3699 | 2196 | 1.0000000 | n/a |
| webm | video | 3 | 0.13 items/s | 0.31 items/s | **2.37x** | 3378 | 2406 | 1.0000000 | n/a |
| webp | image | 3 | 0.69 items/s | 1.29 items/s | **1.86x** | 2906 | 1645 | 1.0000000 | n/a |

**neomme: modalities, file types, sizes** (EPYC 9655P (Zen 5, 1 socket, AVX-512), 16 threads, batch 1)

| input group | modality | items | Python | Rust | Rust / Python | Python RSS MB | Rust RSS MB | dense min cos | late min cos |
|---|---|---|---|---|---|---|---|---|---|
| bmp | image | 3 | REJECTED: raise ValueError( | 3.16 items/s | **-** | - | 1299 | - | n/a |
| gif | image | 3 | 1.43 items/s | 3.10 items/s | **2.17x** | 1971 | 1304 | 1.0000000 | 1.0000000 |
| jpg | image | 3 | 1.43 items/s | 3.14 items/s | **2.20x** | 1965 | 1308 | 1.0000000 | 1.0000000 |
| nativebmp | image | 1 | REJECTED: raise ValueError( | 2.50 items/s | **-** | - | 1114 | - | n/a |
| nativegif | image | 4 | REJECTED: ValueError: too many values to unpack (expected 4) | 22.36 items/s | **-** | - | 890 | - | n/a |
| nativejpg | image | 8 | 3.10 items/s | 5.68 items/s | **1.83x** | 1965 | 1001 | 1.0000000 | 1.0000000 |
| nativepng | image | 4 | 2.64 items/s | 5.06 items/s | **1.92x** | 1965 | 1054 | 1.0000000 | 1.0000000 |
| nativetif | image | 8 | REJECTED: raise ValueError( | 1.09 items/s | **-** | - | 1815 | - | n/a |
| nativewebp | image | 4 | 4.23 items/s | 7.78 items/s | **1.84x** | 1964 | 970 | 1.0000000 | 1.0000000 |
| png | image | 3 | 1.36 items/s | 3.12 items/s | **2.29x** | 1970 | 1319 | 1.0000000 | 1.0000000 |
| res1024 | image | 4 | 2.79 items/s | 5.12 items/s | **1.84x** | 1973 | 996 | 1.0000000 | 1.0000000 |
| res1536 | image | 4 | 1.24 items/s | 2.52 items/s | **2.03x** | 1986 | 1149 | 1.0000000 | 1.0000000 |
| res2048 | image | 4 | 0.60 items/s | 1.41 items/s | **2.36x** | 2011 | 1390 | 1.0000000 | 1.0000000 |
| res512 | image | 4 | 7.86 items/s | 12.36 items/s | **1.57x** | 1975 | 904 | 1.0000000 | 1.0000000 |
| tiff | image | 3 | REJECTED: raise ValueError( | 3.18 items/s | **-** | - | 1296 | - | n/a |
| webp | image | 3 | 1.42 items/s | 3.13 items/s | **2.20x** | 1964 | 1315 | 1.0000000 | 1.0000000 |

</details>

## Per-op time breakdown (custom-kernel question)

<details><summary><b>EPYC 7702 (Zen 2, 2 sockets)</b></summary>


**Per-op time share** (EPYC 7702 (Zen 2, 2 sockets), 16 threads, batch 1): torch eager vs Rust runner (ORT + Rust kernels)

| case | tokens | runtime | total s | GEMM attn-proj/other | GEMM MLP | attention core | norms | RoPE | embed / patch | other (elementwise, framework) | pre/post-processing |
|---|---|---|---|---|---|---|---|---|---|---|---|
| gemma-image | 277 | torch | 2.555 | 17% | 36% | 27% | 8% | 0% | 0% | 13% | 162 ms |
| gemma-image | 277 | rust | 2.201 | 14% | 34% | 33% | 8% | 0% | 0% | 0% | 43 ms |
| gemma-text | 128 | torch | 10.384 | 19% | 9% | 14% | 45% | 0% | 0% | 12% | 1 ms |
| gemma-text | 128 | rust | 0.166 | 27% | 30% | 17% | 9% | 0% | 0% | 3% | 1 ms |
| gemma-text | 2048 | torch | 2.190 | 22% | 24% | 36% | 9% | 0% | 0% | 8% | 6 ms |
| gemma-text | 2048 | rust | 1.395 | 22% | 21% | 34% | 7% | 1% | 0% | 3% | 5 ms |
| gemma-text | 512 | torch | 0.792 | 22% | 19% | 19% | 20% | 7% | 1% | 11% | 2 ms |
| gemma-text | 512 | rust | 0.473 | 27% | 30% | 26% | 6% | 0% | 0% | 1% | 2 ms |
| gemma-text | 8192 | torch | 19.213 | 11% | 13% | 59% | 7% | 0% | 0% | 8% | 19 ms |
| gemma-text | 8192 | rust | 6.833 | 15% | 16% | 57% | 5% | 1% | 0% | 2% | 21 ms |
| neomme-image | 2562 | torch | 4.343 | 19% | 25% | 32% | 8% | 0% | 0% | 15% | 246 ms |
| neomme-image | 2562 | rust | 1.858 | 31% | 29% | 25% | 5% | 0% | 0% | 1% | 30 ms |
| neomme-text | 16384 | torch | 61.492 | 7% | 13% | 53% | 2% | 0% | 0% | 26% | 71 ms |
| neomme-text | 16384 | rust | 13.693 | 25% | 18% | 47% | 3% | 0% | 0% | 2% | 25 ms |
| neomme-text | 4096 | torch | 5.853 | 16% | 29% | 36% | 1% | 0% | 0% | 17% | 33 ms |
| neomme-text | 4096 | rust | 2.509 | 34% | 24% | 30% | 5% | 0% | 0% | 1% | 8 ms |
| neomme-text | 512 | torch | 13.552 | 17% | 9% | 38% | 18% | 0% | 0% | 18% | 29 ms |
| neomme-text | 512 | rust | 0.406 | 40% | 27% | 18% | 5% | 0% | 0% | 1% | 2 ms |

</details>

<details><summary><b>EPYC 75F3 (Zen 3, 2 sockets)</b></summary>


**Per-op time share** (EPYC 75F3 (Zen 3, 2 sockets), 16 threads, batch 1): torch eager vs Rust runner (ORT + Rust kernels)

| case | tokens | runtime | total s | GEMM attn-proj/other | GEMM MLP | attention core | norms | RoPE | embed / patch | other (elementwise, framework) | pre/post-processing |
|---|---|---|---|---|---|---|---|---|---|---|---|
| gemma-image | 277 | torch | 1.723 | 16% | 31% | 28% | 9% | 0% | 0% | 16% | 129 ms |
| gemma-image | 277 | rust | 1.451 | 12% | 36% | 33% | 8% | 0% | 0% | 0% | 33 ms |
| gemma-text | 128 | torch | 0.142 | 27% | 30% | 13% | 19% | 0% | 0% | 10% | 1 ms |
| gemma-text | 128 | rust | 0.101 | 29% | 32% | 16% | 8% | 0% | 0% | 3% | 0 ms |
| gemma-text | 2048 | torch | 1.433 | 21% | 20% | 35% | 12% | 0% | 0% | 11% | 4 ms |
| gemma-text | 2048 | rust | 1.055 | 18% | 20% | 37% | 8% | 1% | 0% | 2% | 4 ms |
| gemma-text | 512 | torch | 0.418 | 24% | 25% | 23% | 17% | 0% | 0% | 12% | 1 ms |
| gemma-text | 512 | rust | 0.288 | 27% | 32% | 24% | 7% | 1% | 0% | 1% | 1 ms |
| gemma-text | 8192 | torch | 10.671 | 10% | 11% | 63% | 6% | 0% | 0% | 10% | 14 ms |
| gemma-text | 8192 | rust | 4.825 | 14% | 15% | 54% | 6% | 1% | 0% | 2% | 13 ms |
| neomme-image | 2562 | torch | 1.756 | 18% | 28% | 32% | 2% | 0% | 0% | 20% | 159 ms |
| neomme-image | 2562 | rust | 1.239 | 29% | 30% | 25% | 5% | 0% | 0% | 1% | 18 ms |
| neomme-text | 16384 | torch | 39.870 | 6% | 9% | 53% | 2% | 0% | 0% | 30% | 49 ms |
| neomme-text | 16384 | rust | 8.949 | 24% | 17% | 47% | 4% | 0% | 0% | 2% | 17 ms |
| neomme-text | 4096 | torch | 3.120 | 14% | 24% | 40% | 2% | 0% | 0% | 19% | 28 ms |
| neomme-text | 4096 | rust | 1.710 | 31% | 23% | 32% | 6% | 0% | 0% | 1% | 6 ms |
| neomme-text | 512 | torch | 0.350 | 22% | 38% | 22% | 5% | 0% | 0% | 13% | 24 ms |
| neomme-text | 512 | rust | 0.279 | 41% | 27% | 17% | 6% | 0% | 0% | 1% | 1 ms |

</details>

<details><summary><b>EPYC 9655P (Zen 5, 1 socket, AVX-512)</b></summary>


**Per-op time share** (EPYC 9655P (Zen 5, 1 socket, AVX-512), 16 threads, batch 1): torch eager vs Rust runner (ORT + Rust kernels)

| case | tokens | runtime | total s | GEMM attn-proj/other | GEMM MLP | attention core | norms | RoPE | embed / patch | other (elementwise, framework) | pre/post-processing |
|---|---|---|---|---|---|---|---|---|---|---|---|
| gemma-image | 277 | torch | 1.206 | 16% | 41% | 29% | 7% | 0% | 0% | 8% | 85 ms |
| gemma-image | 277 | rust | 0.723 | 11% | 33% | 33% | 11% | 0% | 0% | 1% | 24 ms |
| gemma-text | 128 | torch | 0.081 | 28% | 31% | 13% | 19% | 0% | 0% | 8% | 1 ms |
| gemma-text | 128 | rust | 0.067 | 21% | 24% | 23% | 12% | 1% | 0% | 5% | 0 ms |
| gemma-text | 2048 | torch | 1.017 | 22% | 26% | 39% | 8% | 0% | 0% | 6% | 3 ms |
| gemma-text | 2048 | rust | 0.507 | 18% | 20% | 41% | 8% | 1% | 0% | 3% | 3 ms |
| gemma-text | 512 | torch | 0.234 | 28% | 32% | 19% | 12% | 0% | 0% | 9% | 1 ms |
| gemma-text | 512 | rust | 0.150 | 22% | 24% | 27% | 10% | 1% | 0% | 2% | 1 ms |
| gemma-text | 8192 | torch | 10.217 | 11% | 11% | 63% | 8% | 0% | 0% | 8% | 10 ms |
| gemma-text | 8192 | rust | 2.976 | 12% | 13% | 62% | 4% | 1% | 0% | 3% | 11 ms |
| neomme-image | 2562 | torch | 1.484 | 20% | 32% | 30% | 2% | 0% | 0% | 16% | 141 ms |
| neomme-image | 2562 | rust | 0.632 | 28% | 27% | 26% | 8% | 0% | 0% | 1% | 9 ms |
| neomme-text | 16384 | torch | 30.205 | 6% | 10% | 57% | 1% | 0% | 0% | 25% | 36 ms |
| neomme-text | 16384 | rust | 5.057 | 23% | 17% | 46% | 5% | 0% | 0% | 3% | 14 ms |
| neomme-text | 4096 | torch | 2.944 | 14% | 25% | 38% | 1% | 0% | 0% | 22% | 24 ms |
| neomme-text | 4096 | rust | 0.928 | 30% | 22% | 32% | 8% | 0% | 0% | 1% | 5 ms |
| neomme-text | 512 | torch | 0.294 | 23% | 38% | 13% | 6% | 0% | 1% | 19% | 19 ms |
| neomme-text | 512 | rust | 0.143 | 32% | 24% | 22% | 8% | 0% | 0% | 2% | 1 ms |

</details>

What the breakdown says about further custom kernels: in torch, "other" (unfused elementwise ops and framework
overhead) is 8-26 % and grows with length. In the Rust runner it is 1-4 %, and attention is a Rust kernel at every
length. What remains is GEMM (40-60 %; MLAS reaches the same throughput as torch's MKL/oneDNN on these CPUs, about
870 GFLOP/s on 16 Zen 3 cores for NeoMME's MLP) and norms (5-11 %). Next candidates, none needed for parity or for
beating the card runtime: fusing Gemma's q/k RMSNorm and RoPE into the attention kernel's prologue (norms and the
RoPE slice/concat ops are about 12 % of the vision tower). In the audio tower the two ORT bottlenecks (clamps and
the channel-last LayerNorm) are already replaced, and MatMul dominates.

**Close cases and shared-node noise.** The first full run left four image groups at 0.93-0.99x on the 2-socket
nodes, so they were re-run with 8 interleaved reps each, and a GEMM microbenchmark (torch/MKL, ORT/MLAS
with and without spinning, and the `gemm` crate, `src/bin/linear_bench.rs`) ran at the same layer shapes:

- On a Zen 2 node at load ~108 of 128 cores, Rust won all four groups. With the NeoMME graph change below it won
  every rep: res1024 1.21x, native WebP 1.11x, Gemma res512 1.39x, NeoMME res512 1.37x.
- On a Zen 3 node at load ~120 of 64 cores (2x oversubscribed), the NeoMME groups were 0.78-0.82x with the old graph
  and 0.96x with the new one. Gemma res512 was 1.0x and NeoMME res512 1.24x.
- The GEMM microbenchmark does not show a consistent MLAS deficit. On the same shared nodes, torch's own throughput at
  674 x 1024 x 3584 swung from ~1,100 to ~160 GFLOP/s between runs, and MLAS was ahead in one run and behind in the
  other. The `gemm` crate was not faster than MLAS, so the GEMMs stay in ORT.

So the remaining sub-1.0x results come from heavily oversubscribed shared nodes: ORT's intra-op pool (one sync
per op, ~1,400 ops per NeoMME forward) degrades more there than torch's OpenMP, whose small elementwise ops stay
single-threaded. The thread-policy autotune helps (tuning times matched the timed loop) but cannot remove it. The one
code-side change that helped is in the graph: NeoMME's exclusive self-attention is exported without materialising
`repeat_kv(V)` and `F.normalize`'s `expand_as` (attention output viewed as [B,S,Hkv,G,D], broadcast against V),
which removes 34 `Expand` nodes per forward. Same math (wrapper vs HF cosine 0.9999999-1.0000001); all NeoMME
results above use it.

## Findings for the pipeline

- **EmbeddingGemma 2 max length**: sentence-transformers 6.3.0 leaves `max_seq_length` at the tokenizer's 1e30, so
  inputs over the 8,192-token context are *not* truncated unless the caller sets `model.max_seq_length = 8192`.
  The Rust runner truncates at `--max-len` (the over-max rows in the context tables check that both stop at exactly
  8,192 / 16,384 tokens).
- **Media need `max_seq_length` >= ~760**: one 30 s audio clip is 752 soft tokens; with the default 512 the card runtime
  raises a token-count mismatch (it does not silently truncate).
- **Audio is capped at 30 s** by `Gemma4AudioFeatureExtractor(max_length=480000)`; longer clips are truncated by both
  runtimes (the 81 s / 413 s / 1422 s real MP3s embed only their first 30 s). Video: 1 fps, at most 32 frames
  (uniform subsampling beyond), 140 soft tokens per frame.
- **torchcodec**: transformers prefers it for image/audio/video decoding. The PyPI wheel is a CUDA build that cannot
  load next to CPU torch, and once installed it breaks *all* HF media loading (even PNG). Use the CPU wheel
  (`--index-url https://download.pytorch.org/whl/cpu`) with FFmpeg shared libraries on `LD_LIBRARY_PATH`
  (`examples/slurm/embed-rs-venv.sbatch` links the ones bundled in the PyAV wheel; a cluster `ffmpeg` module with only static
  libraries is not enough). Without torchcodec the card runtime rejects video entirely, M4A audio, and TIFF/BMP images.
- **Rust accepts more**: images PNG/JPEG/GIF/WebP/TIFF/BMP (the `image` crate), any audio/video container ffmpeg can
  read (WAV, MP3, FLAC, Ogg/Vorbis, Opus, M4A/AAC, MP4, MOV, M4V, MKV, AVI, WebM).
- **Image-only dicts and mixed batches** (`[{"image": path}]`, `["text", {"image": path}]`) both work in the card runtime.
- **NeoMME**: one conversation is either text or a single image; text+image in one item raises in the chat template
  (so figure captions must be embedded separately). Dense `dense_dim` must be one of the trained Matryoshka sizes.
- **Images**: the HF processors resize with torchvision's uint8 antialias kernel; the Rust port (`src/pil_resize.rs`,
  Pillow algorithm with ATen's int16 weight quantisation and precision search) is bit-identical, up- and downscaling.
  Decoders matter as much as the resize: the `image` crate's JPEG decoder differs from libjpeg-turbo by +-1-2 per
  pixel (NeoMME late cosine 0.91 on photos), so JPEG goes through libjpeg-turbo (`mozjpeg` crate) and WebP through
  libwebp, as on the Python side.
- **Audio/video decoding**: decode with the same FFmpeg major version as the Python side (PyAV/torchcodec bundle
  FFmpeg 9; a cluster FFmpeg 6.1 gave AAC/Vorbis cosines 0.997-0.999). Frame sampling must use torchcodec's
  content-scanned metadata (frame count, `average_fps` = frames / (last pts + duration - first pts)), not header
  fields: MKV/WebM have no frame count in the header, and a 30/1 header rate vs a scanned 29.999999999999996 shifts
  every sampled frame by one (cosines 0.945-0.998 before, 1.0000000 after). Gemma's image budget (280 soft tokens, 16 px patches, 3x3 pooling) means a 2048 px page is
  embedded at ~624 x 1008 px; inputs above ~1000 px add decode cost but no detail.
- **Batching**: sort by length before batching (sentence-transformers does; the HF NeoMME processor does not), or a
  mixed-length batch pads every item to the longest one.

## How to run

Everything runs in Slurm jobs on a CPU cluster (job names `pigmed-embed-*`). Site specifics (account, QOS, paths,
`--output`) live in an untracked site env file that exports `EMBED_ROOT`, `SBATCH_ACCOUNT`, `SBATCH_QOS` and sources
`rust/pigmed-embed/py/env.sh`; jobs receive its path as `EMBED_ENV`.

1. Sync `rust/pigmed-embed/` to `$EMBED_ROOT/code` and `examples/slurm/embed-rs-*.sbatch` to `$EMBED_ROOT/code/slurm`
   (`rsync -rltz`, never `-a/-p/-g` on group-shared storage).
2. From `$EMBED_CODE`, add `--output=...` and `--export=ALL,EMBED_ENV=<site env>,...` to each `sbatch`:

| step | job | what |
|---|---|---|
| setup | `embed-rs-setup.sbatch` (`EMBED_BUNDLES=...`) | rustup toolchain, model snapshots, text/image inputs |
| venv | `embed-rs-venv.sbatch` (`EMBED_VENV=...`) | pinned torch-CPU venv on a uv-managed CPython; CPU torchcodec + FFmpeg libs |
| inputs | `embed-rs-media.sbatch`; `embed-rs-ctx.sbatch` builds long-context inputs on first run (`EMBED_CTX_TARS=...`) | real CC-BY media from the corpus bundles + converted format / resolution / length matrix, acceptance probe |
| export | `embed-rs-export.sbatch` (`ATTN=pigmed WHICH="gemma_lm gemma_vision gemma_vision_dyn neomme_text neomme_image" OUTDIR=$EMBED_ROOT/onnx-flash`; `gemma_audio` separately with `PIGMED_ONNX_OPTIMIZE=0`, as onnxscript's optimizer leaves a dangling input in that graph) | ONNX graphs + wrapper-vs-HF checks |
| ffmpeg | `embed-rs-ffmpeg.sbatch` | static FFmpeg 9 CLI (same major version as PyAV's bundled libraries) into `$EMBED_ROOT/ffmpeg9` |
| build | `embed-rs-build.sbatch` (`TAG=v21`) | `cargo build --release` on node-local disk, installs `$EMBED_ROOT/bin/pigmed-embed-$TAG`, `flash-check-$TAG` |
| bench | `embed-rs-bench.sbatch` (`MODEL MODALITY REPS BIN`) | thread x batch grid, Python vs Rust interleaved |
| context | `embed-rs-ctx.sbatch` (`MODEL REPS BIN`) | 128 ... max tokens, mixed batch, over-max, queries |
| mm | `embed-rs-mm.sbatch` (`MODEL BIN`) | image formats / resolutions / token budgets, audio and video formats and lengths |
| ops | `embed-rs-ops.sbatch` | per-op breakdown torch vs ORT+Rust |

Runner CLI (`ORT_DYLIB_PATH` = the venv's `onnxruntime/capi/libonnxruntime.so.1.31.1`; `ffmpeg`/`ffprobe` on PATH
or `PIGMED_FFMPEG`/`PIGMED_FFPROBE`, FFmpeg 9 for bit-exact audio/video, `examples/slurm/embed-rs-ffmpeg.sbatch` builds one):

```bash
pigmed-embed bench --model gemma|neomme --modality text|image|audio|video --threads 16 --batch 4 --n 32 \
  --inputs $EMBED_ROOT/inputs --onnx $EMBED_ROOT/onnx-flash --snapshot <hf snapshot dir> --out <dir> [--save] \
  [--texts-file f.jsonl] [--images-file f.jsonl] [--side document|query] [--max-len 8192] [--max-soft-tokens 280]
```

`--save` writes `<tag>.dense.npy` (NeoMME also `<tag>.late.npy` + `<tag>.late_offsets.npy`: each item's valid
rows, concatenated) and `<tag>.ids.json`; `<tag>.json` holds items/s, per-batch latency, time inside ONNX Runtime vs
Rust pre/post-processing, peak RSS (`getrusage`), CPU model, host and job ID. `flash-check [threads]` tests the
attention kernel alone against an f64 reference. The CPU target is AVX2+FMA (`.cargo/config.toml`); the attention
softmax switches to AVX-512 at runtime where available (`PIGMED_NO_AVX512=1` disables).

Files:

| file | what |
|---|---|
| `src/main.rs` | runner: sessions, batching, multimodal soft-token merge, mean pooling + L2, NeoMME dense/late |
| `src/flash.rs` | Rust custom ops: FlashAttention (AVX2/AVX-512 softmax, `gemm` tiles), AudioChunkAttention, ChannelLayerNorm; all on ORT's thread pool |
| `src/prep.rs`, `src/pil_resize.rs` | tokenization, Gemma 4 / NeoMME image preprocessing, Pillow/ATen-compatible resize |
| `src/media.rs` | ffmpeg audio/video decoding, Gemma 4 log-mel features, video frame sampling |
| `src/bin/flash_check.rs` | kernel correctness/speed check |
| `src/bin/candle_neomme.rs`, `src/bin/gemm_bench.rs` | candle probe and GEMM microbenchmark (feature `candle`) |
| `py/export_onnx.py` | ONNX export (custom attention op, RMSNorm fusion rewrite) + parity checks vs HF |
| `py/py_bench.py` | Python baseline (card runtime), reference vectors, captured model inputs |
| `py/make_inputs.py`, `py/make_long_inputs.py`, `py/make_media_inputs.py` | benchmark inputs |
| `py/compare.py`, `py/report.py`, `py/ctx_report.py` | tables + parity |
| `py/profile_ops.py`, `py/probe_media.py`, `py/sdpa_bench.py`, `py/ort_bench.py`, `py/ab_graphs.py`, `py/diag_*.py` | profiling, acceptance probe, diagnostics |

## Provenance

All results above come from one benchmark matrix (2026-11-12) and the same source tree; only the runner's build tag
differs by benchmark family. Gemma's fixed-length context sweeps ran on `v19`. The Gemma thread x batch
grids and Gemma mixed-length extras ran on `v20`, which also tunes the thread pool on the largest batch
(identical behaviour for fixed-length inputs). The Gemma modality groups ran on `v21` with the audio graph that uses
`Clip` and `ChannelLayerNorm` (audio-only changes). Every NeoMME job (grid, context, modalities, extras) and the
per-op profiles were re-run on `v21` with the final graph set `onnx-v22` (NeoMME exclusive-attention change above). Each result JSON records its CPU model and the thread policy chosen. On Zen 2 and Zen 3 the Gemma modality jobs hit their 3 h limit after 2 and 3 of 3 reps;
medians use the reps that finished.

<details><summary>Benchmark directories per CPU</summary>

| CPU | benchmark dir |
|---|---|
| EPYC 7702 (Zen 2, 2 sockets) | bench-gemma-image-t16 |
| EPYC 7702 (Zen 2, 2 sockets) | bench-gemma-image-t32 |
| EPYC 7702 (Zen 2, 2 sockets) | bench-gemma-text-t16 |
| EPYC 7702 (Zen 2, 2 sockets) | bench-gemma-text-t32 |
| EPYC 7702 (Zen 2, 2 sockets) | bench-neomme-image-t16 |
| EPYC 7702 (Zen 2, 2 sockets) | bench-neomme-image-t32 |
| EPYC 7702 (Zen 2, 2 sockets) | bench-neomme-text-t16 |
| EPYC 7702 (Zen 2, 2 sockets) | bench-neomme-text-t32 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-gemma-L1024-t16 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-gemma-L1024-t32 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-gemma-L128-t16 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-gemma-L128-t32 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-gemma-L2048-t16 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-gemma-L2048-t32 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-gemma-L4096-t16 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-gemma-L4096-t32 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-gemma-L512-t16 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-gemma-L512-t32 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-gemma-L8192-t16 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-gemma-L8192-t32 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-gemma-extras-t16 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-gemma-over-t16 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-gemma-over-t32 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-neomme-L1024-t16 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-neomme-L1024-t32 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-neomme-L128-t16 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-neomme-L128-t32 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-neomme-L16384-t16 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-neomme-L16384-t32 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-neomme-L2048-t16 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-neomme-L2048-t32 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-neomme-L4096-t16 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-neomme-L4096-t32 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-neomme-L512-t16 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-neomme-L512-t32 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-neomme-L8192-t16 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-neomme-L8192-t32 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-neomme-extras-t16 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-neomme-over-t16 |
| EPYC 7702 (Zen 2, 2 sockets) | ctx-neomme-over-t32 |
| EPYC 7702 (Zen 2, 2 sockets) | mm-gemma |
| EPYC 7702 (Zen 2, 2 sockets) | mm-neomme |
| EPYC 75F3 (Zen 3, 2 sockets) | bench-gemma-image-t16 |
| EPYC 75F3 (Zen 3, 2 sockets) | bench-gemma-text-t16 |
| EPYC 75F3 (Zen 3, 2 sockets) | bench-neomme-image-t16 |
| EPYC 75F3 (Zen 3, 2 sockets) | bench-neomme-text-t16 |
| EPYC 75F3 (Zen 3, 2 sockets) | ctx-gemma-L1024-t16 |
| EPYC 75F3 (Zen 3, 2 sockets) | ctx-gemma-L128-t16 |
| EPYC 75F3 (Zen 3, 2 sockets) | ctx-gemma-L2048-t16 |
| EPYC 75F3 (Zen 3, 2 sockets) | ctx-gemma-L4096-t16 |
| EPYC 75F3 (Zen 3, 2 sockets) | ctx-gemma-L512-t16 |
| EPYC 75F3 (Zen 3, 2 sockets) | ctx-gemma-L8192-t16 |
| EPYC 75F3 (Zen 3, 2 sockets) | ctx-gemma-extras-t16 |
| EPYC 75F3 (Zen 3, 2 sockets) | ctx-gemma-over-t16 |
| EPYC 75F3 (Zen 3, 2 sockets) | ctx-neomme-L1024-t16 |
| EPYC 75F3 (Zen 3, 2 sockets) | ctx-neomme-L128-t16 |
| EPYC 75F3 (Zen 3, 2 sockets) | ctx-neomme-L16384-t16 |
| EPYC 75F3 (Zen 3, 2 sockets) | ctx-neomme-L2048-t16 |
| EPYC 75F3 (Zen 3, 2 sockets) | ctx-neomme-L4096-t16 |
| EPYC 75F3 (Zen 3, 2 sockets) | ctx-neomme-L512-t16 |
| EPYC 75F3 (Zen 3, 2 sockets) | ctx-neomme-L8192-t16 |
| EPYC 75F3 (Zen 3, 2 sockets) | ctx-neomme-extras-t16 |
| EPYC 75F3 (Zen 3, 2 sockets) | ctx-neomme-over-t16 |
| EPYC 75F3 (Zen 3, 2 sockets) | mm-gemma |
| EPYC 75F3 (Zen 3, 2 sockets) | mm-neomme |
| EPYC 9655P (Zen 5, 1 socket, AVX-512) | bench-gemma-image-t16 |
| EPYC 9655P (Zen 5, 1 socket, AVX-512) | bench-gemma-text-t16 |
| EPYC 9655P (Zen 5, 1 socket, AVX-512) | bench-neomme-image-t16 |
| EPYC 9655P (Zen 5, 1 socket, AVX-512) | bench-neomme-text-t16 |
| EPYC 9655P (Zen 5, 1 socket, AVX-512) | ctx-gemma-L1024-t16 |
| EPYC 9655P (Zen 5, 1 socket, AVX-512) | ctx-gemma-L128-t16 |
| EPYC 9655P (Zen 5, 1 socket, AVX-512) | ctx-gemma-L2048-t16 |
| EPYC 9655P (Zen 5, 1 socket, AVX-512) | ctx-gemma-L4096-t16 |
| EPYC 9655P (Zen 5, 1 socket, AVX-512) | ctx-gemma-L512-t16 |
| EPYC 9655P (Zen 5, 1 socket, AVX-512) | ctx-gemma-L8192-t16 |
| EPYC 9655P (Zen 5, 1 socket, AVX-512) | ctx-gemma-extras-t16 |
| EPYC 9655P (Zen 5, 1 socket, AVX-512) | ctx-gemma-over-t16 |
| EPYC 9655P (Zen 5, 1 socket, AVX-512) | ctx-neomme-L1024-t16 |
| EPYC 9655P (Zen 5, 1 socket, AVX-512) | ctx-neomme-L128-t16 |
| EPYC 9655P (Zen 5, 1 socket, AVX-512) | ctx-neomme-L16384-t16 |
| EPYC 9655P (Zen 5, 1 socket, AVX-512) | ctx-neomme-L2048-t16 |
| EPYC 9655P (Zen 5, 1 socket, AVX-512) | ctx-neomme-L4096-t16 |
| EPYC 9655P (Zen 5, 1 socket, AVX-512) | ctx-neomme-L512-t16 |
| EPYC 9655P (Zen 5, 1 socket, AVX-512) | ctx-neomme-L8192-t16 |
| EPYC 9655P (Zen 5, 1 socket, AVX-512) | ctx-neomme-extras-t16 |
| EPYC 9655P (Zen 5, 1 socket, AVX-512) | ctx-neomme-over-t16 |
| EPYC 9655P (Zen 5, 1 socket, AVX-512) | mm-gemma |
| EPYC 9655P (Zen 5, 1 socket, AVX-512) | mm-neomme |

</details>
