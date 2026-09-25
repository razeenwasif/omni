#!/usr/bin/env bash
# Build (if needed) and run the Omni search service for Flux.
#   - Loads the prebuilt index `omni.idx` if present (instant start).
#   - Otherwise builds one from `store/` (crawler output) or `corpus/` and saves it.
# Binds 0.0.0.0:8080 by default so the server is reachable both from WSL AND
# from the Windows host — when Flux runs natively on Windows, its page webview
# loads http://localhost:8080 and WSL forwards that in (same as Ollama).
# 127.0.0.1 would be WSL-only. Override the BIND address with OMNI_BIND.
# NOTE: this is the *listen* address; the URL Flux connects to is set by
# register-flux.sh (OMNI_HOST, default localhost:8080) — keep them distinct.
set -euo pipefail
cd "$(dirname "$0")/.."

ADDR="${OMNI_BIND:-0.0.0.0:8080}"
cargo build --release --manifest-path core/Cargo.toml
BIN=core/target/release/omni

# --essential adds the curated necessity-site launch cards (YouTube, GitHub,
# Overleaf, …); it's idempotent, so it's safe on every start. !bang shortcuts
# (e.g. !gh rust) work regardless. The live background merge + POST /ingest
# endpoint are on by default (tune with --bg-merge-secs).
#
# --embed ollama uses a local Ollama model (nomic-embed-text) for *real* semantic
# search, fused with BM25 (hybrid). Needs Ollama running on localhost:11434; if
# it's unreachable, embedding/query gracefully fall back to lexical-only. Override
# with OMNI_EMBED (e.g. OMNI_EMBED=off, or OMNI_EMBED=hash for the offline embedder).
EMBED="${OMNI_EMBED:-ollama}"
if [ -e omni.idx ]; then
  echo "omni: loading prebuilt index dir (delete omni.idx/ to rebuild)"
  exec "$BIN" --index omni.idx --essential --embed "$EMBED" --addr "$ADDR"
elif ls store/*.doc >/dev/null 2>&1; then
  exec "$BIN" --docs store --index omni.idx --essential --embed "$EMBED" --addr "$ADDR"
else
  echo "omni: no crawl in store/ — indexing the local corpus/ instead"
  exec "$BIN" --corpus corpus --index omni.idx --essential --embed "$EMBED" --addr "$ADDR"
fi
