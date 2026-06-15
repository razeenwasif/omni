# Omni

A personal search engine, built from scratch — and the custom search backend for
the [Flux](../Flux) browser.

The full architecture, language choices, and roadmap are in **[PLAN.md](PLAN.md)**.
Short version: hand-rolled **Rust** core (inverted index + BM25, no search
library), **Go** crawler/gateway, **Python** for extraction/ML, **TypeScript +
CSS** UI. Flux integration is just two HTTP endpoints.

## Quickstart: search the web from Flux

```sh
# 1. Crawl a curated academic corpus into the doc store (edit
#    crawler/seeds/academic.txt to set the scope). Folds in incrementally.
scripts/crawl-academic.sh 1500 150         # [max] [per-host] pages
#    …or a single site:  scripts/crawl.sh https://doc.rust-lang.org/book/ 100

# 2. Build the index and run the service (loads omni.idx if already built)
scripts/serve.sh                       # binds 0.0.0.0:8080

# 3. Make Omni Flux's default engine (writes Flux's search.json), then RESTART Flux
scripts/register-flux.sh
```

In Flux, the omnibox now searches **your** academic index (live typeahead via
`/ac`). Omni is the default; **keyword shortcuts** reach the "necessity" sites a
focused index shouldn't try to crawl:

| Type | Goes to |
|---|---|
| `yt <q>` | YouTube · `gh <q>` GitHub · `ol` Overleaf |
| `scholar <q>` | Google Scholar · `wiki <q>` Wikipedia · `arxiv <q>` arXiv |
| `g`/`ddg`/`b <q>` | Google / DuckDuckGo / Bing |

Flux reads `search.json` **once at startup**, so restart it after registering.

### Keeping the index fresh

```sh
scripts/crawl-academic.sh 3000 200     # re-crawl + incrementally update omni.idx
scripts/update.sh <one-site-url>        # refresh a single site incrementally
# then restart scripts/serve.sh to serve the updated index
```

### Running Flux on Windows with Omni in WSL

Flux's per-tab pages need WebView2, so on a Windows host you run **Flux natively
on Windows** (browsing is broken under WSL/WebKitGTK). Omni can still live in
WSL — two things make it reachable, exactly like a WSL-hosted Ollama:

- **`serve.sh` binds `0.0.0.0:8080`** (not `127.0.0.1`) so the Windows host can
  reach it via WSL localhost forwarding. Flux *connects* to
  `http://localhost:8080` (`OMNI_HOST`, default `localhost:8080`). Keep the
  **bind** address (`OMNI_BIND`, all-interfaces) and the **connect** host
  (`OMNI_HOST`, `localhost`) distinct — `http://0.0.0.0:…` is not a URL Flux can
  load.
- **`register-flux.sh` writes both config dirs.** Tauri's app-config dir is
  OS-specific: Linux `~/.config/dev.flux.browser`, **Windows
  `%APPDATA%\dev.flux.browser`**, macOS `~/Library/Application Support/…`. Run
  inside WSL, the script also writes the Windows `%APPDATA%` copy (via
  `cmd.exe`/`wslpath`) — that's where the Windows build actually reads.

## Status: Phase 20 (index dashboard UI) ✅

A live dashboard at **`GET /dashboard`** in Flux's Royal Velvet × Liquid Glass
theme: glass stat cards (docs, segments, tombstones, embeddings, ANN), a
per-segment bar chart, and the top documents by PageRank — all fed by a new
**`GET /stats`** JSON endpoint and auto-refreshing every 2 s, so a background merge
is visible in real time. UI is vanilla JS/CSS baked into the binary (`ui/`). Omni
still owns only the results + dashboard surfaces; `/` stays a branded search box
(the new-tab start page is Flux's own). Below still holds.

## Status: Phase 26 (corpus ×8 + weight retune) ✅

Expanded the index to **~4575 docs** (deep Stanford SEP, Rust/MDN/Python/Go docs,
Wikipedia hubs) via targeted per-site crawls. Re-ran `scripts/eval.py` (17 queries):
hybrid recall@10 **0.76 → 0.94**, and the optimal semantic weight rises with corpus
size (lexical gets noisier at scale), so the default `sw` was retuned **1.0 → 4.0**.
Visible gains, e.g. *"theory of knowledge and justified belief"* → *Foundationalist
Theories of Epistemic Justification (SEP)*. Below still holds.

## Status: Phases 23–25 (bigger corpus · freshness · eval/tuning · ANN-mmap) ✅

- **Corpus**: widened + deepened academic crawl → `omni.idx` is now **1819 docs**
  (Rust/std, Python, MDN, Stanford, Go, Wikipedia) with Ollama embeddings.
- **Freshness**: the crawler extracts publish dates (`published:` header); the core
  stores them per-doc (`.seg` OSG4) and applies a mild recency boost to dated docs
  (undated pages stay neutral). `/stats` shows the dated count.
- **Eval + tuning**: `scripts/eval.py` (12 known-item queries, MRR@10/recall@10) plus
  a tunable fusion (`/search?sw=…`, `&lex`, `&fmt=json`). Hybrid beats lexical
  **0.60 vs 0.39 MRR**, **0.92 vs 0.67 recall@10**; equal-RRF default confirmed.
- **ANN-mmap** (`--ann-mmap`): optional lazy HNSW that decodes vectors from the
  segments instead of a RAM copy — identical results, negligible latency cost, saves
  memory at scale. RAM stays the default. Below still holds.

## Status: Phase 22 (real semantic embeddings via Ollama) ✅

Hybrid search now uses **real** learned embeddings from a local Ollama model
(`nomic-embed-text`, 768-dim) instead of the offline hash embedder, fused with
BM25. The std-only HTTP client was hardened (HTTP/1.0 + chunked fallback, connect/
read timeouts) and applies nomic's `search_query:` / `search_document:` task
prefixes. `serve.sh` defaults to `--embed ollama` (`OMNI_EMBED=off|hash` to
override); if Ollama is down it falls back to lexical-only. Semantic queries now hit
by meaning — "teaching machines to learn from data" → *Machine Learning* — where
lexical alone returned junk. Below still holds.

## Status: Phase 21 (live ingest + bang shortcuts + essential sites) ✅

Two ways to grow the index without re-crawling, plus first-class necessity sites:
- **`POST /ingest`** adds doc-store records to the *running* index (new segment →
  atomic swap → background merge folds it in); `scripts/ingest.sh` is a helper.
- **`!bang` shortcuts** (`bangs.rs`): `!gh rust`, `!yt lofi`, `!ol thesis` redirect
  (302) to that site's own search — the *necessity* sites you act through, not
  search within. Plain searches stay on the index.
- **`--essential`** indexes curated launch cards (YouTube, GitHub, Overleaf, Stack
  Overflow, Wikipedia, arXiv, Scholar, Wolfram, MDN) so `overleaf`/`github` surface
  a clickable card; the dashboard lists them. Below still holds.

## Status: Phase 19 (live background merge + atomic swap) ✅

The running server can now rebalance its own index **without downtime**. Segments
are held behind `Arc`, so a background thread builds a merged index off a read-only
snapshot (sharing the untouched segments, `index.rs::merged_view`) and swaps it in
atomically (`live.rs`, a hand-rolled `arc-swap` over `RwLock<Arc<Index>>`). Readers
take a cheap snapshot per request and are never blocked by a swap; the merged
layout is persisted so it's durable. Tunable with `--bg-merge-secs N` (0 disables);
a new `GET /stats` reports live index stats (and feeds the dashboard). Verified a
13→4 segment merge live while `/search` kept returning 200s. Below still holds.

## Status: Phase 18 (HNSW refinements: diversity heuristic + persistence) ✅

Vector-index upgrades: neighbor selection now uses the paper's **diversity
heuristic** (keep long-range edges, better recall), and the HNSW graph is
**persisted** as a tiny signature-validated `ann` sidecar (topology only —
vectors are rehydrated from the segments), so it's loaded not rebuilt on startup
unless the index composition changed.

## Status: Phase 17 (HNSW approximate vector index) ✅

Semantic recall now runs on an **HNSW** graph (`hnsw.rs`) instead of scanning every
embedding — a layered proximity graph giving ~O(log N) nearest-neighbor search.
It's built in memory at load over the live embeddings (deterministic, seeded) and
used by hybrid search; below 64 vectors it falls back to exact brute-force cosine
(faster and better at that size). The graph isn't persisted — rebuild-on-load is
cheap and segments stay content-addressed. Below still holds.

## Status: Phase 16 (tiered auto-merge) ✅

Repeated `--update`s each append a small segment; a **tiered (log-size) merge
policy** (`merge.rs`, Lucene/Tantivy style) now auto-combines similarly-sized
segments so the count stays ~`merge_factor·log(N)` and tombstone space is
reclaimed — each doc is rewritten only O(log N) times. It runs after every update
(`--no-merge` to disable, `--merge-factor N` to tune); `--compact` still forces a
full single-segment merge. Search is partition-invariant, so a merge never changes
ranking. Below still holds.

## Status: Phase 15 (segmented index, stage 2c — lazy stored fields) ✅

The index is a **directory of segments** (`omni.idx/`): a `manifest`, one
**immutable content-addressed** `seg-<hash>.seg` per segment, and small mutable
`.del`/`.rank` sidecars. An incremental update **appends a new segment file** and
rewrites only the tiny sidecars — unchanged posting data is never rewritten.
Loading is now **lazy zero-copy**: a segment's `.seg` is memory-mapped (or held)
and its posting lists are **decoded on demand per queried term** (cached); the big
stored fields — body `text` (for snippets) and the `embedding` vector — are also
read from the mapping only when a result needs them, so opening a large index is
cheap and most of its bytes are never touched. Add `--mmap` for the memory-mapped
path. Search merges a global top-K across segments (globally-aggregated BM25F
stats); `--compact` merges segments, drops tombstones, and cleans orphans.
(`--index` names a directory.) Everything below still holds.

## Status: Phase 11 (academic corpus + curation) ✅

A std-only Rust core and a stdlib-only Go crawler feeding it through a plain-text
doc store. The core has a **positional** inverted index with **BM25F** fielded
ranking (title weighted above body), **phrase queries**, **stemming + stop-words**
(Porter), a **proximity bonus**, **PageRank** over the crawl's link graph,
**query-biased highlighted snippets**, and **prefix autocomplete** — persisted to
a **varint/delta-compressed on-disk index** (loadable via **mmap**). Retrieval is
**two-phase**: canonical **Block-Max WAND** builds a fast recall pool, then the
richer signals re-rank it — optionally **hybrid** with semantic embeddings fused
via Reciprocal Rank Fusion. The index updates **incrementally** (`--update`), and
the results page matches Flux's **Royal Velvet × Liquid Glass** theme with live
autocomplete. Plugs into **Flux** as the default engine. See [PLAN.md §8](PLAN.md).

### Keeping the index fresh, semantic search, fast loads

```sh
# Incremental refresh: re-crawl, apply only the changed pages (no full rebuild)
scripts/update.sh https://doc.rust-lang.org/book/ 100

# Semantic / hybrid search: embed docs, fuse lexical + vector results (RRF).
#   offline, zero-dep (fuzzy):   --embed hash
#   real semantics via Ollama:   --embed ollama --embed-model nomic-embed-text
cargo run -- --index ../omni.idx --update ../store --embed ollama

# Memory-mapped load (no full-file heap copy):
cargo run -- --index ../omni.idx --mmap
```

## Run

**Serve search over the bundled local corpus:**

```sh
cd core
cargo run -- --corpus ../corpus --addr 0.0.0.0:8080   # 0.0.0.0 = reachable from the Windows host
# or just use scripts/serve.sh, which binds 0.0.0.0:8080 by default
cargo test             # index/scoring/query/docstore unit tests
```

**Or crawl real sites, then serve the crawled index:**

```sh
cd crawler
go run . -seeds https://example.com -out ../store -max 200   # → ../store/*.doc
cd ../core
cargo run -- --docs ../store                                 # index the crawl
```

**Persist the index so restarts skip the rebuild:**

```sh
# First run builds from --docs and saves to omni.idx:
cargo run -- --docs ../store --index ../omni.idx
# Later runs load the prebuilt index instantly (no --docs needed):
cargo run -- --index ../omni.idx
```

`--docs` and `--corpus` can be combined; the crawler is documented in
[crawler/README.md](crawler/README.md).

### Query syntax

- `web crawler` — free terms (docs matching both, and matching them *near* each
  other, rank higher).
- `"web crawler"` — exact phrase (positional adjacency required).
- Terms are **stemmed** (`crawling` matches `crawler`) and **stop-words**
  (the, of, and…) are ignored.
- Title matches and higher-PageRank pages are boosted automatically; result
  snippets highlight the matching terms.

Then:

```sh
curl "http://127.0.0.1:8080/search?q=inverted+index"   # HTML results page
curl "http://127.0.0.1:8080/ac?q=rust"                 # autocomplete JSON
curl  http://127.0.0.1:8080/health                      # liveness
```

Or open <http://127.0.0.1:8080/search?q=rust> in any browser.

Add documents by dropping `.html` files into `corpus/` and restarting (the index
is in-memory in Phase 1).

## Endpoints (the Flux contract)

| Endpoint | Returns |
|---|---|
| `GET /search?q={query}` | HTML results page (rendered in a Flux webview) |
| `GET /ac?q={query}` | Autocomplete JSON: `["query", ["suggestion", ...]]` |
| `GET /health` | `ok` |

## Hook into Flux

With Omni running, register it from Flux and make it the default (see Flux's
README → Search):

```ts
import { searchAddEngine, searchSetDefault } from "./ipc";

await searchAddEngine({
  id: "omni",
  name: "Omni",
  keyword: "o",
  search_template:  "http://localhost:8080/search?q={query}",
  suggest_template: "http://localhost:8080/ac?q={query}",
});
await searchSetDefault("omni");
```

Now typing in the Flux omnibox searches your own engine. (`o <query>` routes to
Omni explicitly via its keyword.)

## Layout

```
Omni/
├── PLAN.md      # architecture, decisions, roadmap
├── core/        # Rust — inverted index, BM25, query, HTTP server  (Phase 1 ✅)
├── corpus/      # local HTML documents to index
├── crawler/     # Go — fetcher/frontier/robots/rate-limit          (Phase 2 ✅)
├── store/       # crawler output: one *.doc record per page (gitignored)
├── gateway/     # Go — API front door                              (later)
├── pipeline/    # Python — extraction, ranking eval, ML            (Phase 3+)
├── ui/          # CSS + JS results-page frontend (Flux-themed)     (Phase 9 ✅)
└── assets/      # vision screenshot, brand
```
