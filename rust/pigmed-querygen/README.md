# pigmed-querygen

Config-driven synthetic-query generator for the PigMed retriever. Everything that defines a query (personas, intent
cards, writing style, figure arm, reject rules) lives in [`querygen/`](../../querygen): one TOML config plus prompt
templates. The code only reads it. Spec: [docs/QUERYGEN_SPEC.md](../../docs/QUERYGEN_SPEC.md).

```
persona library ─▶ stage A (intent cards per article) ─▶ stage B (surface queries) ─▶ judge / rejects ─▶ queries
      ▲                    ▲  figure cards (image + caption + citing text)                     │
 querygen/*.toml      any OpenAI-compatible server (vLLM / SGLang)                         mix report
```

| Command | What it does |
|---|---|
| `personas` | Build the deterministic persona library from a config |
| `render` | Write the prompts and schemas for N articles (no LLM call) |
| `fig-inputs` | Prepare figure inputs (image, caption, citing chunks) |
| `generate` | Run one arm against an OpenAI-compatible endpoint (text + figure outputs) |
| `mix` | Library, planned and realised query mixes |
| `mock-serve` | Schema-faithful fake server for tests and dry runs |
| `peek`, `compare` | Inspect kept queries; diff two runs' requests and outputs |

Configs chain with `extends`: `v3.1d.toml` → `fig.toml` → `v3.1e.toml` → `querygen.toml` (v3.2).
`querygen/parity.sh` checks that `v3.1d.toml` reproduces `pigmed-synth`'s v3.1d recipe byte for byte.

```bash
cargo build --release && cargo test
```
