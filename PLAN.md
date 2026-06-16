# Omni — a personal search engine

A from-scratch web search engine that plugs into the **Flux** browser (`~/Flux`)
as a custom search backend. This document is the build plan: architecture,
language choices, and a phased roadmap.

---

## 0. How Omni connects to Flux (the only contract that matters)

Flux's `flux-search` crate treats engines as **pure data** — a URL template with
`{query}`. Omni does not link into Flux's Rust code. Flux just opens a tab at
your service. The entire integration is two HTTP endpoints:

| Endpoint | Method | Returns | Flux field |
|---|---|---|---|
| `GET /search?q={query}` | HTML | The results page (rendered in a webview) | `search_template` |
| `GET /ac?q={query}` | JSON | Autocomplete suggestions (OpenSearch list format) | `suggest_template` |

Once Omni serves these, register it from Flux (per its README):

```ts
await searchAddEngine({
  id: "omni", name: "Omni", keyword: "o",
  search_template:  "http://localhost:8080/search?q={query}",
  suggest_template: "http://localhost:8080/ac?q={query}",
});
await searchSetDefault("omni");
```

The `/ac` JSON must match the format Flux already consumes from DuckDuckGo
(`type=list`): `["query", ["suggestion 1", "suggestion 2", ...]]`.

**Consequence:** Omni can be built and tested entirely on its own (just hit the
URLs in any browser). Flux integration is the *last* 10 minutes, not the first.

---

## 1. Anatomy of a search engine

Five components, each with different performance characteristics. This is why
"one backend language" is the wrong frame — pick per component.

```
            ┌─────────────┐   seeds/frontier   ┌──────────────┐
            │   Crawler   │ ─────────────────► │  Doc store    │
            │  (fetch +   │                    │ (raw HTML +   │
            │   parse)    │ ◄───────────────── │  metadata)    │
            └─────────────┘     re-crawl       └──────┬───────┘
                                                      │
                                              ┌───────▼────────┐
                                              │    Indexer      │
                                              │ (tokenize →     │
                                              │  inverted index)│
                                              └───────┬────────┘
                                                      │ posting lists
                                              ┌───────▼────────┐
   Flux tab ──HTTP──► ┌──────────────┐        │  Query engine   │
   /search, /ac       │   API / web   │◄──────►│ (retrieve +     │
                      │   gateway     │        │  rank: BM25…)   │
                      └──────────────┘        └────────────────┘
```

1. **Crawler** — fetch pages, respect `robots.txt`, dedupe, follow links.
   Massively I/O-concurrent.
2. **Doc store** — raw + cleaned documents, link graph, crawl metadata.
3. **Indexer** — tokenize/stem, build the **inverted index** (term → posting
   list of doc IDs + positions). Batch, throughput-bound.
4. **Query engine** — the latency-critical heart. Holds posting lists, scores
   candidates (BM25, later: link/freshness signals), returns top-K. **This is
   the "backend" your C++/Java question is really about.**
5. **API/web gateway** — serves `/search` (HTML) and `/ac` (JSON), talks to the
   query engine, renders the UI.

---

## 2. Language per component (recommended)

| Component | Language | Why |
|---|---|---|
| Crawler | **Go** | Goroutines make 10k concurrent fetches trivial; great HTTP/TLS stdlib; one static binary. |
| Content extraction, ranking experiments, any ML (embeddings, query understanding) | **Python** | Ecosystem (trafilatura, scikit-learn, sentence-transformers); fast to prototype. |
| Indexer + **Query engine** (core) | **Java** or **C++** | See §3 — this is the real decision. |
| API gateway / microservice glue | **Go** | Tiny, fast, concurrent; natural front door to the query engine. |
| UI (results page + dashboard) | **TypeScript + CSS** | Per the screenshot. |

The query engine and API can be the *same* process (simplest) or split via gRPC
once you scale. Start as one.

---

## 3. The decision: C++ vs Java for the core index/query engine

This depends entirely on **your primary goal**. Both are legitimate; they lead
to different projects.

### Java — recommended if the goal is "a working search engine, fast"
- **Apache Lucene** gives you a production-grade inverted index, analyzers,
  stemming, BM25, phrase/positional queries, and faceting **out of the box**.
  You'd have ranked results over a real corpus in *days*.
- Modern GCs (ZGC/Shenandoah) keep tail latency in low single-digit ms — a
  non-issue at personal-corpus scale.
- Huge IR ecosystem, easy to hire concepts from (Elasticsearch/Solr are Lucene).
- Cost: you're *using* an index, learning less about how one works internally.

### C++ — recommended if the goal is "learn IR from the metal up"
- You write posting-list encoding (varint/PForDelta), skip lists, BM25 scoring,
  memory-mapped index segments yourself. Maximum control, cache-friendly layout,
  SIMD, no GC. This is how the giants' cores are built.
- Cost: months before you match what Lucene gives day one. Easy to sink time in
  memory bugs instead of search quality.

### The honest third option: Rust
Flux is already Rust, and **Tantivy** is a Lucene-class library in Rust. Choosing
Rust+Tantivy means one toolchain across both projects, Lucene-level features, and
memory safety without GC. If you're enjoying Rust in Flux, this is arguably the
best fit — it just wasn't in your list.

### ✅ DECISION: Rust, hand-rolled (no Tantivy)
Goal is *both* ship a working engine and learn IR deeply, with **learning
prioritized**. So the core index is **hand-written in Rust** — we author the
inverted index, posting lists, BM25, and retrieval ourselves (Tantivy/Lucene are
used only as references to compare against). This learns IR from the metal while
sharing one toolchain with Flux and staying ship-friendly (memory-safe, fast).

Phase 1 is deliberately **zero-dependency / std-only** so it builds offline and
nothing hides the IR logic. Heavier machinery (compression, mmap segments, async
HTTP) is added deliberately in later phases.

---

## 4. The UI (from `assets/` screenshot)

Two distinct surfaces:

- **Dashboard / start page** — central search bar with mic, plus widgets:
  profile, weather, a stocks/markets card, "Recently", an Apps grid, a Music
  player. Frosted-glass cards on a soft gradient. This is the new-tab experience.
- **Results page** — what `/search?q=` renders: ranked list, query echoed in the
  bar, suggestions. (Not in the screenshot; design next.)

Stack: TypeScript + CSS (matches Flux's SolidJS chrome — SolidJS or plain Vite is
a good choice for visual consistency). The glassmorphism aesthetic mirrors Flux's
"Liquid Glass" theme, so the search engine feels native inside the browser.

---

## 5. Phased roadmap

**Phase 1 — End-to-end skeleton (smallest thing that searches)**
- Go API gateway serving `/search` (HTML) + `/ac` (JSON stub).
- Core engine indexes a handful of local HTML files; BM25 top-K.
- Register in Flux, set as default. *Goal: type in Flux → see Omni results.*

**Phase 2 — Real crawler + index**
- Go crawler: frontier, `robots.txt`, dedupe, polite rate-limiting → doc store.
- Python content extraction (boilerplate removal).
- Indexer builds a persistent inverted index over crawled pages.

**Phase 3 — Ranking quality**
- Positional/phrase queries, field boosts (title > body), link-graph signal
  (PageRank-lite), freshness. Python for ranking experiments/eval.

**Phase 4 — The dashboard UI**
- Build the screenshot: search bar, widgets (weather/stocks/music via APIs).
- Real `/ac` autocomplete (prefix trie or finite-state over query log).

**Phase 5 — Smart features (leverages Flux's local-AI angle)**
- Embeddings for semantic retrieval (Python + a vector index).
- Query understanding, instant-answer cards.

---

## 6. Proposed directory layout

```
Omni/
├── PLAN.md
├── assets/                 # vision screenshot, brand
├── crawler/                # Go — fetcher, frontier, robots
├── core/                   # Java (Lucene) | C++ | Rust (Tantivy) — index + query
├── gateway/                # Go — HTTP API: /search, /ac
├── pipeline/               # Python — extraction, ranking eval, ML
├── ui/                     # TypeScript + CSS — dashboard + results
└── docs/                   # ADRs, like Flux's docs/adr
```

---

## 7. Decisions

1. ✅ **Core language**: **Rust, hand-rolled** (§3).
2. ✅ **Crawl scope**: **local-docs-only first**, then a **curated set of
   sites/topics**. Whole-web is out of scope.
3. ✅ **Storage**: **directory of plain-text doc records** (`store/*.doc`), one
   file per crawled page. Chosen over SQLite/RocksDB because it needs **zero
   dependencies on either side** (Go writes, Rust reads), builds offline, is
   human-debuggable, and has **no write contention** (each crawler worker writes
   its own files). Compressed/mmap binary index *segments* are a separate later
   learning project; the doc store stays plain text.

   **Record format** — RFC822-style headers, blank line, then raw body text:
   ```
   url: https://example.com/page
   title: Example Page
   status: 200
   fetched: 2026-06-14T09:30:00Z
   links: https://example.com/a https://example.com/b

   <visible page text, may span many lines, until EOF>
   ```
   Filename is `<sha256(url)[:16]>.doc`. The `links:` header is the **link
   graph**, persisted now so PageRank-lite (Phase 3) needs no re-crawl.

## 8. Phase status

### Phase 1 — DONE ✅
A working end-to-end engine lives in `core/` (Rust, std-only, zero deps):
`index.rs` (inverted index), `score.rs` (BM25), `query.rs` (top-K retrieval),
`corpus.rs` (loads local HTML), `server.rs` (`/search`, `/ac`, `/health`).
Indexes `corpus/*.html`, ranks with BM25, serves the Flux contract from §0.

### Phase 2 — DONE ✅
Real crawl → persistent doc store → index → search.
- **Go crawler** (`crawler/`, stdlib-only): BFS frontier, URL normalization +
  visited dedupe, **robots.txt** fetch/parse/respect (+ Crawl-delay), per-host
  rate limiting, worker pool, curated-host allowlist, stdlib HTML extraction
  (title/text/outlinks). Writes `store/*.doc` records.
- **Doc store** (§7): plain-text records, one file per page, written by Go and
  read by Rust with zero shared dependencies.
- **Rust loader** (`core/src/docstore.rs`): `omni --docs <dir>` indexes the
  crawl output. Verified end-to-end against a local site (robots-disallowed
  pages skipped, off-host links ignored, script/style text excluded).

### Phase 3 — DONE ✅
Ranking quality + a persistent on-disk index.
- **Positional index** (`index.rs`): postings store token positions, not just a
  count. Titles are indexed alongside the body (with a position gap so phrases
  can't cross the field boundary), so title-only terms are findable.
- **Phrase queries** (`query.rs`): `"quoted phrases"` are matched by positional
  adjacency; free terms keep OR-ish accumulation.
- **Title boost** (`query.rs`): a query term in the title adds `2×` its IDF.
- **PageRank-lite** (`pagerank.rs`): iterative PageRank over the `links:` graph
  from the doc store, folded into the score as a `1 + 0.5·(rank/max)` authority
  multiplier so relevance still dominates.
- **On-disk index** (`persist.rs`): a custom binary format with **delta-encoded
  doc ids + positions** and **varint (LEB128)** integers — the classic IR
  posting-list compression lesson. `omni --index <file>` loads a prebuilt index
  (skipping crawl/tokenize) or builds and saves one. Round-trips verified.

Verified end-to-end: crawl → build+save → reload-from-disk → ranked search,
where a hub page linked by every other page ranks top via PageRank + title.

### Phase 4 — Ranking quality, round 2 — DONE ✅
- **Analyzer** (`analyze.rs`): a faithful **Porter stemmer** + a **stop-word**
  list, applied identically at index and query time (so "running" finds "run").
  Kept terms retain their original token position, so phrase queries stay
  accurate across removed stop-words.
- **Query-biased snippets** (`snippet.rs`): the result snippet is the window of
  text where query terms cluster, with matches wrapped in `<mark>` (matched on
  stems, HTML-escaped). The stored body text (capped) lives on the document.
- **Proximity bonus** (`query.rs`): a minimum-cover-span over positions rewards
  docs where the distinct query terms appear close together, scaled by IDF.

Verified live: a search for `engines` highlights `engine`/`Engineers`,
`connect` highlights `connecting`, and adjacent terms outrank scattered ones.

### Phase 5 — Query performance: WAND + two-phase retrieval — DONE ✅
- **WAND dynamic pruning** (`wand.rs`): top-K BM25 retrieval that skips
  documents which provably can't enter the top-K, using per-term score upper
  bounds, a running threshold (the K-th best so far), and pivot-based cursor
  skipping. Returns the *identical* top-K to exhaustive scoring (property-tested
  across queries and K). On a selective query (one common term, one rare) it
  scored **14 of 510 matching postings — ~97% pruned**.
- **Two-phase retrieval** (`query.rs`): phase A uses WAND to build a BM25 recall
  pool (`max(5K, 100)`); phase B re-ranks that pool with the richer signals
  (title boost, proximity, PageRank). Phrase queries fall back to exhaustive
  recall so no phrase match is missed. This "cheap recall → rich re-rank" split
  is how production engines are structured.

### Phase 6 — BM25F + Block-Max WAND — DONE ✅
- **BM25F fielded scoring** (`score.rs`): per-field term frequencies (title vs
  body), each length-normalized against its own field's average, combined
  *before* the tf saturation, with a field weight (title 3× body by default).
  This replaces the earlier query-time title-boost hack with a principled model.
  Data model gained per-field `tf`/`len` (`index.rs`), serialized in index format
  **v2** (`persist.rs`). Verified: a title-only match outranks a body-only match.
- **Block-Max WAND** (`wand.rs`): each posting list is chunked into blocks with a
  stored per-block max impact, so the bound used for a candidate is the max of
  just its block — far tighter than the global max. Before a full evaluation, the
  block-max upper bound at the pivot is checked against θ and the document is
  skipped if it can't qualify. Identical top-K to exhaustive (property-tested);
  on a corpus with a strong head and a long weak tail it did **128 full
  evaluations vs plain WAND's 2000 (~15× less work)**. Plain WAND is retained as
  the reference baseline.

### Phase 7 — Block-level skipping in BMW — DONE ✅
- **Whole-block skipping** (`wand.rs`): when a candidate's block-max bound can't
  beat θ, the retriever jumps to the next block boundary (bounded by where the
  next query term enters) instead of advancing document by document. Over a long
  weak tail this turns an O(documents) walk into an O(blocks) one: a 5000-doc
  tail was traversed in **168 main-loop iterations** (≈ block-rate, not 5000).
- **Correctness fix found while building this:** the block-max bound now sums
  over *all* cursors aligned at the pivot document (ties beyond the pivot rank),
  not just the prefix — otherwise the bound could underestimate and wrongly skip
  a qualifying document. Guarded by the exhaustive-equivalence property tests.
- A `Stats { full_evaluations, iterations }` return exposes both work measures.

### Phase 8 — Canonical BMW pivot + Flux integration — DONE ✅
- **Block-max pivot selection** (`wand.rs`): the pivot is now chosen by
  accumulating each cursor's *current-block* maximum (not the global max). When
  no current-block configuration can beat θ, the shallowest cursor jumps past
  its block and we re-pivot — so long weak tails are skipped at block rate and
  low-then-high posting lists are still handled correctly (new test guards this).
- **Omnibox autocomplete** (`suggest.rs`): prefix completion over the indexed
  title vocabulary — `/ac?q=refe` → `reference`. Replaces the substring stub so
  Flux's omnibox gets real typeahead.
- **Flux integration**: `scripts/crawl.sh` (crawl → store), `scripts/serve.sh`
  (build/load index → serve), `scripts/register-flux.sh` (writes Flux's
  `search.json` at `~/.config/dev.flux.browser/`, Omni as `default_id`). Verified
  end-to-end against an 80-page crawl of the Rust Book: real ranked results and
  live autocomplete served on `localhost:8080`, config validated against Flux's
  `SearchConfig` schema.

### Phase 9 — Results-page UI (Flux-native) — DONE ✅
**Cross-check result:** Flux already owns the dashboard — `StartPage.tsx` is the
screenshot's vision (search hero + glass cards for clock/weather/recent/
shortcuts + flowing wave). So Omni does **not** build a dashboard (that would
duplicate/compete). Omni styles the **results page** Flux loads at `/search`.
- **`ui/style.css`**: results page in Flux's **Royal Velvet × Liquid Glass**
  identity — velvet gradient, glass result cards, royal/violet/teal/magenta
  accents, Inter, pill search bar, highlighted `<mark>` snippets. Tokens mirror
  Flux's `theme.css`.
- **`ui/omni.js`**: live autocomplete dropdown over `/ac` (debounced, keyboard
  nav, click-select), progressive enhancement over the SSR form.
- **Serving**: both baked into the binary via `include_str!` (zero runtime asset
  deps), served at `/static/*` with versioning + `Cache-Control: no-store`.
- **Verified** in a headless browser via computed styles + behavior: velvet
  background (`rgb(7,5,15)`), violet titles, glass cards, pill bar all applied;
  autocomplete opens with real completions (`refe` → `reference`). (The capture
  path here doesn't paint CSS backgrounds, so visual confirmation was by computed
  style, not screenshot — it renders normally in Flux's webview.)

### Phase 10 — Incremental updates · semantic search · mmap — DONE ✅
- **Incremental updates** (`index.rs`, `docstore.rs`): the index is now mutable.
  `omni --update <store>` diffs the doc store against the live index by url +
  content hash and applies just the changes — add new pages, tombstone+re-add
  changed ones, tombstone removed ones — then recomputes PageRank over the live
  graph. Tombstones (a per-doc `deleted` flag + live-only corpus stats) are
  excluded from results/scoring; a full rebuild compacts them. `scripts/update.sh`
  crawls + applies incrementally. Format **v3**.
- **Semantic / hybrid retrieval** (`embed.rs`, `query.rs`): a pluggable
  `Embedder` — an offline deterministic **feature-hash** embedder (zero-dep,
  great for tests) and a hand-rolled **Ollama-style HTTP** client (`POST {model,
  prompt}` → `{embedding}`) for *real* learned semantics. `--embed hash|ollama|
  http://…` embeds docs (stored per-doc, format **v4**); queries fuse the lexical
  (BM25F+BMW) and semantic (brute-force cosine KNN) rankings via **Reciprocal
  Rank Fusion**. Disabled by default → pure lexical. *Honest note:* the hash
  embedder's cosine ≈ term overlap, not learned meaning — point `--embed` at a
  real model for true semantic recall.
- **Memory-mapped loads** (`mmap.rs`): a from-scratch safe `Mmap` RAII wrapper
  over libc `mmap`/`munmap` (no `memmap2` crate). `omni --index <f> --mmap`
  decodes straight from the mapping with no full-file heap copy; verified to
  produce a byte-identical index to the plain read. *Honest note:* this maps the
  file for decode but still builds the full in-memory index — fully lazy per-term
  zero-copy over a kept-open mapping is the segmented-index follow-up.

### Phase 11 — Academic corpus + curation — DONE ✅
Scope the engine to academia (the intended use), with non-crawlable "necessity"
sites reached via shortcuts instead.
- **Crawler** (`crawler/`): `-seedfile` (curated multi-domain seed list) and
  `-per-host` (cap pages per domain so one big site can't dominate). Hosts stay
  scoped to the seed allowlist.
- **`crawler/seeds/academic.txt`**: curated static-HTML references (Wikipedia,
  Stanford Encyclopedia, arXiv, MathWorld, MDN, language docs, proof wikis…).
  `scripts/crawl-academic.sh` crawls the set and incrementally updates the index;
  `omni --no-serve` makes that a build-and-exit step.
- **"Necessity" sites** (YouTube, GitHub, Overleaf, Scholar, Wikipedia, arXiv)
  are *not* crawled — they're dynamic apps. Instead they're **Flux keyword
  shortcuts** (`scripts/register-flux.sh`: `yt`, `gh`, `ol`, `scholar`, `wiki`,
  `arxiv`), so they're one keystroke away while the index stays clean.
- **Verified**: a polite multi-domain crawl (per-host 40) added 217 pages across
  10 academic domains; incremental update → 290 live docs; queries span
  philosophy (Stanford Encyclopedia), CS/ML (arXiv), Python docs, MDN, etc.

### Phase 12 — Segmented index (Stage 1) — DONE ✅
The index is now a **collection of segments** (`segment.rs` = one self-contained
mini-index; `index.rs` = the collection), the Lucene/Tantivy model.
- **Merged search with global stats** (`query.rs`, `wand.rs`): each segment is
  searched independently (block-max WAND), but with **globally aggregated** BM25F
  stats — collection size, per-term document frequency, field averages — passed
  in, so scores are comparable across segments; results merge into one global
  top-K. A global doc is addressed by `(segment, local_id)`. Tested: multi-segment
  search returns *identical* results to the same docs in one segment.
- **Flush-based incremental** (`docstore::update`): new/changed docs land in a
  *fresh* segment (`begin_segment`); existing segments are only tombstoned, never
  rewritten. Deletes are per-segment liveness.
- **Compaction** (`Index::compact`, `omni --compact`): merges all segments into
  one tombstone-free segment, *remapping* postings (not re-tokenizing, so nothing
  is lost to the stored-text cap); ranks and embeddings are preserved. Tested.
- Persist **v5**: embedder descriptor + per-segment blocks. PageRank recomputed
  over the global (segment-major) doc space.

### Phase 13 — Segmented index Stage 2a: per-segment files — DONE ✅
The index is now an on-disk **directory** (`persist.rs`), not a single file:
- `manifest` — format version, embedder config, ordered list of live segment ids.
- `seg-<id>.seg` — one **immutable, content-addressed** segment (`id` = a hash of
  its bytes): stored fields + varint/delta postings. Because the name is the
  content hash, an unchanged segment keeps its file and is **never rewritten** —
  an incremental flush only writes the *new* segment (test:
  `flush_appends_segment_file_without_rewriting_old` asserts the old file's mtime
  is untouched).
- `seg-<id>.del` / `seg-<id>.rank` — small **mutable sidecars** (tombstone bitset,
  PageRank f64s). Deletes and PageRank recompute rewrite only kilobytes, never the
  big posting data. Absent sidecar ⇒ none.
- **Compaction** rewrites one merged segment and `remove_orphans` deletes the
  now-unreferenced files (tested). `--mmap` maps each `.seg` for the decode.

`--index` now names a **directory**; `omni.idx/` was migrated from the old single
file. Format magic `OMNIDIR\x06`.

### Phase 14 — Segmented index Stage 2b: lazy zero-copy postings — DONE ✅
Loading no longer decodes the inverted index into memory.
- **Lazy postings** (`segment.rs`): a loaded segment holds its `.seg` bytes
  (memory-mapped with `--mmap`, or owned) and only a **term→byte-range
  dictionary**; a term's posting list is decoded **on demand** on first query and
  kept in a `RwLock` cache. So opening a big index is cheap and terms never
  queried are never decoded. The `.seg` format gained a per-term blob length so
  the dictionary is built in O(vocab) without touching posting bytes.
- **API**: `Segment::postings()` now returns `Arc<[Posting]>` and `positions()`
  returns owned `Vec<u32>` (the WAND cursors own their lists); stored fields stay
  eager (needed for length norms + snippets). `Mmap` is `unsafe impl Send+Sync`
  (read-only immutable mapping) so the threaded server can share `Arc<Index>`.
- **Resave stays append-only**: a loaded (lazy) segment is content-addressed by
  hashing its held bytes — no re-encode, and its file already exists, so an update
  rewrites only the new segment + sidecars. Tested: lazy load holds bytes, builds
  `term_df` from the dict, decodes a term on demand correctly; mmap load matches
  read load.

### Phase 15 — Segmented index Stage 2c: lazy stored fields — DONE ✅
Loading now touches almost none of a segment's bytes. The two big per-doc fields
— the stored body `text` and the `embedding` vector — are no longer copied into
memory at load; they're read from the (mmapped or owned) mapping **on demand**.
- **`.seg` layout v3 (`OSG3`)**: each doc record now writes its small/eager
  fields first (url, title, field lengths, content hash, `emb_len`) and only then
  the *cold* `text` + raw embedding floats. The loader records each doc's cold
  byte offset and **skips past** the cold region, so opening a segment builds just
  the term dictionary and the small fields.
- **API** (`segment.rs`): `Segment::text(doc)` and `Segment::embedding(doc)`
  return `Cow` — borrowed from memory for a writable segment, decoded from the
  mapping for a lazy one. `Document` keeps `emb_len` so the vector's length is
  known (for scoring/skip) without holding the vector. `compact` materializes the
  cold fields from the source segment before remapping into the merged one;
  `embed_missing` skips lazy segments (their vectors are immutable on disk).
- Verified: a memory-mapped index serves correct **snippets** (text decoded from
  the map) and correct **semantic/hybrid** results (embeddings decoded from the
  map); 46 tests green; `omni.idx` rebuilt to OSG3 (582 docs).

### Phase 16 — Tiered merge policy (auto-compaction) — DONE ✅
Repeated incremental updates each append a new segment; left alone, the segment
count (and thus per-query work + tombstone waste) grows without bound. A
**tiered (log-size) merge policy** now keeps it in check automatically.
- **Policy** (`merge.rs`, Lucene `LogMergePolicy` / Tantivy style): bucket
  segments into size *levels* on a log scale (base = `merge_factor`, default 10);
  once a level holds ≥ `merge_factor` segments, merge the smallest of them. The
  merged result rises to a higher level, so similarly-sized segments combine
  repeatedly (leveled compaction). Net: segment count ~ `merge_factor · log(N)`,
  each doc rewritten only O(log N) times — amortized-linear total merge work.
  `pick(sizes)` makes the decision; segments past `max_merged_docs` are left be.
- **Subset merge** (`index.rs`): `compact` (all segments) and `merge_segments`
  (a chosen subset) now share one `build_merged` core that remaps live postings
  to fresh ids and materializes lazy cold fields; `maybe_merge` loops the policy
  to convergence. `merge_segments` drops the consumed segments and appends the
  merged one, then rebuilds the url→addr map.
- **Wiring** (`main.rs`): after every `--update` the index auto-merges (disable
  with `--no-merge`, tune with `--merge-factor N`), then saves — so the on-disk
  segment count self-bounds. Verified: 6 updates with factor 3 oscillate between
  1–3 segment files instead of growing to 6; partial-merge search is byte-for-byte
  identical to pre-merge (global stats are partition-invariant); 52 tests green.
- **Deferred** (honest scope): execution is on the offline build/update path, not
  a live thread. The server loads the index once into a read-only `Arc<Index>`, so
  a true *background* merge while serving would need an atomic index swap
  (`arc-swap`/`RwLock`) — separate plumbing; the IR substance (the policy + subset
  merge) is here.

### Phase 17 — HNSW approximate vector index — DONE ✅
Semantic recall no longer scans every embedding. The brute-force cosine in
`semantic_ranking` was O(N·dim) per query (fine at hundreds of docs, linear past
that); an **HNSW** graph makes it ~O(log N).
- **Graph** (`hnsw.rs`, Malkov & Yashunin 2016): a layered proximity graph —
  upper layers are sparse long-range "express lanes", layer 0 holds everyone. A
  query greedily descends the upper layers to land in the right neighborhood, then
  beam-searches (`ef`) layer 0. Vectors are L2-**normalized** on insert so cosine
  is a dot product and graph distance is `1 - dot`. Params: `M=16`, `M0=32`,
  `ef_construction=100`. Node levels are drawn from a **seeded** SplitMix64
  (`floor(-ln U / ln M)`), so the graph is deterministic and reproducible.
- **Heaps**: a min-dist candidate frontier (`Reverse<Neighbor>`) and a bounded
  max-dist result set keep the `ef` nearest; `Neighbor` orders floats via
  `total_cmp`. Simple "keep the M closest" neighbor selection (the paper's
  diversity heuristic is a clean follow-up that would lift recall further).
- **Integration**: `Index` holds an `Option<Hnsw>`; `build_ann` constructs it from
  the live embeddings (materializing lazy vectors from the mapping) — but only at
  ≥ 64 vectors, below which exact brute force is both faster and strictly better,
  so `ann` stays `None` and `semantic_ranking` falls back. The graph is built in
  memory at load (`main.rs`, after embeddings settle) and **not persisted**, so
  segments stay content-addressed and no on-disk format changed.
- Verified: synthetic recall@10 ≥ 90% vs exact NN; empty/one-node graphs safe;
  warning-free. End-to-end on the 582-doc hash-embedded index, the graph builds in
  <3 s (process incl. load) and hybrid queries return sensible top hits (e.g.
  *“memory safety”* → the Rustonomicon) over the mmap'd index.

### Phase 18 — HNSW refinements: diversity heuristic + persistence — DONE ✅
Two upgrades to the vector index.
- **Diversity neighbor selection** (`hnsw.rs`, paper Algorithm 4): when wiring a
  node's neighbors (on insert *and* when pruning an over-full list), accept a
  candidate only if it's closer to the node than to every already-chosen neighbor.
  This drops redundant links into the same cluster and keeps long-range edges that
  make the graph more navigable — better recall than plain "M closest". Underfilled
  lists are backfilled with the nearest rejects (`keepPrunedConnections`) so
  connectivity is preserved.
- **Graph persistence** (`ann` sidecar): the graph's **topology** (params, per-node
  address + level + adjacency) is written to `dir/ann`; the vectors are *not*
  stored — they already live in the segments, and `from_bytes` rehydrates them from
  the index on load. So the sidecar is tiny (**36 KB** for 582 docs vs the 13 MB
  segment) and embeddings aren't duplicated. The file is prefixed with a
  **signature** (a hash of the manifest's ordered segment ids + embedder); on load
  the graph is reused only if that signature still matches — a merge, update, or
  embedder change shifts it and forces a clean rebuild. A missing rehydrated vector
  also invalidates it.
- **Wiring** (`main.rs`): startup now does *load-or-build* — reuse the persisted
  graph when valid, else (re)build and save it. Verified end-to-end: build saves
  the sidecar; plain and mmap reloads **load** it (no rebuild); `--compact` that's a
  content no-op keeps it; an `--update` that adds a doc shifts the signature and
  triggers a **rebuild**, which is then persisted. Serialize→rehydrate→search is
  byte-for-byte identical; stale/garbage bytes return `None` not a panic. 55 tests
  green, warning-free.
- **Deferred**: ANN over mmap'd vectors without a RAM copy (vectors are still held
  in memory for distance computation).

### Phase 19 — Live background merge with atomic index swap — DONE ✅
The tiered merge now also runs **while the server is serving**, with zero-downtime
swaps — the running index rebalances itself instead of only the build/update path.
- **Shared segments** (`index.rs`): `Index` now holds `Vec<Arc<Segment>>`, so an
  immutable segment can belong to two indexes at once. `merged_view(sel)` builds a
  *new* index that merges the selected segments and **shares** (Arc-clones) the
  untouched ones — no copy — then rebuilds the url map and ANN over the new layout.
  The mutating build paths use `Arc::get_mut` (segments are uniquely owned before
  they're shared).
- **Atomic swap** (`live.rs`): `LiveIndex` wraps `RwLock<Arc<Index>>` — a
  hand-rolled `arc-swap`. Readers take a cheap `Arc` **snapshot** (lock held only
  to clone the pointer, never for the query); the merger swaps the pointer in one
  write. In-flight readers keep their old snapshot until they drop it. (Unlinking a
  still-mmapped orphan segment is safe on Linux, so the on-disk cleanup can race
  the swap harmlessly.)
- **Background merger** (`live.rs`): a thread wakes every `--bg-merge-secs` (default
  30, `0` disables — independent of the offline `--no-merge`), and if a size tier is
  over-full does one merge step off a snapshot, **persists** it (durable), and swaps
  it in. A balanced index just sleeps.
- **`/stats` endpoint** (`server.rs`): live JSON (segments, live/total docs,
  tombstones, embedded/ANN flags) — feeds the dashboard and lets you watch a merge.
- Verified end-to-end: a 13-segment index served with `--bg-merge-secs 1
  --merge-factor 4` shrank **13 → 10 → 7 → 4** segments live (visible in `/stats`)
  while a concurrent `/search` flood returned **HTTP 200 throughout** — no dropped
  requests, no lost docs, and the merged layout persisted to disk. 57 tests green
  (incl. snapshot-survives-swap and shared-segment merged_view), warning-free.
- **Deferred**: a live *ingest* endpoint (so segments also accumulate at runtime,
  not just from a `--no-merge` build) — the swap machinery is ready for it.

### Phase 20 — Index dashboard UI — DONE ✅
A glass-card dashboard for index health, in Flux's Royal Velvet × Liquid Glass
identity (cross-checked against `~/Flux`'s start page + `theme.css`).
- **`GET /stats`** (`server.rs`): live JSON — live/total docs, tombstones, segment
  count, embedder kind+dim, ANN vector count, avg field lengths, **per-segment
  sizes**, and **top-8 documents by PageRank**.
- **`GET /dashboard`** (`server.rs` + `ui/dashboard.js`, baked via `include_str!`):
  a static shell that fetches `/stats` and renders stat cards, a per-segment bar
  chart (live vs. total), and the authority list — **auto-refreshing every 2 s**, so
  a background merge is visible in real time (segment bars collapsing live). Vanilla
  JS, no build step; styling added to `ui/style.css` reusing the shared Flux tokens.
- **Polish**: a gradient ✦ SVG favicon (`/favicon.svg`) on both pages; a
  `dashboard ⇄ search` nav link.
- Verified in a real browser (Playwright over the WSL→Windows localhost forward):
  page renders with the velvet body, `blur(40px)` glass cards, gradient-clipped
  stat numbers, royal→magenta segment bars, and the pulsing liveness dot; cards
  populate from live `/stats` (582 docs · embeddings on · 582 ANN vectors · arXiv &
  Wikipedia topping authority); zero console errors. 57 tests green, warning-free.
- Scope: Omni owns the *results* + *dashboard* surfaces; the new-tab/start page is
  Flux's own `StartPage`, so Omni's `/` stays a branded search box, not a competing
  start page.

### Phase 21 — Live ingest + bang shortcuts + essential sites — DONE ✅
Two ways to grow Omni without re-crawling, plus first-class "necessity" sites.
- **Live ingest** (`POST /ingest`, finishes Phase 19's deferred item): the server
  reads a request body of doc-store records (separated by a line `---`), stages the
  **new** urls as a fresh segment (embedding them if the index is embedded), and
  swaps the grown index in atomically via the same `commit` path the background
  merger uses — then the merger folds the segment into the tiers. Already-indexed
  urls are skipped (replace via offline `--update`). Required teaching the minimal
  server to read headers + a `Content-Length` body, and a `Reply` enum so a route
  can return a redirect as well as a page. (`index.rs::with_appended` shares the
  existing segments via `Arc`; PageRank for ingested docs is 0 until a rebuild.)
- **Bang shortcuts** (`bangs.rs`): the *necessity* sites you act through, not search
  within. A query whose first token is `!<key>` (`!gh rust async`, `!yt lofi`,
  `!ol thesis`) returns a **302** to that site's own search (url-encoded); a bare
  `!yt` → its home. The `!` prefix keeps ordinary searches (`github actions`) on the
  index. Resolved at the Omni layer, so they work independent of Flux's omnibox
  keyword shortcuts (which stay too — same sites, different trigger).
- **Essential launch cards** (`--essential`): the same `bangs::SITES` table (one
  source of truth) is also indexed as small searchable documents, so a plain search
  for `overleaf`/`github` surfaces a clickable card. Curated set: YouTube, GitHub,
  Overleaf, Stack Overflow, Wikipedia, arXiv, Google Scholar, Wolfram Alpha, MDN.
- **Dashboard**: an "Essential sites" panel lists the shortcuts (teal `!bang`
  chips); the landing page hints `!yt !gh !ol`.
- **Scripts**: `scripts/ingest.sh` (POST a file/stdin to a running Omni);
  `serve.sh` now passes `--essential`.
- Verified end-to-end: all bangs 302 to the right encoded URLs; `overleaf`/`github`/
  `youtube` searches return their cards; `POST /ingest` of a Raft paper → `{added:1}`,
  `live_docs 591→592`, and on a lexical index it's the **#1** hit for "raft
  consensus"; re-ingesting the same url → `{added:0,skipped:1}`; dashboard panel
  renders 9 shortcut cards (Playwright), zero console errors. 62 tests green
  (5 new bang tests), warning-free; Go checks clean.
- **Still deferred** (unrelated to this goal): ANN over mmap'd vectors without the
  in-RAM copy; freshness — the crawl timestamp isn't a publish date. To add more
  *academic* corpus, the Go crawler + `scripts/crawl-academic.sh` remain the path.

### Phase 22 — Real semantic embeddings (Ollama) — DONE ✅
The hash embedder exercised the pipeline but isn't learned semantics (cosine ≈ term
overlap, so it added noise). Omni now uses a **local Ollama model** for *real*
embeddings, fused with BM25 (hybrid).
- **Hardened HTTP client** (`embed.rs`): the std-only client now speaks **HTTP/1.0**
  so Ollama's response is close-delimited (under HTTP/1.1 it replies
  `Transfer-Encoding: chunked`, whose chunk-size lines would corrupt the float array
  mid-parse); a chunked body is still decoded as a fallback (`dechunk`). Added
  connect (2 s) + read (60 s, for cold model load) + write timeouts so an
  unreachable model degrades to lexical fast instead of stalling a build or query.
- **Task prefixes**: `nomic-embed-text` is instruction-tuned and needs asymmetric
  prefixes, so documents are embedded as `search_document: …` (`embed_doc`) and
  queries as `search_query: …` (`embed_query`). Other models / the hash embedder get
  the text unchanged.
- **Build UX**: `embed_missing` shows progress for large batches (a real embedder is
  hundreds of model calls vs the instant hash). dim is discovered from the first
  vector (768 for nomic), and switching embedder kind clears + re-embeds; the
  embedder config + ANN signature persist, so a plain reload keeps hybrid on.
- **Wiring**: `serve.sh` defaults to `--embed ollama` (`OMNI_EMBED=off|hash` to
  override); the canonical `omni.idx` was rebuilt with 768-dim nomic embeddings
  (591 docs in ~22 s, 36 KB ANN sidecar). If Ollama is down, embedding and queries
  fall back to lexical-only gracefully.
- Verified against a live Ollama (`nomic-embed-text`): unambiguous semantic queries
  now hit by *meaning*, not term overlap — "teaching machines to learn from data" →
  **Machine Learning**, "avoiding memory corruption in systems code" → **Vec /
  std::vec** (no shared terms), where lexical-only returned *Bioinformatics* /
  *Portal:Current events*. (Abstract/ambiguous queries on this small philosophy-heavy
  corpus stay soft — a direct cosine check confirmed that's the embedder's genuine
  ranking, not a fusion bug; sharper results need a larger/better-matched corpus,
  not code.) 63 tests green (incl. a chunked-decode test), warning-free; Go clean.

### Phase 23 — Bigger corpus + freshness — DONE ✅
Grew the index and gave it a sense of time.
- **Corpus** (`crawler/`, `seeds/academic.txt`): widened the seeds and ran a broad
  crawl plus a targeted deepening crawl of the rich doc sites (Rust book + std,
  Python, MDN, Stanford, Go). Store **582 → ~1810 docs**; `omni.idx` rebuilt to
  **1819 docs** (incl. essential cards) with 768-dim Ollama embeddings in ~75 s.
  The multi-host BFS favors Wikipedia, so the per-site targeted crawl is how you
  deepen technical docs.
- **Freshness** (crawler + Rust): the crawler now extracts a publish date
  (`<meta article:published_time>`, JSON-LD `datePublished`, `<time datetime>`) into
  a `published:` header. The Rust side parses it (`docstore::parse_published`, a
  no-chrono `days_from_civil`) into a per-doc unix time on `Document` (`.seg` format
  **OSG4**), and `query.rs` applies a **mild recency boost** (`1 + 0.10·exp(−age/
  365d)`) only to dated docs — undated reference pages stay neutral, never penalized.
  `/stats` reports the `dated` count (405 of 1819) — which also verifies the date
  round-trips through OSG4.

### Phase 24 — Eval harness + hybrid-ranking tuning — DONE ✅
Made ranking quality measurable, then tuned on the data.
- **Tunable fusion** (`query.rs`): `rrf_weighted` + `SearchOpts.semantic_weight`;
  `/search` accepts `sw=<f64>` (semantic weight), `lex` (lexical-only), and
  `fmt=json` (ranked JSON for clients/eval).
- **Eval** (`scripts/eval.py`): 12 known-item queries (verified targets across
  Rust/MDN/Python/Stanford), measuring MRR@10 + recall@10, sweeping `sw`.
- **Result**: hybrid **crushes** lexical — MRR **0.39 → 0.60**, recall@10
  **0.67 → 0.92**. The sweet spot is `sw ∈ [0.5, 1.0]` (mid-weights 1.5–3.0 were
  slightly worse); the standard equal-RRF default (1.0) is confirmed near-optimal, so
  it's kept and documented. Quantitative proof the real embeddings earn their keep.

### Phase 25 — ANN over mmap'd vectors (optional) — DONE ✅
An opt-in memory mode for large corpora.
- **Lazy HNSW** (`hnsw.rs`): with `keep_ram = false` the graph drops its RAM vector
  copy after construction and decodes each vector from the segments on demand via a
  `Fetch` closure at search time (the topology sidecar is mode-agnostic). Threaded an
  optional fetch through `dist`/`search_layer`/`search`; `Index.ann_lazy` +
  `--ann-mmap` select it; `query.rs` supplies the closure.
- **Honest benchmark**: lazy vs RAM on the 1819-doc index gave **identical** results
  (MRR 0.602 / recall 0.92) with **no meaningful latency penalty** — the Ollama query
  embedding dominates, so the per-distance decode is lost in the noise — saving ~5.3 MB
  here (linear: ~3 GB at 1M docs). So the feared pessimization is negligible in
  practice; RAM stays the default, lazy is there for memory-constrained scale.
- 65 tests green (incl. lazy==RAM equivalence), warning-free; Go clean.

### Phase 26 — Corpus expansion ×8 + weight retune — DONE ✅
Grew the index again and let the eval re-pick the fusion weight.
- **Corpus**: widened `seeds/academic.txt` and ran targeted per-site crawls
  (Stanford SEP deep, Rust book/reference/by-example, MDN CSS/Web-API, Python
  library, Go spec, Wikipedia hubs). Store **582 → 4566 docs** (~7.8×);
  `omni.idx` rebuilt to **4575 docs**, 4572 embedded (768-dim Ollama) in ~3.4 min,
  1413 dated. Per-site crawls remain the way to beat the Wikipedia-dominated BFS.
- **Re-tuned fusion**: re-ran `scripts/eval.py` (now 17 queries — natural-language
  *and* keyword/navigational). Finding: **the optimal semantic weight grows with
  corpus size** — as the index grows, lexical gets noisier (more keyword
  collisions), so semantic should weigh more. recall@10 0.76 (lexical) → **0.94**
  (hybrid), with the knee at **`sw≈4`** (vs ~1.0 at 1.8k docs). Bumped
  `DEFAULT_SEMANTIC_WEIGHT` 1.0 → **4.0** (kept hybrid 4:1 so keyword queries still
  land; verified default == sw=4 live).
- Visible quality jump on the bigger corpus: *"rules for who owns a value in rust"*
  → **std::ptr** (was *African Ethics*); *"theory of knowledge and justified belief"*
  → **Foundationalist Theories of Epistemic Justification (SEP)**; *"css flexbox
  layout"* → **CSS layout (MDN)**. 65 tests green, warning-free.

The corpus (`store/`) and index (`omni.idx/`) are gitignored — regenerate with
`scripts/crawl-academic.sh` + a rebuild. The eval harness means the next corpus
bump can be re-tuned on numbers, not vibes.

### Phase 27 — Corpus ×13 (deeper crawl) — DONE ✅
Pushed the corpus further with deeper per-site crawls (Stanford SEP to ~1800
entries, MDN Web/API+CSS+JS+HTTP, Rust nomicon + by-example + reference, Python
library, math wikis: ProofWiki/MathWorld/nLab/Wikibooks/Wikiversity). Store
**4566 → 7823 docs** (~13× the original 582); `omni.idx` rebuilt to **7832 docs**,
7827 embedded (768-dim Ollama) in ~5 min, 2448 dated.
- **Re-eval**: the `scripts/eval.py` sweep **flattened** — MRR ~0.65 across `sw`
  1–20, so the `sw=4.0` default held (no retune). recall@10 on the *exact* known-item
  targets dipped (0.94 → ~0.82) — expected: among 7.8k docs there are now more
  genuinely-relevant competitors (e.g. MDN's *Using promises* guide outranking the
  bare `Promise` reference), which the strict single-target proxy reads as a miss.
- **Qualitative quality improved**, confirming the dip isn't a regression: *"how a
  program manages memory safely"* → **the Rustonomicon**, *"rules for who owns a
  value in rust"* → **Rust By Example: Scoping rules** (both from the new crawls);
  epistemology / ML / CSS-flexbox all still land. 65 tests green.

### Phase 28 — Graded-relevance eval (nDCG@10) + retune — DONE ✅
Upgraded the eval from a single exact target per query to **graded relevance**, so
further corpus gains are measurable (the old metric dipped as the corpus grew even
when quality rose).
- **`scripts/eval.py`**: each of 17 queries now has a *set* of relevant URL patterns
  with grades (3 = canonical, 2 = closely related, 1 = tangential; `$` = exact-page).
  Metric is **nDCG@10** with the ideal DCG built from a **pooled** deep (k=50) fetch
  — so patterns whose docs aren't in the corpus don't deflate the score. Also reports
  **success@10** (a grade ≥ 2 doc in the top 10). Added a `k=` param to `/search`'s
  JSON output to fetch the pool.
- **Finding — it flips the earlier conclusion**: graded nDCG@10 peaks at `sw≈1–2`
  (0.68 vs 0.51 lexical-only), and success@10 reaches **1.00 at sw=2** (every query
  surfaces a strongly-relevant doc in the top 10). The single-exact-target metric had
  over-favored high weights by only rewarding the one canonical page; crediting the
  whole relevant *family* shows a balanced 2:1 hybrid is best.
- **Re-tuned** `DEFAULT_SEMANTIC_WEIGHT` 4.0 → **2.0** (peak success@10, near-peak
  nDCG). Verified live (default == sw=2): *"rust ownership borrowing and lifetimes"*
  → *Validating References with Lifetimes*; *"theory of knowledge and justified
  belief"* → *The Value of Knowledge (SEP)*; *"fetch api http requests"* → *Using the
  Fetch API (MDN)*. 65 tests green, warning-free.

### Phase 29 — Corpus ×20 + broadened ruler — DONE ✅
Grew the corpus again and made the eval sturdier, then let it judge.
- **Corpus**: added broad Wikipedia (science/math/CS/philosophy hubs), **C++**
  reference (cppreference), Python PEPs + reference/howto, Rust cargo/nomicon, and
  Go stdlib. Store **7823 → 12028 docs** (~20× the original 582); `omni.idx` rebuilt
  to **12037 docs**, 12029 embedded (768-dim Ollama) in ~9 min, 4479 dated. New
  seeds added to `seeds/academic.txt` (cppreference, go pkg, python peps/reference,
  rust cargo).
- **Sturdier ruler**: `scripts/eval.py` grown from 17 → **32 graded queries** across
  C++, Go stdlib, Python, Rust, MDN, Wikipedia science/math, and SEP philosophy
  (all grade-3 anchors verified present).
- **Result — the broadened ruler validates the `sw=2.0` default**: nDCG@10 is a flat
  plateau over `sw∈[2,4]` (0.666–0.675, vs 0.477 lexical-only), peaking at sw=3.0
  but within 32-query noise; success@10 peaks at **0.97 at sw=2** (31/32 queries
  surface a grade-≥2 doc in the top 10). So no retune — the default holds, now
  confirmed on a much stronger eval.
- New domains all land: *"c++ vector dynamic array"* → **std::vector (cppreference)**,
  *"python pep 8 style"* → **PEP 8**, *"theory of relativity spacetime"* →
  **Spacetime**, *"philosophy of mind consciousness"* → **The Neuroscience of
  Consciousness (SEP)**. 65 tests green.

### Phase 30 — Live ingest from Flux — DONE ✅
The index now grows from what the user actually reads in Flux, not just crawls.
- **Omni side** (`json.rs`, `server.rs`): `POST /ingest` now also accepts **JSON**
  — `{url,title,text}` or an array — the natural payload for a browser, alongside
  the existing doc-store text format (sniffed by the leading `{`/`[`). Added a tiny
  hand-rolled JSON reader (objects/arrays, full string unescaping incl. `\u`
  surrogate pairs, skips non-string fields) to keep the no-deps ethos. New urls are
  staged → embedded → atomically swapped in (the live-merge machinery from Phase 19);
  revisits are skipped. 68 tests green (3 new JSON tests).
- **Flux side** (`~/Flux`, separate repo — validated with `cargo check` +
  shell `tsc`, full Tauri build is Windows-only): Flux already captures each page's
  visible text via `dom_publish`; that hook now also calls `omni::maybe_auto_ingest`,
  which POSTs `{url,title,text}` to Omni's `/ingest`. Two paths, both reusing the
  existing flux-core↔Omni `ureq` bridge + `omni_base()`:
    * **explicit** `omni_ingest_active` command — "save this page to Omni",
    * **opt-in auto** (`omni_ingest_set_auto`, off by default, seeded from
      `FLUX_OMNI_INGEST=1`) — index every substantial page (≥500 chars, http(s))
      on load. A toggle was added to Flux's `flux://omni` dashboard.
  Privacy-first: nothing is ingested unless the user enables auto or clicks save.
- Verified the Omni side end-to-end: JSON single + array ingest add docs that become
  searchable; re-ingest is skipped. The Flux side compiles/typechecks; the user
  builds it on Windows to exercise the live loop.

### Phase 31 — Cross-encoder reranking (opt-in) + an honest negative result ✅
Added a second-stage **cross-encoder reranker** (`rerank.rs`): the top hybrid
candidates are re-scored by an LLM that reads the query and each passage *together*
(RankGPT-style listwise — Ollama has no `/api/rerank`). One `/api/chat` call returns
the candidates in relevance order; lenient parsing + graceful fallback mean it can
only help, never break, a query. Fed the **query-biased passage** (`snippet::plain`),
not lead boilerplate, so the model sees the relevant content. Opt-in via `&rerank=1`
(+ `&rr_model=`); refactored a shared `embed::http_post_json`; added `k=` to the JSON
search and `extract_string_field` to the JSON reader.
- **The harness then earned its keep — and said no.** Measured with graded nDCG@10
  on the 12k-doc corpus: a small reranker (phi4-mini 3.8B) *hurt* badly
  (**0.666 → 0.34**); mid and large instruct models were exactly **neutral** —
  gemma4 **e4b 0.666** and **12b 0.666** (both verified over 32 queries), they just
  echo the already-good hybrid order. (success@10 is 0.97 — the hybrid already puts
  a strong doc in the top 10 for 31/32 queries, so there's little for a reranker to
  fix.) The eval-tuned hybrid is strong enough that a *locally-runnable generative*
  reranker adds nothing; a real gain needs a **distilled cross-encoder** (e.g.
  bge-reranker via ONNX) — a dependency Omni doesn't carry.
- *(Correction: an earlier note here blamed VRAM for a crashed 12b eval run — that
  was wrong. The box is a 24 GB RTX 4090; 12b+nomic sit at ~16 GB with headroom, no
  OOM in the kernel log. That run was **terminated** (timeout/stray process), not
  starved; the 12b result above is from a clean re-run.)*
- **Decision**: ship it as **pluggable, opt-in, off-by-default** scaffolding (zero
  VRAM/latency cost unless requested; default model set to the lightest *neutral*
  one, `gemma4:e4b-it-qat`). When a dedicated reranker is available (ONNX, a much
  stronger model, or a future Ollama rerank endpoint) it's a drop-in. Stopping a
  default-on feature that would have added latency + VRAM for no gain is the eval
  harness doing exactly what it's for. 70 tests green, warning-free.

### Phase 32 — Passage-level indexing (the dense-retrieval unit) — DONE ✅
Whole-doc embeddings average a long page into one vector and, worse, **overflow
`nomic-embed-text`'s 2048-token context** — a naive whole-doc rebuild left **21 %
of docs (2520/12027) with no embedding at all**. The fix is to retrieve over
**passages**: chunk each doc (`passages.rs`, ~150 words × ≤6 windows), embed each,
and let a doc's semantic score be its **best passage** (max-pool). This is also the
unit a reranker and a future RAG answer mode operate on.
- **Storage (segment format `OSG4`→`OSG5`)**: each `Document` now stores a flat
  blob of `n_passages × dim` f32 plus `n_passages`; `segment.passages()` slices it
  back. `HNSW` nodes carry `(doc Addr, passage_idx)` (`OANN2`→`OANN3`); search
  over-fetches passages then dedups to **best-passage-first** docs. Brute-force and
  lazy/mmap paths max-pool too. `STORED_TEXT_CAP` 4000→8000.
- **Parallel embedder (~12×)**: `embed_missing` now pre-chunks under an immutable
  borrow, embeds passages across **8 scoped threads**, and writes back by index
  (deterministic). Ollama keeps one resident 0.3 GB nomic model and serves the
  concurrent requests from it, so wall-time went **~4 → ~50 docs/sec** with **no**
  extra VRAM (~2.3 GB total on the 4090). Chunk size is env-tunable for experiments
  (`OMNI_WORDS_PER`, `OMNI_MAX_PASSAGES`).
- **A methodology catch.** `eval.py` normalizes nDCG against a *per-index self-pool*
  (its own deep fetch), so its nDCG is **not comparable across two different
  indexes** — a stronger index builds a deeper pool, a bigger ideal DCG, and an
  identical lexical run then scores *lower*. (Symptom: lexical-only read 0.466 on the
  passage index vs 0.630 on a whole-doc one, despite BM25 being identical.) Added
  `scripts/compare.py`: a **shared pool** (union of every host's deep fetch) + raw
  **DCG@10** + **success@10**, all comparable across servers.
- **Result (shared pool, sw=2.0, 32 queries)** — passages win on every comparable
  metric:

  | index | DCG@10 | nDCG@10* | success@10 |
  |---|---|---|---|
  | **passage 150×6** | **8.80** | **0.630** | **0.97** |
  | whole-doc 900×1 (same text budget) | 8.73 | 0.612 | 0.91 |
  | whole-doc uncapped (naive) | 7.77 | 0.535 | 0.84 |

  vs the clean same-budget whole-doc control: **+2.9 % nDCG\*, +6 pp success@10**;
  vs naive whole-doc: **+17.8 % / +13 pp**. A deeper probe (150×10) reached perfect
  **success@10 = 1.00** but *lowered* DCG/nDCG (more passages → more max-pool false
  positives demote the canonical doc) — so **150×6 is the shipped default**: best
  ranking quality, near-perfect recall.
- **Cost**: ~64.9k passage vectors vs 12k whole-doc → the on-disk index grows
  ~212 MB→406 MB and the **first** serve start rebuilds the HNSW once (~3 min for
  65k vectors); thereafter the sidecar matches (essential cards are baked in) and
  load is instant. **72 tests green**, warning-free.

### Phase 33 — RAG answer mode + reranker on real passages — DONE ✅
Passages aren't just a ranking unit — they're the thing a reader actually wants and
the thing a reranker should judge. This phase spends them on both.
- **Extractive direct answer.** Each search now finds the top hit's *best-matching
  passage* (argmax query↔passage cosine over the doc's stored passage vectors),
  re-chunks the stored text to recover that passage verbatim, and returns it as a
  **direct answer** — no LLM generation, so it's effectively free and adds zero VRAM.
  Gated by a confidence floor (`ANSWER_MIN_SIM = 0.6`) so a weak match shows nothing
  rather than a wrong paragraph. `Hit::answer` is set only on the top hit; the HTML
  results page renders it as a featured card (violet glass), and `&answer=1` adds it
  to the JSON. Spot-checks: *“python asyncio event loop”* → the lead definition
  (“The event loop is the core of every asyncio application…”); *“rust ownership and
  borrowing”* → the borrowing/scope passage.
- **Reranker fed real passages.** `rerank_pool` now hands the LLM each candidate's
  semantically-best passage (word-capped) instead of a keyword lead snippet — the
  actual dense unit, so the cross-encoder judges the relevant content. (Still opt-in
  and measured-neutral on this corpus; this just sharpens what it sees.)
- **One query embedding, shared.** `search_with` embeds the query **once** and
  threads the vector through hybrid fusion, the reranker, and the answer step
  (`semantic_ranking` now takes the vector rather than re-embedding) — answer mode
  costs only a cosine scan of one doc's passages on top.
- **Entity cleanup.** `passage_text` decodes the HTML entities that survive into
  stored crawl text (numeric + common named) so the answer reads as prose and the
  reranker sees clean text; decode-then-`html_escape` stays XSS-safe. **73 tests
  green**, warning-free.

### Phase 34 — General sites + generative RAG answer mode — DONE ✅
Two pushes: broaden the curated launch cards beyond reference sites, and add a true
generative answer on top of the extractive one.
- **Curated sites 9 → 34.** Added the everyday destinations alongside the reference
  set: LinkedIn, Medium, Kaggle, Reddit, Hacker News, Dev.to, Hugging Face, ChatGPT,
  Claude, npm, crates.io, PyPI, Google, DuckDuckGo, Gmail, Maps, Drive, X, Amazon,
  Netflix, Spotify, IMDb, Twitch, Notion, Figma. Each gets bang keys (`!li`, `!kg`,
  `!hf`, …), a real search-URL template, and an indexed launch card, all from the one
  `SITES` table. New test asserts **every bang key is unique** (a dup would silently
  shadow a site as the table grows).
- **Generative RAG (`rag.rs`, `GET /answer`).** The heavier sibling of the extractive
  answer: `query::answer_context` pulls the best passage from each of the top-5 hits,
  and a local LLM composes a 2-5 sentence answer **grounded only in those passages**,
  citing them `[n]`. Returns `{answer, sources[]}`. It's a model call per request
  (seconds + VRAM), so it's a **separate opt-in endpoint**, never auto-run from
  `/search`. Default model `gemma4:12b-it-qat` (`&model=` to override); graceful
  `null` on any failure. Verified end-to-end: *“what is an inverted index and how
  does BM25 rank documents”* → a correct, cited two-sentence answer grounded in the
  Inverted-Index source.
- **`think:false` — a real bug, and a correction.** gemma `*-it-qat` are *reasoning*
  models: via Ollama `/api/chat` they spend the token budget in a `thinking` field
  and return an **empty `content`**. The first `/answer` call returned `null` for
  exactly this reason; adding `"think":false` fixed it. The same flag was missing
  from the **reranker** — meaning the earlier "gemma 12b reranker ≈ neutral (0.666)"
  result was almost certainly the reranker silently **falling back to the hybrid
  order** (empty reply → `parse_order` → keep order), not a real measurement. Both
  call sites now send `think:false`; the reranker's effect is worth re-measuring.
- **77 tests green**, warning-free. (Adding sites means the next `serve.sh` start
  bakes the new cards in and rebuilds the HNSW once — the usual one-time cost.)
