#!/usr/bin/env bash
# Crawl the curated academic seed set (crawler/seeds/academic.txt) and
# incrementally fold it into the index. Builds the index and exits — run
# scripts/serve.sh (or restart it) afterwards to serve the expanded corpus.
#
#   usage: scripts/crawl-academic.sh [max-pages] [per-host] [delay]
#   e.g.:  scripts/crawl-academic.sh 3000 200 300ms
set -euo pipefail
cd "$(dirname "$0")/.."

MAX="${1:-1500}"
PERHOST="${2:-150}"
DELAY="${3:-300ms}"

( cd crawler && go run . -seedfile seeds/academic.txt -out ../store \
    -max "$MAX" -per-host "$PERHOST" -delay "$DELAY" -workers 8 )

cargo build --release --manifest-path core/Cargo.toml
# Incremental: fold the new/changed pages into the existing index (new docs are
# auto-embedded if the index already uses an embedder). --no-serve = build & exit.
core/target/release/omni --update store --index omni.idx --no-serve
echo "omni: index updated — run scripts/serve.sh to serve the expanded corpus."
