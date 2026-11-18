# Slurm templates

Generic batch templates. They contain **no site settings**: account, QOS, partitions, storage paths and caches come
from your own env file (copy [`configs/site.env.example`](../../configs/site.env.example) outside the repo and
`export PIGMED_ENV=<file>`; the embed templates use `EMBED_ENV`). Set `SBATCH_ACCOUNT` / `SBATCH_QOS` there, or pass
`-A` / `--qos` on the command line.

| Template | Purpose |
|---|---|
| `rs-build`, `rs-ingest`, `stream-publish` | Build `pigmed-rs`; one plan part per array task; ingest + verified Hub upload on one node |
| `ingest`, `publish` | Python ingest and publisher |
| `benchmark-cpu`, `benchmark-gpu` | Model-only embedding benchmarks |
| `embed-rs-setup`, `-venv`, `-build`, `-export`, `-ffmpeg`, `-media` | One-time setup for `rust/pigmed-embed` |
| `embed-rs-bench`, `-ctx`, `-mm`, `-ops`, `-run`, `-dev` | Parity and speed sweeps, ad-hoc runs |

Submit wrappers: [`scripts/submit_rs_ingest.sh`](../../scripts/submit_rs_ingest.sh) and
[`scripts/submit_ingest.sh`](../../scripts/submit_ingest.sh). Set `SBATCH_CMD` to wrap `sbatch` (for example with a
site account check).
