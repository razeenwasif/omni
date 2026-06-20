# Omni UI

The frontend for Omni's **search results page** — the surface Flux loads in a
webview when you search. It is intentionally **not** a dashboard/new-tab: Flux
already owns that (`~/Flux/apps/shell/src/StartPage.tsx`), so Omni only styles
results and would otherwise duplicate it.

## Files

| File | Role |
|---|---|
| `style.css` | Results-page theme, matching Flux's **Royal Velvet × Liquid Glass** tokens (velvet gradient, glass cards, royal/violet/teal/magenta accents, Inter). |
| `omni.js` | Progressive-enhancement omnibox: live autocomplete dropdown over `/ac` with keyboard nav. The page works without it (plain GET form). |

## Autocomplete

The `/ac` endpoint returns OpenSearch list JSON:

```json
["query", ["suggestion 1", "suggestion 2"]]
```

Suggestions are ranked from three sources:

1. Full queries issued during the current server session, ranked by frequency
   and recency.
2. Indexed title phrases, used before the session has learned enough.
3. Last-token title-word completion, preserving the original behavior for
   partial words like `rust owne`.

Only normal HTML `/search` requests teach the in-memory session history.
Programmatic/eval requests with `fmt=json` and `!bang` redirects are ignored.
The history is deliberately process-local and resets when the Omni server
restarts.

## Query Operators

The results endpoint accepts a small Google-style operator set:

| Operator | Meaning |
|---|---|
| `site:host` / `site:host/path` | Keep results whose normalized URL starts with that host/path. |
| `-term` | Exclude docs containing the analyzed term. |
| `intitle:term` | Require the analyzed term in the document title. |
| `after:YYYY[-MM-DD]` | Keep dated docs published on or after the date. |
| `before:YYYY[-MM-DD]` | Keep dated docs published before the date. |

Recognized operators are removed from the lexical/semantic query text before
embedding and reranking, then applied as filters over the candidate pool. Positive
filter-only searches can return matching docs; purely negative searches do not
seed the entire corpus.

## Search Verticals

The results page renders Google-style vertical tabs for:

| Tab | `type=` value | Behavior |
|---|---|---|
| All | `all` or omitted | Normal ranking. |
| Images | `images` | Currently empty until the crawler persists image metadata. |
| News/Fresh | `fresh` | Keeps pages with a publish date. |
| Docs | `docs` | Keeps documentation/reference/guide/manual-like pages. |
| Code | `code` | Keeps repository/package/API/source-like pages. |
| Sites | `sites` | Keeps curated essential-site launch cards from the bang table. |

Submitting from a non-All tab preserves the active `type=` value. JSON clients can
use the same parameter with `fmt=json`.

## Telemetry

Normal HTML result pages wrap result links with:

```text
/click?q=<query>&u=<target-url>
```

`/click` records an in-memory click signal, rejects unsafe non-http(s) redirect
targets, then redirects to the original URL. The session telemetry is exposed in
`/stats` as search counts, click counts, zero-result rate, average/p95 search
latency, top queries, and top clicked URLs. `/dashboard` renders those fields in
the stat cards and telemetry panels.

Telemetry is local to the running Omni process and resets on restart.
Programmatic/eval searches using `fmt=json` are not recorded.

## How it's served

Both files are baked into the `omni` binary at compile time via `include_str!`
(see `core/src/server.rs`), so the service has **zero runtime asset
dependencies** and always works, while the UI stays editable as plain files.
Served at `/static/style.css` and `/static/omni.js` (versioned + `no-store`).

Editing CSS/JS requires a rebuild (`cargo build`) since they're embedded. The
page is server-rendered (instant paint in the webview); `omni.js` only adds the
live suggestion dropdown.

## Design source of truth

Tokens mirror `~/Flux/apps/shell/src/theme.css`. If Flux's theme changes, update
the `:root` variables in `style.css` to match.

## Note on the stack

`omni.js` is plain JS authored with JSDoc types (so `tsc --checkJs` and editors
type-check it) to avoid a build step. Moving to a Vite + TypeScript bundle later
is a drop-in: keep the markup contract (`#q`, `#suggest`, `#searchform`) and have
the server serve `ui/dist/` instead of the embedded files.
