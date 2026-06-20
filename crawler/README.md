# Omni crawler

A polite, concurrent web crawler (Go, **stdlib only**) that feeds the Omni
index. Breadth-first from seed URLs, scoped to a curated host allowlist, with
robots.txt respect, per-host rate limiting, and dedupe.

## Run

```sh
cd crawler
# one site
go run . -seeds https://example.com -out ../store -max 200 -workers 8 -delay 1s
# many sites from a curated file, balanced so no domain dominates
go run . -seedfile seeds/academic.txt -out ../store -max 1500 -per-host 150
# RSS/Atom feeds and XML sitemaps can seed fresh URLs
go run . -feeds https://example.com/rss.xml,https://example.com/sitemap.xml -out ../store
```

Then index the result with the Rust core:

```sh
cd ../core && cargo run -- --docs ../store
```

Or use the wrapper that crawls the academic set and folds it into the index
incrementally: `scripts/crawl-academic.sh [max] [per-host] [delay]`.

## Flags

| Flag | Default | Meaning |
|---|---|---|
| `-seeds` | — | Comma-separated seed URLs |
| `-seedfile` | — | File of seed URLs, one per line (`#` comments). Combined with `-seeds`. |
| `-feeds` | — | Comma-separated RSS/Atom feed or XML sitemap URLs. |
| `-feedfile` | — | File of RSS/Atom feed or XML sitemap URLs, one per line (`#` comments). Combined with `-feeds`. |
| `-hosts` | hosts of the seeds and feeds | Comma-separated host allowlist (crawl scope) |
| `-out` | `../store` | Doc-store output directory |
| `-max` | `200` | Max pages to fetch (whole crawl) |
| `-per-host` | `0` | Max pages per host (0 = unlimited). Keeps one big domain from dominating. |
| `-workers` | `8` | Concurrent fetch workers |
| `-delay` | `1s` | Default per-host crawl delay (robots `Crawl-delay` overrides) |
| `-ua` | `OmniBot/0.1` | User-Agent (also used to match robots.txt groups) |
| `-timeout` | `15s` | Per-request timeout |

The curated academic seed set lives in `crawler/seeds/academic.txt` — edit it to
widen/narrow the crawl scope. Without `-hosts`, the host allowlist defaults to the
hosts from seed and feed URLs.

## What it does

- **Scope**: only fetches hosts in the allowlist (defaults to the seeds' hosts).
  Off-host links are discovered but not followed.
- **robots.txt**: fetched once per host and cached; longest-match Allow/Disallow
  precedence; honors `Crawl-delay`. Missing/forbidden robots.txt → allow all.
- **Politeness**: a per-host gate enforces the crawl delay; workers run
  concurrently across *different* hosts.
- **Dedupe**: URLs are normalized (lowercased host, no fragment, `/` default
  path) and tracked in a visited set.
- **Readable extraction**: HTML is cleaned before indexing. The extractor removes
  non-content blocks (`script`, `style`, `template`, SVG/canvas/iframe), prefers
  `<main>`, `<article>`, `role=main`, and content/article wrappers, strips common
  nav/footer/sidebar/cookie/ad/menu boilerplate, keeps headings/code/body text in
  reading order, and decodes named plus numeric HTML entities.
- **Image metadata**: extracts OpenGraph/Twitter image cards plus `<img>`
  `src`, lazy-load attributes, and first `srcset` candidates. Relative image URLs
  are resolved against the page URL and stored with alt/title labels for Omni's
  Images vertical.
- **RSS/sitemap discovery**: optional `-feeds` / `-feedfile` sources can be RSS,
  Atom, XML sitemaps, or sitemap indexes. Discovered URLs are added to the same
  polite frontier as normal links; sitemap indexes recurse to nested sitemaps with
  a small depth cap. Feed dates (`pubDate`, `updated`, `published`, `lastmod`,
  etc.) are normalized to ISO timestamps and used as a `published:` fallback when
  the fetched HTML page has no publish-date metadata.
- **Output**: one `store/<hash>.doc` per page — see the doc-store format in
  `../PLAN.md` §7.

Each `.doc` record may include repeated image headers before the blank line:

```text
image: https://example.com/diagram.png	Search architecture diagram
```

## Files

| File | Role |
|---|---|
| `main.go` | CLI flags, config, entry point |
| `crawl.go` | Frontier, worker pool, per-host rate gate, fetch loop |
| `robots.go` | robots.txt fetch, parse, and Allow/Disallow/Crawl-delay logic |
| `extract.go` | Title / readable text / publish date / outlink / image extraction from HTML |
| `extract_test.go` | Fixture coverage for main-content selection, boilerplate removal, code preservation, image extraction, and entity decoding. |
| `feeds.go` | RSS/Atom/sitemap parsing, nested sitemap discovery, and feed date normalization |
| `feeds_test.go` | Fixture coverage for RSS, Atom, sitemap, sitemap-index, dedupe, and date normalization. |
| `store.go` | Writing `*.doc` records to the doc store |

## Checks

```sh
GOCACHE=/tmp/omni-go-build-cache go test ./...
GOCACHE=/tmp/omni-go-build-cache go vet ./...
gofmt -l .
```
