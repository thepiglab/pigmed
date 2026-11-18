# Biomedical adaptation without leaking evaluation data

## Recommendation

Start from **NeoMME-260M-Retriever**, not a freshly initialized NeoMME backbone. First compare its unmodified retrieval against EmbeddingGemma 2, MedCPT for text, and BiomedCLIP for figures. Biomedical fine-tuning is worthwhile if there is a measured failure that better scientific training pairs can address. A from-scratch foundation model would add large data/compute requirements without isolating the value of biomedical specialization.

The most relevant objective for this project is not “all PubMed next-token prediction.” It is accurate cross-modal evidence retrieval: text ↔ scientific figure/page and figure/photo ↔ related evidence, with distinction between topical similarity and genuinely supporting evidence.

## Data sources and curriculum

Use your rights-reviewed PMC corpus for figure-caption pairs, figure-referencing paragraphs, table questions, and text-passage pairs. Preserve captions and provenance rather than relying only on synthetic descriptions. MIRIAD is a candidate source of synthetic biomedical question/evidence pairs, subject to its license and upstream rights; it should not be the only evaluation source.

Start with a pilot of roughly 10,000–100,000 reviewed/filtered pairs. Suggested mixture to test, not an established optimum: 40% biomedical text queries to passages, 40% text queries to figures/pages, 20% image/photo query pairs and general-domain replay combined. Vary the mix based on held-out failures. Crop/blur/perspective augmentations can support photograph robustness but must preserve clinically/scientifically important content.

For a mature training setup, mine difficult negatives from BM25 and a frozen retriever; distinguish a hard negative from another genuinely relevant passage. Mask uncertain same-paper negatives and retain multiple valid positives. Favor examples with conflicting units, anatomy, species, intervention, and study design rather than only superficially unrelated images. Include both plot-heavy and microscopy/radiology-style figures.

Avoid large synthetic-query feedback loops graded only by the same generator that produced the questions. Audit expert-labeled examples, exact source support, and copied-caption shortcuts. Use independent natural queries and at least some figure-dependent questions that cannot be answered from the caption alone.

## Split papers, not independent chunks

Build connected groups across PMCID versions, DOI/PMID aliases, preprint/publication pairs, duplicate figures, and related reports from the same study when leakage would matter. This clustering is a data-curation task; the included split helper does not infer duplicate studies by itself. Give it a `duplicate_cluster` field after clustering.

Assign approximately 96% of groups to training, 2% to development, and 2% to test by a stable seed. Small test fractions can still yield many thousands of held-out papers. Use a fixed representative manually judged evaluation subset rather than expecting that every held-out paper needs a query.

**Held-out documents belong in the searchable corpus.** They must be indexed so queries can retrieve them. Freezing a retriever and computing document embeddings is not gradient training. Withhold their relevance labels, derived training questions, figure-caption training pairs, teacher supervision, and gradient-bearing hard-negative use. Do not repeatedly tune hyperparameters on the test queries. A temporal evaluation on later papers is useful, but unknown foundation-model pretraining contamination cannot be ruled out merely by your new split.

Indexing every publication does not force training on every publication. That separation is the answer to the “we want as much of PubMed as possible” concern.

## Included trainer

`pigmed train-neomme` implements a small single-device full-parameter adaptation experiment with:
- official query/document preprocessing;
- joint normalized dense and mean-MaxSim late-interaction losses;
- multiple-positive masks and exclusion of uncertain same-paper negatives;
- FP32 master weights/optimizer states with optional BF16 CUDA autocast;
- gradient accumulation and clipping;
- a held-out-group blocklist check;
- a final safetensors checkpoint and training configuration/metrics.

It is **not** the authors' complete training pipeline. It does not implement explicit mined-negative batches, distributed cached losses, dynamic pooling-aware loss, optimizer-state resume, or automatic pair generation. Accumulating eight microbatches does not make each microbatch see eight times as many negatives. Use a supported multi-vector training framework or extend the sampler/loss after the first pilot.

Input JSONL format (each `query` and `positive` follows the encoder unit schema):

```json
{"split":"train","group_id":"study-cluster-001","query":{"kind":"text","text":"Which figure shows amplitude loss after vascular occlusion?"},"positive":{"unit_id":"PMC123.1:fig1","kind":"figure","media_path":"/absolute/path/to/figure.png","text":""},"positive_ids":["PMC123.1:fig1"]}
{"split":"train","group_id":"study-cluster-002","query":{"kind":"text","text":"How was the signal sampling procedure described?"},"positive":{"unit_id":"PMC456.1:passage2","kind":"text","text":"An illustrative training passage."}}
```

These two rows illustrate the format only. They are not real biomedical training data and are not enough to establish useful training.

```bash
uv run pigmed train-neomme \
  --input /path/to/reviewed-train-pairs.jsonl \
  --out /path/to/new-experiment-directory \
  --model-id Hcompany/NeoMME-260M-Retriever \
  --revision PINNED_40_CHARACTER_COMMIT_SHA \
  --blocklist /path/to/dev-and-test-group-ids.txt \
  --device cuda --batch-size 2 --accumulation 8 --lr 0.00002 --max-steps 1000
```

Run first as a short GPU smoke test. Vision patch length can dominate memory; reduce image resolution, shorten text, or increase memory deliberately rather than silently truncating evidence. The trainer loss itself is unit-tested on synthetic tensors; real-model training was not run here.

## Evaluation before final embedding

Compare frozen base model, biomedical-adapted model, and modality-specific baselines using identical candidate corpora, split groups, and query sets. Measure Recall@100/1000, nDCG@10, MRR, article-level and evidence-unit-level accuracy, image-query robustness, index bytes, and throughput. The included evaluator supports recall, nDCG, and MRR at a chosen k. Aggregate page/figure hits to paper IDs separately to prevent duplicate pages inflating article coverage.

Run ablations for text chunk size, image resolution, caption mixing, dense versus late scoring, compression, and multimodal training mixture. Include a general-domain retention test. Tune on development only, freeze all settings, then evaluate the test split once.

If training improves retrieval, publish a versioned checkpoint, freeze its tokenizer/processor and software environment, and only then generate the final full-corpus embeddings. Earlier shards remain labeled by their original embedding identity; never query them with a different checkpoint's vectors.
