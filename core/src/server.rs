//! A minimal, std-only HTTP server exposing the contract Flux consumes.
//!
//! Two endpoints make Omni a usable Flux search backend (PLAN.md §0):
//!   * `GET /search?q=...` → an HTML results page (rendered in a Flux webview).
//!   * `GET /ac?q=...`     → autocomplete JSON in OpenSearch list format:
//!                            `["query", ["suggestion", ...]]` — the same shape
//!                            Flux already consumes from DuckDuckGo.
//! Plus `GET /health` for liveness.
//!
//! This is a teaching-grade server (one thread per connection, hand-rolled
//! request parsing). Phase 2 swaps in a real async HTTP stack and splits the
//! Go gateway out front; the index/query core stays untouched.

use crate::index::Index;
use crate::live::{BgMerge, LiveIndex};
use crate::query;
use crate::suggest::Suggester;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;

/// What a route produces: a normal page/JSON, or a redirect (used by bangs).
enum Reply {
    Page {
        status: &'static str,
        ctype: &'static str,
        body: String,
    },
    Redirect(String),
}

fn page(status: &'static str, ctype: &'static str, body: String) -> Reply {
    Reply::Page {
        status,
        ctype,
        body,
    }
}
fn json(status: &'static str, body: String) -> Reply {
    page(status, "application/json; charset=utf-8", body)
}

pub fn serve(
    live: Arc<LiveIndex>,
    addr: &str,
    bg: Option<BgMerge>,
    dir: Option<PathBuf>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    let dir = dir.map(Arc::new);
    // Build the omnibox autocomplete dictionary once from the indexed titles. A
    // background merge never adds/removes live docs, so this stays valid across
    // swaps and needn't be rebuilt.
    let suggester = Arc::new(Suggester::build(&live.snapshot()));
    println!(
        "omni: serving on http://{addr}  ({} docs indexed)",
        live.snapshot().doc_count()
    );
    println!("  GET  /search?q=...  results page (HTML; !bangs redirect)");
    println!("  GET  /ac?q=...      autocomplete (JSON)");
    println!("  GET  /stats         index stats (JSON)");
    println!("  GET  /dashboard     index dashboard (HTML)");
    println!("  POST /ingest        add doc-store records to the live index");

    // Start the live background merger (atomic swaps while serving), if enabled.
    if let Some(cfg) = bg {
        println!(
            "  background merge:   every {}s (merge-factor {})",
            cfg.interval.as_secs(),
            cfg.merge_factor
        );
        crate::live::spawn_background_merger(Arc::clone(&live), cfg);
    }

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let live = Arc::clone(&live);
                let sug = Arc::clone(&suggester);
                let dir = dir.clone();
                // One thread per connection keeps Phase 1 simple and dependency-free.
                std::thread::spawn(move || {
                    let _ = handle(s, live, sug, dir);
                });
            }
            Err(e) => eprintln!("omni: accept error: {e}"),
        }
    }
    Ok(())
}

fn handle(
    mut stream: TcpStream,
    live: Arc<LiveIndex>,
    suggester: Arc<Suggester>,
    dir: Option<Arc<PathBuf>>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(&stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    // e.g. "POST /ingest HTTP/1.1"
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let path = parts.next().unwrap_or("/").to_string();

    // Drain headers; capture Content-Length for a request body.
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
    }

    let route_path = path.split('?').next().unwrap_or("/");
    let reply = if method == "POST" && route_path == "/ingest" {
        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            reader.read_exact(&mut body)?;
        }
        let dpath = dir.as_ref().map(|p| p.as_path());
        ingest_reply(&live, dpath, &String::from_utf8_lossy(&body))
    } else {
        // Cheap snapshot for this request; a concurrent background swap won't
        // disturb it.
        let index = live.snapshot();
        route(&path, &index, &suggester)
    };

    write_reply(&mut stream, reply)
}

fn write_reply(stream: &mut TcpStream, reply: Reply) -> std::io::Result<()> {
    match reply {
        Reply::Page {
            status,
            ctype,
            body,
        } => {
            let head = format!(
                "HTTP/1.1 {status}\r\n\
                 Content-Type: {ctype}\r\n\
                 Content-Length: {}\r\n\
                 Access-Control-Allow-Origin: *\r\n\
                 Cache-Control: no-store\r\n\
                 Connection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes())?;
            stream.write_all(body.as_bytes())?;
        }
        Reply::Redirect(location) => {
            // 302 so a bang query (`!gh rust`) bounces straight to the site.
            let head = format!(
                "HTTP/1.1 302 Found\r\n\
                 Location: {location}\r\n\
                 Content-Length: 0\r\n\
                 Access-Control-Allow-Origin: *\r\n\
                 Cache-Control: no-store\r\n\
                 Connection: close\r\n\r\n"
            );
            stream.write_all(head.as_bytes())?;
        }
    }
    stream.flush()
}

fn route(path: &str, index: &Index, suggester: &Suggester) -> Reply {
    let (route, query_str) = match path.split_once('?') {
        Some((r, q)) => (r, q),
        None => (path, ""),
    };
    let q = query_param(query_str, "q").unwrap_or_default();

    match route {
        "/health" => page("200 OK", "text/plain; charset=utf-8", "ok".to_string()),
        "/favicon.ico" | "/favicon.svg" => page(
            "200 OK",
            "image/svg+xml; charset=utf-8",
            FAVICON_SVG.to_string(),
        ),
        "/static/style.css" => page("200 OK", "text/css; charset=utf-8", STYLE_CSS.to_string()),
        "/static/omni.js" => page(
            "200 OK",
            "text/javascript; charset=utf-8",
            OMNI_JS.to_string(),
        ),
        "/static/dashboard.js" => page(
            "200 OK",
            "text/javascript; charset=utf-8",
            DASHBOARD_JS.to_string(),
        ),
        "/dashboard" => page("200 OK", "text/html; charset=utf-8", dashboard_page()),
        "/ac" => json("200 OK", autocomplete_json(suggester, &q)),
        "/stats" => json("200 OK", stats_json(index)),
        "/sites" => json("200 OK", sites_json()),
        // A `!bang` query redirects to the target site; otherwise normal search.
        "/search" => match crate::bangs::resolve(&q) {
            Some(url) => Reply::Redirect(url),
            None => {
                let opts = parse_search_opts(query_str);
                if query_param(query_str, "fmt").as_deref() == Some("json") {
                    let k = query_param(query_str, "k")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(20usize)
                        .clamp(1, 100);
                    json("200 OK", search_json(index, &q, opts, k))
                } else {
                    page(
                        "200 OK",
                        "text/html; charset=utf-8",
                        results_page(index, &q, opts),
                    )
                }
            }
        },
        "/" => page(
            "200 OK",
            "text/html; charset=utf-8",
            results_page(index, "", query::SearchOpts::default()),
        ),
        _ => page(
            "404 Not Found",
            "text/plain; charset=utf-8",
            "not found".to_string(),
        ),
    }
}

/// Handle `POST /ingest`: parse doc-store records (separated by a line `---`), add
/// the *new* urls as a fresh segment, embed them if the index is embedded, and
/// atomically swap the grown index in (persisting it). Already-indexed urls are
/// skipped — use the offline `--update` path to replace existing pages.
fn ingest_reply(live: &Arc<LiveIndex>, dir: Option<&std::path::Path>, body: &str) -> Reply {
    let snap = live.snapshot();
    // Two accepted formats: JSON `{url,title,text}` (or an array) — what the Flux
    // browser POSTs for the page it's viewing — or the doc-store text format.
    let records: Vec<crate::docstore::Record> = match crate::json::parse_pages(body.trim()) {
        Some(pages) => pages
            .into_iter()
            .filter(|p| !p.url.is_empty())
            .map(|p| crate::docstore::Record {
                url: p.url,
                title: p.title,
                links: Vec::new(),
                text: p.text,
                published: crate::docstore::parse_published(&p.published),
            })
            .collect(),
        None => body
            .split("\n---\n")
            .filter_map(crate::docstore::parse)
            .collect(),
    };
    if records.is_empty() {
        return json(
            "400 Bad Request",
            "{\"error\":\"no valid records — POST JSON {url,title,text} (or an array), or doc-store text (url:/title: headers, blank line, body; records separated by a line ---)\"}".to_string(),
        );
    }

    let mut staging = Index::new();
    staging.set_embedder(snap.embedder().clone());
    let (mut added, mut skipped) = (0usize, 0usize);
    for rec in &records {
        if snap.addr_for_url(&rec.url).is_some() {
            skipped += 1;
            continue;
        }
        let title = if rec.title.is_empty() {
            rec.url.clone()
        } else {
            rec.title.clone()
        };
        staging.add_document(rec.url.clone(), title, &rec.text);
        if rec.published != 0 {
            staging.set_published(&rec.url, rec.published);
        }
        added += 1;
    }

    if added > 0 {
        if staging.embedder().enabled() {
            if let Some(e) = crate::embed::Embedder::from_config(staging.embedder()) {
                staging.embed_missing(&e);
            }
        }
        let next = snap.with_appended(&staging);
        crate::live::commit(live, next, dir);
        println!("omni: ingested {added} doc(s) ({skipped} skipped) — live swap");
    }
    json(
        "200 OK",
        format!("{{\"added\":{added},\"skipped\":{skipped}}}"),
    )
}

/// Extract and percent-decode a query parameter from a raw query string.
fn query_param(query_str: &str, key: &str) -> Option<String> {
    query_str.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        if k == key {
            Some(percent_decode(v))
        } else {
            None
        }
    })
}

/// Minimal percent-decoder that also turns '+' into space (form encoding).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hi = hex_val(bytes[i + 1]);
                let lo = hex_val(bytes[i + 2]);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push(hi << 4 | lo);
                    i += 3;
                    continue;
                }
                out.push(b'%');
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Build OpenSearch-format autocomplete: `["q", ["suggestion", ...]]` — the
/// shape Flux's omnibox consumes. Completions come from prefix-matching the
/// last query token against the indexed vocabulary (see `suggest.rs`).
fn autocomplete_json(suggester: &Suggester, q: &str) -> String {
    let items: Vec<String> = suggester
        .complete(q)
        .iter()
        .map(|s| json_string(s))
        .collect();
    format!("[{}, [{}]]", json_string(q), items.join(", "))
}

/// Live index statistics as JSON — consumed by the dashboard and handy for
/// watching the background merger reshape the index without a restart.
/// The curated essential-site shortcuts as JSON, so clients (e.g. the Flux
/// `flux://omni` dashboard) render the grid from the live bang table instead of
/// mirroring it. One entry per site: its primary `!key`, name, home URL, blurb.
fn sites_json() -> String {
    let items: Vec<String> = crate::bangs::SITES
        .iter()
        .map(|s| {
            format!(
                "{{\"key\":{},\"name\":{},\"home\":{},\"blurb\":{}}}",
                json_string(s.keys[0]),
                json_string(s.name),
                json_string(s.home),
                json_string(s.blurb),
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

fn stats_json(index: &Index) -> String {
    let live = index.doc_count();
    let total = index.total_docs();
    let fs = index.field_stats();
    let emb = index.embedder();

    // Per-segment sizes — makes the tiered merge / background merge visible.
    let sizes: Vec<String> = index
        .segments()
        .iter()
        .map(|s| {
            format!(
                "{{\"live\":{},\"total\":{}}}",
                s.live_count(),
                s.total_docs()
            )
        })
        .collect();

    // Top documents by PageRank authority; also count docs with a publish date.
    let mut ranked: Vec<(f64, &str, &str)> = Vec::new();
    let mut dated = 0usize;
    for seg in index.segments() {
        for d in seg.docs.iter() {
            if !d.deleted {
                ranked.push((d.rank, d.url.as_str(), d.title.as_str()));
                if d.published > 0 {
                    dated += 1;
                }
            }
        }
    }
    ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let top: Vec<String> = ranked
        .iter()
        .take(8)
        .map(|(r, u, t)| {
            format!(
                "{{\"url\":{},\"title\":{},\"rank\":{r:.5}}}",
                json_string(u),
                json_string(t)
            )
        })
        .collect();

    format!(
        "{{\"live_docs\":{live},\"total_docs\":{total},\"tombstones\":{tomb},\
          \"segments\":{segs},\"embedded\":{embd},\"ann\":{ann},\"ann_vectors\":{annv},\
          \"dated\":{dated},\
          \"embedder_kind\":{ek},\"embedder_dim\":{ed},\
          \"avg_title_len\":{atl:.2},\"avg_body_len\":{abl:.2},\
          \"segment_sizes\":[{sizes}],\"top_docs\":[{top}]}}",
        tomb = total - live,
        segs = index.segment_count(),
        embd = emb.enabled(),
        ann = index.ann().is_some(),
        annv = index.ann().map(|a| a.len()).unwrap_or(0),
        ek = json_string(emb.kind_name()),
        ed = emb.dim,
        atl = fs.avg_title_len,
        abl = fs.avg_body_len,
        sizes = sizes.join(","),
        top = top.join(","),
    )
}

/// Parse retrieval tuning from the query string: `lex` (lexical-only),
/// `sw=<f64>` (semantic weight), else the tuned default.
fn parse_search_opts(query_str: &str) -> query::SearchOpts {
    let semantic_weight = if query_param(query_str, "lex").is_some() {
        0.0
    } else {
        query_param(query_str, "sw")
            .and_then(|v| v.parse().ok())
            .unwrap_or(query::DEFAULT_SEMANTIC_WEIGHT)
    };
    query::SearchOpts { semantic_weight }
}

/// Ranked search results as a JSON array (`fmt=json`) — for programmatic clients
/// and the eval harness.
fn search_json(index: &Index, q: &str, opts: query::SearchOpts, k: usize) -> String {
    if q.is_empty() {
        return "[]".to_string();
    }
    let items: Vec<String> = query::search_with(index, q, k, opts)
        .iter()
        .map(|h| {
            format!(
                "{{\"url\":{},\"title\":{},\"score\":{:.5}}}",
                json_string(&h.url),
                json_string(&h.title),
                h.score
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

/// Render the HTML results page.
fn results_page(index: &Index, q: &str, opts: query::SearchOpts) -> String {
    let hits = if q.is_empty() {
        Vec::new()
    } else {
        query::search_with(index, q, 20, opts)
    };

    // Body: a centered landing when there's no query, otherwise a meta line and
    // result cards. (The dashboard/new-tab is Flux's StartPage — Omni only owns
    // the results surface, so the empty state is just a branded search box.)
    let mut body = String::new();
    if q.is_empty() {
        body.push_str(
            "<p class=\"hint\">Search your own index — terms are stemmed, \
             <kbd>\"quoted phrases\"</kbd> match exactly. Jump to a site with a \
             bang: <kbd>!yt</kbd> <kbd>!gh</kbd> <kbd>!ol</kbd>. \
             <a class=\"hint-link\" href=\"/dashboard\">dashboard →</a></p>",
        );
    } else if hits.is_empty() {
        body.push_str(&format!(
            "<p class=\"hint\">No results for <strong>{}</strong>.</p>",
            html_escape(q)
        ));
    } else {
        body.push_str(&format!("<div class=\"meta\">{} results</div>", hits.len()));
        for h in &hits {
            body.push_str(&format!(
                "<div class=\"result\">\
                   <a class=\"title\" href=\"{url}\">{title}</a>\
                   <div class=\"url\">{url}</div>\
                   <div class=\"snippet\">{snippet}</div>\
                   <div class=\"score\">score {score:.3}</div>\
                 </div>",
                url = html_escape(&h.url),
                title = html_escape(&h.title),
                // Snippet is already HTML-safe (escaped + <mark> highlights).
                snippet = h.snippet,
                score = h.score,
            ));
        }
    }

    let shell_class = if q.is_empty() {
        "shell landing"
    } else {
        "shell"
    };
    let brand = "<div class=\"brand\"><span class=\"spark\">✦</span> <b>Omni</b></div>";
    let bar = format!(
        "<div class=\"bar-wrap\">\
           <form id=\"searchform\" class=\"bar\" action=\"/search\" method=\"get\" autocomplete=\"off\">\
             <span class=\"glyph\">⌕</span>\
             <input id=\"q\" name=\"q\" value=\"{q_val}\" placeholder=\"Search Omni\" \
                spellcheck=\"false\" autofocus>\
             <button type=\"submit\">Search</button>\
           </form>\
           <div id=\"suggest\" class=\"suggest\"></div>\
         </div>",
        q_val = html_escape(q),
    );

    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Omni — {q_title}</title>\
         <link rel=\"icon\" href=\"/favicon.svg\">\
         <link rel=\"stylesheet\" href=\"/static/style.css?v={ver}\"></head>\
         <body><div class=\"{shell_class}\">{brand}{bar}{body}</div>\
         <script src=\"/static/omni.js?v={ver}\" defer></script></body></html>",
        ver = env!("CARGO_PKG_VERSION"),
        q_title = if q.is_empty() {
            "search".to_string()
        } else {
            html_escape(q)
        },
    )
}

/// The index dashboard: a glass-card view of live index health (segments, docs,
/// tombstones, embeddings/ANN, top authority), refreshed from `/stats`. Static
/// shell — `dashboard.js` fetches the numbers and renders, so it tracks a
/// background merge live without a reload.
fn dashboard_page() -> String {
    let brand = "<div class=\"brand\"><span class=\"spark\">✦</span> <b>Omni</b>\
                 <span class=\"dash-sub\">index dashboard</span>\
                 <a class=\"dash-link\" href=\"/search\">search →</a></div>";
    // Essential-site shortcuts, rendered server-side from the bang table.
    let shortcuts: String = crate::bangs::SITES
        .iter()
        .map(|s| {
            format!(
                "<li><a href=\"{home}\" class=\"sc-go\"><span class=\"bang\">!{key}</span>\
                 <span class=\"sc-name\">{name}</span>\
                 <span class=\"sc-blurb\">{blurb}</span></a></li>",
                home = html_escape(s.home),
                key = html_escape(s.keys[0]),
                name = html_escape(s.name),
                blurb = html_escape(s.blurb),
            )
        })
        .collect();
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Omni — dashboard</title>\
         <link rel=\"icon\" href=\"/favicon.svg\">\
         <link rel=\"stylesheet\" href=\"/static/style.css?v={ver}\"></head>\
         <body><div class=\"shell dash\">{brand}\
           <div class=\"cards\" id=\"cards\"></div>\
           <div class=\"panel\"><div class=\"panel-h\">Segments <span id=\"segnote\" class=\"panel-note\"></span></div>\
             <div class=\"segbars\" id=\"segbars\"></div></div>\
           <div class=\"panel\"><div class=\"panel-h\">Essential sites <span class=\"panel-note\">type <code>!key</code> in search to jump</span></div>\
             <ul class=\"shortcuts\">{shortcuts}</ul></div>\
           <div class=\"panel\"><div class=\"panel-h\">Top authority · PageRank</div>\
             <ol class=\"toplist\" id=\"toplist\"></ol></div>\
           <div class=\"dash-foot\" id=\"foot\">connecting…</div>\
         </div>\
         <script src=\"/static/dashboard.js?v={ver}\" defer></script></body></html>",
        ver = env!("CARGO_PKG_VERSION"),
    )
}

// The frontend lives in `ui/` (the TypeScript/CSS surface) and is baked into the
// binary at compile time, so the server has zero runtime asset dependencies and
// always works, while the UI stays editable as normal files.
pub const STYLE_CSS: &str = include_str!("../../ui/style.css");
pub const OMNI_JS: &str = include_str!("../../ui/omni.js");
pub const DASHBOARD_JS: &str = include_str!("../../ui/dashboard.js");

/// The Omni "✦" spark mark on a velvet tile — a gradient-filled SVG favicon, so
/// the tab gets an icon and the browser stops 404-ing `/favicon.ico`.
const FAVICON_SVG: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 32 32\">\
    <defs><linearGradient id=\"g\" x1=\"0\" y1=\"0\" x2=\"1\" y2=\"1\">\
      <stop offset=\"0\" stop-color=\"#2ff3ff\"/><stop offset=\"0.55\" stop-color=\"#7b61ff\"/>\
      <stop offset=\"1\" stop-color=\"#ec4be0\"/></linearGradient></defs>\
    <rect width=\"32\" height=\"32\" rx=\"7\" fill=\"#0b0a1d\"/>\
    <text x=\"16\" y=\"23\" font-size=\"22\" text-anchor=\"middle\" fill=\"url(#g)\">\u{2726}</text></svg>";

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Serialize a string as a JSON string literal (quotes + minimal escaping).
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
