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
