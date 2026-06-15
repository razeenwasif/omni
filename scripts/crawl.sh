#!/usr/bin/env bash
# Crawl a site into the doc store, then force the index to rebuild on next serve.
#   usage: scripts/crawl.sh <seed-url> [max-pages] [delay]
# Example: scripts/crawl.sh https://doc.rust-lang.org/book/ 100 200ms
set -euo pipefail
cd "$(dirname "$0")/.."

SEED="${1:?usage: crawl.sh <seed-url> [max-pages] [delay]}"
MAX="${2:-100}"
DELAY="${3:-200ms}"

( cd crawler && go run . -seeds "$SEED" -out ../store -max "$MAX" -delay "$DELAY" )
rm -f omni.idx   # stale: force serve.sh to rebuild from the new crawl
echo "omni: crawl done. Run scripts/serve.sh to (re)build the index and serve."
