#!/usr/bin/env bash
# Restore the (not redistributed) guideline adapter fixtures and verify them against the public manifest.
# usage: scripts/fetch_guideline_fixtures.sh <dir | archive.tar.gz | hf://datasets/OWNER/REPO>
# The source must contain a `guidelines/` tree laid out as in rust/pigmed-ingest/tests/fixtures/MANIFEST.tsv.
set -euo pipefail
src="${1:?usage: $0 <dir | archive.tar.gz | hf://datasets/OWNER/REPO>}"
root="$(cd "$(dirname "$0")/.." && pwd)"
dest="$root/rust/pigmed-ingest/tests/fixtures"
manifest="$dest/MANIFEST.tsv"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT

case "$src" in
  hf://datasets/*)
    repo="${src#hf://datasets/}"
    command -v hf >/dev/null || { echo "needs the Hugging Face CLI (pip install huggingface_hub)" >&2; exit 2; }
    hf download "$repo" --repo-type dataset --local-dir "$tmp/src" >/dev/null
    src="$tmp/src" ;;
  *.tar.gz|*.tgz) mkdir -p "$tmp/src"; tar -xzf "$src" -C "$tmp/src"; src="$tmp/src" ;;
esac
[[ -d "$src/guidelines" ]] || { echo "no guidelines/ directory in $src" >&2; exit 2; }

mkdir -p "$dest/guidelines"
rsync -a "$src/guidelines/" "$dest/guidelines/"

sha() { if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -c1-64; }
ok=0; bad=0; missing=0
while IFS=$'\t' read -r want _size path; do
  [[ "$want" == \#* || -z "$want" ]] && continue
  f="$dest/$path"
  if [[ ! -f "$f" ]]; then missing=$((missing+1)); echo "missing  $path";
  elif [[ "$(sha "$f")" != "$want" ]]; then bad=$((bad+1)); echo "differs  $path";
  else ok=$((ok+1)); fi
done < "$manifest"
echo "fixtures: $ok ok, $bad differ, $missing missing"
[[ $bad -eq 0 && $missing -eq 0 ]]
