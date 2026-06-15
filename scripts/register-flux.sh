#!/usr/bin/env bash
# Register Omni as Flux's default search engine by writing Flux's search.json.
# Flux (identifier dev.flux.browser) reads it from its OS app-config dir at
# startup — so RESTART Flux after running this.
#
# IMPORTANT — config-dir location depends on where Flux RUNS, not where this
# script runs. Flux's per-tab webviews need WebView2/WKWebView, so on a Windows
# host you run Flux natively on Windows (browsing is broken under WSL/WebKitGTK).
# Tauri's app-config dir then differs:
#   Linux  : $XDG_CONFIG_HOME/dev.flux.browser              (~/.config/...)
#   Windows: %APPDATA%\dev.flux.browser                     (AppData\Roaming\...)
#   macOS  : ~/Library/Application Support/dev.flux.browser
# This script writes the Linux path, AND — when run inside WSL — also the
# Windows %APPDATA% path, since that's where the Windows build actually reads.
#
# OMNI_HOST is the host:port Flux CONNECTS to (default localhost:8080). It is
# NOT the bind address — the server listens on 0.0.0.0 (see serve.sh); from the
# Windows host, localhost:8080 forwards into WSL. Re-run after changing the port.
# Omni stays the default; other engines are keyword shortcuts ("bangs"):
#   o=Omni  g=Google  ddg=DuckDuckGo  b=Bing
#   yt=YouTube  gh=GitHub  ol=Overleaf  scholar=Scholar  wiki=Wikipedia  arxiv=arXiv
# In Flux's omnibox: `yt lecture`, `gh rust`, `scholar transformers`, etc.
set -euo pipefail

HOST="${OMNI_HOST:-${OMNI_ADDR:-localhost:8080}}"

read -r -d '' JSON <<JSON || true
{
  "engines": [
    {
      "id": "omni",
      "name": "Omni",
      "keyword": "o",
      "search_template": "http://$HOST/search?q={query}",
      "suggest_template": "http://$HOST/ac?q={query}"
    },
    {
      "id": "ddg",
      "name": "DuckDuckGo",
      "keyword": "ddg",
      "search_template": "https://duckduckgo.com/?q={query}",
      "suggest_template": "https://duckduckgo.com/ac/?q={query}&type=list"
    },
    {
      "id": "google",
      "name": "Google",
      "keyword": "g",
      "search_template": "https://www.google.com/search?q={query}",
      "suggest_template": "https://suggestqueries.google.com/complete/search?client=firefox&q={query}"
    },
    {
      "id": "bing",
      "name": "Bing",
      "keyword": "b",
      "search_template": "https://www.bing.com/search?q={query}",
      "suggest_template": null
    },
    {
      "id": "yt",
      "name": "YouTube",
      "keyword": "yt",
      "search_template": "https://www.youtube.com/results?search_query={query}",
      "suggest_template": null
    },
    {
      "id": "gh",
      "name": "GitHub",
      "keyword": "gh",
      "search_template": "https://github.com/search?q={query}&type=repositories",
      "suggest_template": null
    },
    {
      "id": "ol",
      "name": "Overleaf",
      "keyword": "ol",
      "search_template": "https://www.overleaf.com/project",
      "suggest_template": null
    },
    {
      "id": "scholar",
      "name": "Google Scholar",
      "keyword": "scholar",
      "search_template": "https://scholar.google.com/scholar?q={query}",
      "suggest_template": null
    },
    {
      "id": "wiki",
      "name": "Wikipedia",
      "keyword": "wiki",
      "search_template": "https://en.wikipedia.org/w/index.php?search={query}",
      "suggest_template": null
    },
    {
      "id": "arxiv",
      "name": "arXiv",
      "keyword": "arxiv",
      "search_template": "https://arxiv.org/search/?searchtype=all&query={query}",
      "suggest_template": null
    }
  ],
  "default_id": "omni"
}
JSON

# Write to every Flux config dir that applies on this machine.
write_config() {
  local dir="$1"
  mkdir -p "$dir"
  printf '%s\n' "$JSON" > "$dir/search.json"
  echo "omni: wrote $dir/search.json"
}

# 1) Linux/native location.
write_config "${XDG_CONFIG_HOME:-$HOME/.config}/dev.flux.browser"

# 2) Under WSL, also the Windows %APPDATA% location (where a Windows-native
#    Flux build actually reads from).
if grep -qiE "microsoft|wsl" /proc/version 2>/dev/null && command -v cmd.exe >/dev/null 2>&1; then
  win_appdata="$(cmd.exe /c 'echo %APPDATA%' 2>/dev/null | tr -d '\r')"
  if [ -n "$win_appdata" ]; then
    write_config "$(wslpath "$win_appdata")/dev.flux.browser"
  fi
fi

echo "omni: default engine = omni @ http://$HOST — RESTART Flux to apply."
