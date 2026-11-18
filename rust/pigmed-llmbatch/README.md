# pigmed-llmbatch

Resumable batch client for an OpenAI-compatible chat server (vLLM, SGLang).

```
tasks.jsonl  {"id", "messages", "max_tokens"?, "schema"?}  ─▶  results.jsonl  {"id", "content", tokens, "ms"} | {"id", "error"}
```

- Ids already in the output (without `error`) are skipped, so a rerun resumes.
- Tasks go out in input order, so tasks sharing a long prefix stay adjacent and hit the server's prefix cache.
- `schema` turns on JSON-schema guided decoding where the server supports it.
- A summary (wall time, token totals) is printed at the end.

```bash
cargo run --release -- --tasks tasks.jsonl --out results.jsonl --endpoint http://127.0.0.1:8000/v1 --model <name> --concurrency 64
```
