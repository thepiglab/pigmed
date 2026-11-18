# Python pipeline (`src/pigmed`)

The original Python implementation of acquisition, embedding, publishing and exact search. Acquisition has since
moved to Rust (`rust/pigmed-ingest`, same bundle format), and corpus embedding to the Rust/GPU runners; the Python
tools still read and publish those bundles. Measured Python-vs-Rust timings are in [TEST_LADDER.md](TEST_LADDER.md).

## What is implemented

- Current PMC S3 inventory planning, latest-version selection, stable hash sampling, and metadata-first license filtering.
- Annual PubMed baseline and daily-update planning, streamed XML processing, and an idempotent citation/tombstone materializer.
- JATS sections, paragraphs, captions, tables, formulas, identifiers, licensing, retraction flags, and provenance.
- Separate full PDF page renders and figure views; optional timestamped video and audio clips.
- Model-agnostic archival bundles; re-embedding/rechunking without original article downloads when views are archived.
- Pinned NeoMME and EmbeddingGemma 2 adapters; explicit inference dtype, context checks, CPU/GPU device selection, and benchmark commands.
- Immutable metadata/vector/media shards, checksums, checkpoint recovery, reservation-based backpressure, and verify-before-delete export.
- Hugging Face private-dataset publisher and an offline local sink. JSONL.gz metadata is directly streamable; optional Parquet metadata conversion is separate.
- Small exact-search reference, retrieval metrics, paper-group splits, media-prevalence audit, and a single-device NeoMME adaptation trainer.
- CPU and optional GPU benchmark Slurm templates (site settings via `PIGMED_ENV` / `SBATCH_*`).

**Not implemented:** production approximate-nearest-neighbor serving, a web UI, a BM25 service, an OCR/captioning fallback, the paper's hierarchical/binary 255× late-vector compression, full distributed training, cross-paper duplicate detection, or automatic corpus-wide PMC removal reconciliation. All are described as extensions, not silently simulated.

**Validation:** see [TESTING.md](../TESTING.md). The offline tests cover dataflow and safety invariants; model quality and live services are covered by the measured runs in TEST_LADDER.md.

## Recommended first run

Do not start with millions of PDFs. Use low-priority CPU capacity for a small model-agnostic pilot, while leaving existing GPU workloads untouched. The defaults are a 200 GiB persistent outbox and eight concurrent 8-core workers. These are conservative starting settings, not throughput promises.

Run setup and inventory planning in an appropriate allocated compute/data-transfer session, not as a large login-node workload. Verify your actual group directory and permission first:

```bash
source "$PIGMED_ENV"        # your site env (configs/site.env.example): PIGMED_CODE, PIGMED_WORK, caches
mkdir -p "$PIGMED_WORK/logs"
cd "$PIGMED_CODE"

# Install once before array submission. The resulting uv.lock should be retained.
uv sync --extra render --extra hub --extra test
uv run pigmed doctor --root "$PIGMED_WORK"
uv run pytest -q

# Immutable source manifest: latest available version per PMCID, all publication years.
# A 1/1000 stable hash sample is roughly a several-thousand-paper pilot, not an exact count.
# This reads the inventory, not all article bodies. Inspect resulting plan.json before proceeding.
uv run pigmed plan-pmc --out "$PIGMED_WORK/plans/pmc-pilot" \
  --sample-mod 1000 --sample-rem 0 --per-part 100

export PLAN_DIR="$PIGMED_WORK/plans/pmc-pilot"
export PIGMED_CONFIG="$PIGMED_CODE/configs/ingest-pilot.json"
export CONCURRENCY=8   # ACCOUNT/QOS come from SBATCH_ACCOUNT/SBATCH_QOS in your site env
bash scripts/submit_ingest.sh
```

`submit_ingest.sh` submits jobs when you execute it. Confirm the QOS name and resource authorization in your account. Confirm the allocation is authorized for a large research corpus.

For a tiny connectivity smoke test, use `--limit 10 --per-part 5`. A limited prefix is **not** a representative sample for estimating media prevalence.

### Storage modes

`configs/ingest-pilot.json` retains rendered pages and figures in TAR files so competing models can be tested on identical views. This is intentionally a pilot configuration; retaining all rendered PNG pages is not the corpus-scale default.

`configs/ingest-text-first.json` archives canonical JATS/text without downloading PDFs/media. Media source URLs remain in canonical metadata. Its provisional chunks use an explicitly labeled word counter. Rechunk with the selected model tokenizer before final embedding.

The model-specific templates set `archive_media=false`. Media bytes are transient; final bundles retain vector rows, canonical text, source URLs/checksums, render recipes, and locators. Re-embedding media later requires re-fetching verified sources. Old remote objects may change or disappear, so archive a rights-cleared evaluation set permanently.

### PubMed citation catalog, all years

```bash
uv run pigmed plan-pubmed --out "$PIGMED_WORK/plans/pubmed-baseline"
uv run pigmed plan-pubmed --updates --out "$PIGMED_WORK/plans/pubmed-updates"
```

The current annual baseline contains the current citation database across publication years. It is not restricted to its release year. Each task downloads one compressed XML archive, verifies its MD5, iterates records, commits output shards, and removes the temporary archive.

Ingest these plans with `configs/ingest-text-first.json`. Citation shards intentionally require a separate terms review before Hub export. To create a current catalog from locally present change-log shards:

```bash
uv run pigmed materialize-citations \
  "$PIGMED_WORK/runs/*/ready/*/items.jsonl.gz" \
  --database "$TMPDIR/pubmed-current.sqlite" \
  --out "$PIGMED_WORK/pubmed-current.jsonl.gz"
```

Use a persistent copy/checkpoint of the single-writer SQLite database if needed; `$TMPDIR` disappears when the allocation ends. Retain citation event shards remotely and materialize a snapshot from a complete downloaded event set. Do not mix different annual baseline generations in one database. The pipeline exports citation events but does **not** yet turn the resulting citation catalog into a separate abstract-vector index.

### Pin, prefetch, and compare real models

```bash
uv sync --extra render --extra hub --extra embed --extra test
uv run pigmed lock-model google/embeddinggemma-2 \
  --out "$PIGMED_WORK/gemma2-lock.json" --download
uv run pigmed lock-model Hcompany/NeoMME-260M-Retriever \
  --out "$PIGMED_WORK/neomme-lock.json" --download
uv run pigmed doctor --root "$PIGMED_WORK"

uv run python scripts/bind_model.py \
  --template configs/embed-gemma2.template.json \
  --lock "$PIGMED_WORK/gemma2-lock.json" \
  --out "$PIGMED_WORK/gemma2-run.json"
```

Use `gemma2-run.json` as `PIGMED_CONFIG` for end-to-end acquisition plus embedding. `--device cuda` selects a GPU, but submit to a GPU allocation, not a CPU-only QOS. Exact library versions must be pinned after a successful real-model smoke test. `doctor` checks interface availability but does not validate inference.

EmbeddingGemma 2 uses FP32 on CPU and BF16 on supported CUDA devices. **FP16 inference is intentionally avoided** because its official model card warns about numerical failure. Stored normalized vectors are FP16; this is distinct from model execution precision.

To reuse a model-agnostic archived-view bundle:

```bash
export MODEL_SHA=$(python -c 'import json,os; print(json.load(open(os.environ["PIGMED_WORK"]+"/gemma2-lock.json"))["revision"])')
uv run pigmed reembed --bundle /absolute/path/to/one/committed/bundle \
  --root "$PIGMED_WORK/gemma2-pilot" --backend gemma2 \
  --revision "$MODEL_SHA" --device cpu --batch-size 1 \
  --chunk-tokens 512 --overlap-tokens 64
```

This duplicates source assets into the new bundle for a self-contained pilot. At production scale, use a shared asset store/manifest reference layer rather than keeping identical media in every model's dataset. The included trainer saves a local checkpoint; freeze/version and publish your own model checkpoint before using the SHA-pinned corpus inference commands.

### Benchmark CPU and GPU fairly

Create separate text/page/figure benchmark files; avoid comparing models on different input types:

```bash
uv run python scripts/make_benchmark_inputs.py \
  --bundle /absolute/path/to/archived/pilot/bundle \
  --out "$PIGMED_WORK/bench-pages" --kind page --limit 100
export BENCH_INPUTS="$PIGMED_WORK/bench-pages/inputs.jsonl"
export BACKEND=gemma2 MODEL_ID=google/embeddinggemma-2
# MODEL_SHA, PIGMED_CODE, PIGMED_WORK set above; model already cached.
sbatch examples/slurm/benchmark-cpu.sbatch          # account/QOS from SBATCH_ACCOUNT/SBATCH_QOS
# OPTIONAL: only when a GPU is actually available and account authorization is confirmed.
sbatch --account=<gpu-account> --qos=<gpu-qos> --partition=<gpu-partition> examples/slurm/benchmark-gpu.sbatch
```

Benchmark 4/8/16 CPU threads and batches 1/2/4 separately. Benchmark GPU batches 1/4/8/16 as memory permits. Measure end-to-end ingestion too: the model-only benchmark excludes remote acquisition, PDF rendering, and Hub upload. CPU batch size is not inherently smaller than GPU batch size; RAM, attention cost, memory bandwidth, and throughput determine the useful batch size.

### Verified publishing and local deletion

Authenticate once using `hf auth login` or a protected `HF_TOKEN` environment variable. Do not embed tokens in manifests, shell scripts, or Slurm logs. Start with a private dataset and confirm hosting/storage terms:

```bash
uv run pigmed publish --root "$PIGMED_WORK" \
  --repo YOUR_ACCOUNT/pigmed-pilot --create-private --limit 1
# Inspect the remote bundle and dataset card first; then enable draining.
uv run pigmed publish --root "$PIGMED_WORK" \
  --repo YOUR_ACCOUNT/pigmed-pilot --delete-after-verify
```

For a concurrent bounded run, set `HF_REPO` to an existing private dataset and submit `examples/slurm/publish.sbatch` under a regular (non-preemptible) QOS. The publisher hashes every remote file at the committed revision before deleting local data. This round-trip readback costs bandwidth. It is safer than equating “upload call returned” with “verified durable copy.” A provider-checksum-aware verifier could later replace full readback.

A private repository does not grant redistribution rights. The default ingestion policy accepts only machine-readable CC0/CC-BY article licenses, skips explicitly flagged third-party figures, and records provenance. It is a filter, not a comprehensive rights determination. Review PDFs and supplements for exceptions before publishing, even privately. Citation export additionally requires `--reviewed-pubmed-terms` after that specific review.

If upload stops, reservations eventually fill the outbox and producers exit **75** rather than grow without bound. Restart those array tasks after draining. Hard crashes can leave `worker.lock` or `publisher.lock`; inspect its recorded host/PID/Slurm job, confirm the job is dead, then remove the stale lock. Never auto-delete live locks. Test cross-node `flock` behavior on the actual filesystem before broad multi-node use.

### Bundle format

```
BUNDLE/
  manifest.json       # commit marker; source checkpoint, embedding identity, file SHA-256
  items.jsonl.gz      # article/unit rows, provenance, vector offsets, asset members
  assets.tar          # canonical XML/JSON; optional rendered figures/pages/clips
  dense.f16           # contiguous little-endian normalized vectors
  late.i8             # optional token vectors, no pooling
  late-scales.f32     # per-vector dequantization scales
```

The reference prototype reads this layout directly. HF Datasets can stream JSONL metadata; it does not automatically resolve binary offsets/TAR media into ready-to-use vector rows. A custom reader or conversion step is required for that. `pigmed to-parquet` writes stable-schema metadata, not an automatically indexed/searchable database.

Each embedding identity includes the model revision and preprocessing recipe. Query and document embeddings from unrelated models must never be compared by a single dot product; use separate indexes and fuse ranks.

### Retrieval, media audit, and training

See [DESIGN.md](DESIGN.md), [TRAINING.md](TRAINING.md), and [SOURCES.md](SOURCES.md). Examples:

```bash
# Count unique articles with discoverable video/audio filenames; report denominator and Wilson CI.
uv run pigmed audit-media "$PIGMED_WORK/plans/pmc-pilot/part-*.jsonl" \
  --sample-size 1000 --out "$PIGMED_WORK/media-audit.json"

# Prototype exact search; do not run this linear scan against a hundred-million-unit corpus.
uv run pigmed search --bundles '/path/to/gemma2-pilot/runs/*/ready/*' \
  --backend gemma2 --revision "$MODEL_SHA" --image /path/to/figure.png --text 'similar findings' --k 10

# Optional Parquet metadata only.
uv run pigmed to-parquet --bundle /path/to/bundle --out /path/to/items.parquet
```

Install FFmpeg/ffprobe through your site's approved environment before enabling media clipping. The code does not assume a particular module name. `archive_media=false` prevents materializing image queries from old shards without fetching the source again. NeoMME image-query experiments require an explicit `--allow-experimental-image-query` flag: text-to-page success does not establish photographed-figure retrieval quality.

### Optional compressed-baseline size check

To decide whether the **citation baseline** fits locally, measure the planned archive sizes rather than guessing from the publication count:

```bash
uv run python scripts/measure_pubmed_archives.py \
  "$PIGMED_WORK/plans/pubmed-baseline/part-*.jsonl"
```

This reads response metadata without saving archives and reports incomplete measurements explicitly. It does not estimate full-text PDFs or supplementary media. Streaming remains the default whether or not the compressed citation baseline fits.
