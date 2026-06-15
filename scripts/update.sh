#!/usr/bin/env bash
# Incrementally refresh the index: (re)crawl into the store, then apply just the
# changed pages to the existing omni.idx (add/replace/delete) — no full rebuild.
#   usage: scripts/update.sh <seed-url> [max-pages] [delay]
set -euo pipefail
cd "$(dirname "$0")/.."

SEED="${1:?usage: update.sh <seed-url> [max-pages] [delay]}"
MAX="${2:-100}"
DELAY="${3:-200ms}"

( cd crawler && go run . -seeds "$SEED" -out ../store -max "$MAX" -delay "$DELAY" )
cargo build --release --manifest-path core/Cargo.toml
# Build & exit (--no-serve) so we don't collide with a running serve.sh.
core/target/release/omni --update store --index omni.idx --no-serve
echo "omni: index updated — run scripts/serve.sh to serve (or restart it)."
