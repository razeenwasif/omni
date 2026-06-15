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
| `-hosts` | hosts of the seeds | Comma-separated host allowlist (crawl scope) |
| `-out` | `../store` | Doc-store output directory |
| `-max` | `200` | Max pages to fetch (whole crawl) |
| `-per-host` | `0` | Max pages per host (0 = unlimited). Keeps one big domain from dominating. |
| `-workers` | `8` | Concurrent fetch workers |
| `-delay` | `1s` | Default per-host crawl delay (robots `Crawl-delay` overrides) |
| `-ua` | `OmniBot/0.1` | User-Agent (also used to match robots.txt groups) |
| `-timeout` | `15s` | Per-request timeout |

The curated academic seed set lives in `crawler/seeds/academic.txt` — edit it to
widen/narrow the crawl scope (the host allowlist defaults to those seeds' hosts).

## What it does

- **Scope**: only fetches hosts in the allowlist (defaults to the seeds' hosts).
  Off-host links are discovered but not followed.
- **robots.txt**: fetched once per host and cached; longest-match Allow/Disallow
  precedence; honors `Crawl-delay`. Missing/forbidden robots.txt → allow all.
- **Politeness**: a per-host gate enforces the crawl delay; workers run
  concurrently across *different* hosts.
- **Dedupe**: URLs are normalized (lowercased host, no fragment, `/` default
  path) and tracked in a visited set.
- **Output**: one `store/<hash>.doc` per page — see the doc-store format in
  `../PLAN.md` §7.

## Files

| File | Role |
|---|---|
| `main.go` | CLI flags, config, entry point |
| `crawl.go` | Frontier, worker pool, per-host rate gate, fetch loop |
| `robots.go` | robots.txt fetch, parse, and Allow/Disallow/Crawl-delay logic |
| `extract.go` | Title / visible text / outlink extraction from HTML |
| `store.go` | Writing `*.doc` records to the doc store |

## Checks

```sh
go build ./... && go vet ./... && gofmt -l .
```
