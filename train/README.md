# train/: retriever training (S1: late-interaction head on EmbeddingGemma 2)

Plan and results: `docs/RETRIEVER_TRAINING_PLAN.md`. Data sources and licences: `docs/RETRIEVER_DATA_PLAN.md`.

This is the accepted Python exception to the Rust-first rule. Training needs PyTorch. Embedding for mining and
the production-path dense eval run on flashembed (Rust).

## Package `eg2late`

| Module | What it does |
|---|---|
| `model.py` | `EG2Dual`: the EG2 text tower with its released dense head (mean pool of the 768-d token outputs, prompt included) plus `LateHead`, a 768→128 per-token head with a linear path and a zero-initialised GELU MLP residual, L2-normalised. MaxSim is the mean over query text tokens of the max over document text tokens; prompt, special and pad tokens are masked out. `export_snapshot` writes an HF snapshot that flashembed and sentence-transformers load unchanged, plus `late_head.safetensors`. |
| `train_s1.py` | One-GPU GradCache trainer. Losses: dense InfoNCE (768 + MRL 512/256/128), late InfoNCE, a symmetric dense↔late KL, and an optional KD term from a precomputed teacher score matrix. The default teacher is frozen base-EG2 dense scores, used as an anti-forgetting anchor; a vetted cross-encoder can plug in later. Batches are single-source, and gold documents are masked as negatives. |
| `prep_data.py` | Fetches upstream commercial-use sets (NQ pairs, HotpotQA distractor, SQuAD v2) and NanoBEIR-en, then writes prompted queries/docs and pairs. |
| `mine.py` | Hard negatives with base EG2 (flashembed vectors) using the NV-Retriever TopK-PercPos rule (drop above 0.95× the positive's score), gold and answer-string filters. Top-k runs on GPU. |
| `evaluate.py` | NanoBEIR, ConTEB (naive + late chunking, with a port of flashembed's 8k windowing) and the 98k abstract-sentence→body task, for both heads in one encoder pass. Includes a paired bootstrap `compare`. |

`tests/check_wrapper.py` checks five things: dense parity with sentence-transformers, the late mask, MaxSim
against a naive loop (and PyLate when it imports), GradCache against direct gradients, and an export round trip.

## Why not PyLate's trainer

PyLate 1.6 pins `sentence-transformers==5.3` and `transformers<=5.3`. EG2 needs transformers ≥ 5.18 and
sentence-transformers ≥ 6.1. PyLate is installed `--no-deps` for reference checks only. The trainer follows its
formulation: ColBERT MaxSim, contrastive with cached gradients, and score distillation.

## Running

```
python -m eg2late.prep_data fetch --out RAW && python -m eg2late.prep_data build --raw RAW --out DATA
# base-EG2 vectors per source (flashembed embed) -> DATA/<src>/{qvec,dvec}; then
python -m eg2late.mine --src DATA/<src>
python -m eg2late.train_s1 --base EG2_SNAPSHOT --data DATA --out RUN
python -m eg2late.evaluate nanobeir --model RUN/best --data RAW/nanobeir --out R.json
```

Licence of the base: EmbeddingGemma 2 is Apache 2.0. The card's licence link is the Gemma 4 page, which is the
plain Apache 2.0 text. The card also asks deployments to follow the Gemma Prohibited Use Policy, so a release should
ship the Apache licence and NOTICE, mark changed files, and link that policy.
