#!/usr/bin/env bash
# Add documents to a RUNNING Omni index at runtime (no restart) via POST /ingest.
# The server builds a new segment from the records, embeds them if the index is
# embedded, atomically swaps the grown index in, and persists it; the background
# merger folds the new segment into the tiers later. Already-indexed urls are
# skipped (use scripts/update.sh for replacing existing pages).
#
# Records are doc-store format — header lines, a blank line, then the body:
#     url: https://example.com/page
#     title: A Page
#     links: https://example.com/other
#     <blank line>
#     body text …
# Multiple records are separated by a line containing only `---`.
#
# Usage:
#   scripts/ingest.sh path/to/records.txt        # ingest a file
#   scripts/ingest.sh < records.txt              # ingest from stdin
#   ls store/*.doc | head | xargs cat | scripts/ingest.sh   # re-ingest some docs
# Override the target with OMNI_HOST (default localhost:8080).
set -euo pipefail

HOST="${OMNI_HOST:-localhost:8080}"
DATA="${1:-/dev/stdin}"

curl -fsS -X POST --data-binary "@${DATA}" "http://${HOST}/ingest"
echo
