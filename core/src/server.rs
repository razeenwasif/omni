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
use crate::telemetry::Telemetry;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

const IMAGES_PER_RESULT: usize = 4;

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
    let telemetry = Arc::new(Telemetry::new());
    println!(
        "omni: serving on http://{addr}  ({} docs indexed)",
        live.snapshot().doc_count()
    );
    println!("  GET  /search?q=...  results page (HTML; !bangs redirect)");
    println!("  GET  /ac?q=...      autocomplete (JSON)");
    println!("  GET  /answer?q=...  generative RAG answer (JSON; opt-in, slow)");
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
                let tel = Arc::clone(&telemetry);
                let dir = dir.clone();
                // One thread per connection keeps Phase 1 simple and dependency-free.
                std::thread::spawn(move || {
                    let _ = handle(s, live, sug, tel, dir);
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
    telemetry: Arc<Telemetry>,
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

    // Streaming RAG answer (SSE) is written straight to the socket token-by-token,
    // not buffered through `Reply`. Drop the request reader first so we can take a
    // mutable borrow of the stream.
    if method == "GET" && route_path == "/answer" {
        let query_str = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        if query_param(query_str, "stream").as_deref() == Some("1") {
            drop(reader);
            let q = query_param(query_str, "q").unwrap_or_default();
            let model = query_param(query_str, "model")
                .unwrap_or_else(|| crate::rag::DEFAULT_MODEL.to_string());
            let index = live.snapshot();
            return stream_answer(&mut stream, &index, &q, &model);
        }
    }

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
        route(&path, &index, &suggester, &telemetry)
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

fn route(path: &str, index: &Index, suggester: &Suggester, telemetry: &Telemetry) -> Reply {
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
        // Generative RAG answer (opt-in, a model call per request — see rag.rs).
        "/answer" => {
            let model = query_param(query_str, "model")
                .unwrap_or_else(|| crate::rag::DEFAULT_MODEL.to_string());
            json("200 OK", answer_json(index, &q, &model))
        }
        "/stats" => json("200 OK", stats_json(index, telemetry)),
        "/sites" => json("200 OK", sites_json()),
        "/click" => match query_param(query_str, "u").filter(|u| safe_redirect_url(u)) {
            Some(url) => {
                telemetry.record_click(&q, &url);
                Reply::Redirect(url)
            }
            None => json(
                "400 Bad Request",
                "{\"error\":\"missing or unsafe redirect url\"}".to_string(),
            ),
        },
        // A `!bang` query redirects to the target site; otherwise normal search.
        "/search" => match crate::bangs::resolve(&q) {
            Some(url) => Reply::Redirect(url),
            None => {
                let opts = parse_search_opts(query_str);
                let json_fmt = query_param(query_str, "fmt").as_deref() == Some("json");
                if !q.is_empty() && !json_fmt {
                    suggester.record_query(&q);
                }
                if json_fmt {
                    let k = query_param(query_str, "k")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(20usize)
                        .clamp(1, 100);
                    json("200 OK", search_json(index, &q, opts, k))
                } else {
                    page(
                        "200 OK",
                        "text/html; charset=utf-8",
                        results_page(index, &q, opts, Some(telemetry)),
                    )
                }
            }
        },
        "/" => page(
            "200 OK",
            "text/html; charset=utf-8",
            results_page(index, "", query::SearchOpts::default(), None),
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
                images: Vec::new(),
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
        staging.add_document_with_images(rec.url.clone(), title, &rec.text, rec.images.clone());
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

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else if b == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn safe_redirect_url(url: &str) -> bool {
    (url.starts_with("http://") || url.starts_with("https://"))
        && !url.contains('\r')
        && !url.contains('\n')
}

fn click_url(q: &str, url: &str) -> String {
    format!("/click?q={}&u={}", percent_encode(q), percent_encode(url))
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

fn stats_json(index: &Index, telemetry: &Telemetry) -> String {
    let live = index.doc_count();
    let total = index.total_docs();
    let fs = index.field_stats();
    let emb = index.embedder();
    let tel = telemetry.snapshot();

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

    let top_queries: Vec<String> = tel
        .top_queries
        .iter()
        .map(|q| {
            format!(
                "{{\"query\":{},\"count\":{},\"zero_results\":{},\"avg_results\":{:.2},\"avg_ms\":{:.2}}}",
                json_string(&q.query),
                q.count,
                q.zero_results,
                q.avg_results,
                q.avg_ms,
            )
        })
        .collect();
    let top_clicks: Vec<String> = tel
        .top_clicks
        .iter()
        .map(|c| {
            format!(
                "{{\"url\":{},\"clicks\":{},\"last_query\":{}}}",
                json_string(&c.url),
                c.clicks,
                json_string(&c.last_query),
            )
        })
        .collect();

    format!(
        "{{\"live_docs\":{live},\"total_docs\":{total},\"tombstones\":{tomb},\
          \"segments\":{segs},\"embedded\":{embd},\"ann\":{ann},\"ann_vectors\":{annv},\
          \"dated\":{dated},\
          \"embedder_kind\":{ek},\"embedder_dim\":{ed},\
          \"searches\":{searches},\"clicks\":{clicks},\"zero_results\":{zeros},\
          \"zero_result_rate\":{zrate:.4},\"avg_search_ms\":{avgms:.2},\"p95_search_ms\":{p95},\
          \"avg_title_len\":{atl:.2},\"avg_body_len\":{abl:.2},\
          \"segment_sizes\":[{sizes}],\"top_docs\":[{top}],\
          \"top_queries\":[{top_queries}],\"top_clicks\":[{top_clicks}]}}",
        tomb = total - live,
        segs = index.segment_count(),
        embd = emb.enabled(),
        ann = index.ann().is_some(),
        annv = index.ann().map(|a| a.len()).unwrap_or(0),
        searches = tel.searches,
        clicks = tel.clicks,
        zeros = tel.zero_results,
        zrate = tel.zero_results as f64 / tel.searches.max(1) as f64,
        avgms = tel.avg_search_ms,
        p95 = tel.p95_search_ms,
        ek = json_string(emb.kind_name()),
        ed = emb.dim,
        atl = fs.avg_title_len,
        abl = fs.avg_body_len,
        sizes = sizes.join(","),
        top = top.join(","),
        top_queries = top_queries.join(","),
        top_clicks = top_clicks.join(","),
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
    query::SearchOpts {
        semantic_weight,
        rerank: query_param(query_str, "rerank").as_deref() == Some("1"),
        rerank_model: query_param(query_str, "rr_model"),
        answer: query_param(query_str, "answer").as_deref() == Some("1"),
        vertical: query_param(query_str, "type")
            .as_deref()
            .map(query::Vertical::from_param)
            .unwrap_or(query::Vertical::All),
    }
}

/// Ranked search results as a JSON array (`fmt=json`) — for programmatic clients
/// and the eval harness.
fn search_json(index: &Index, q: &str, opts: query::SearchOpts, k: usize) -> String {
    if q.is_empty() {
        return "[]".to_string();
    }
    let include_fresh_label = opts.vertical == query::Vertical::Fresh;
    let items: Vec<String> = query::search_with(index, q, k, opts)
        .iter()
        .map(|h| {
            // `answer` is present only on the top hit, and only when `&answer=1`.
            let answer = match &h.answer {
                Some(a) => format!(",\"answer\":{}", json_string(a)),
                None => String::new(),
            };
            let images = if h.images.is_empty() {
                String::new()
            } else {
                let imgs: Vec<String> = ranked_images_for_hit(q, h)
                    .into_iter()
                    .map(|img| {
                        format!(
                            "{{\"url\":{},\"alt\":{}}}",
                            json_string(&img.url),
                            json_string(&img.alt)
                        )
                    })
                    .collect();
                format!(",\"images\":[{}]", imgs.join(","))
            };
            let published = if h.published > 0 {
                format!(
                    ",\"published\":{},\"published_display\":{}",
                    h.published,
                    json_string(&format_date(h.published))
                )
            } else {
                String::new()
            };
            let fresh_label = if include_fresh_label {
                h.fresh_label
                    .map(|label| format!(",\"fresh_label\":{}", json_string(label)))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            format!(
                "{{\"url\":{},\"title\":{},\"score\":{:.5}{}{}{}{}}}",
                json_string(&h.url),
                json_string(&h.title),
                h.score,
                answer,
                images,
                published,
                fresh_label
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

/// Generative RAG answer as JSON: `{"answer": <string|null>, "sources": [...]}`.
/// Grounds a local LLM in the best passages of the top results and asks it to
/// answer with `[n]` citations. Slow (a model call) and opt-in — clients fetch this
/// separately from `/search`, which stays instant.
fn answer_json(index: &Index, q: &str, model: &str) -> String {
    if q.is_empty() {
        return "{\"answer\":null,\"sources\":[]}".to_string();
    }
    let ctx = query::answer_context(index, q, crate::rag::CONTEXT_PASSAGES);
    let sources: Vec<String> = ctx
        .iter()
        .enumerate()
        .map(|(i, (title, url, _))| {
            format!(
                "{{\"n\":{},\"title\":{},\"url\":{}}}",
                i + 1,
                json_string(title),
                json_string(url)
            )
        })
        .collect();
    let base = index.embedder().host_base();
    let answer = match crate::rag::generate(&base, model, q, &ctx) {
        Some(a) => json_string(&a),
        None => "null".to_string(),
    };
    format!(
        "{{\"answer\":{},\"sources\":[{}]}}",
        answer,
        sources.join(",")
    )
}

/// Stream a generative answer as **Server-Sent Events** straight to the client
/// socket. Emits a `sources` event first (instant, from retrieval), then one
/// `token` event per generated chunk, then a `done` event. Each event is a single
/// `data: <json>\n\n` line; the JSON carries a `type` of `sources` | `token` | `done`.
/// Writing fails silently if the client hangs up (generation then stops).
fn stream_answer(
    stream: &mut TcpStream,
    index: &Index,
    q: &str,
    model: &str,
) -> std::io::Result<()> {
    // Per-token writes should hit the wire immediately, not wait on Nagle.
    let _ = stream.set_nodelay(true);
    let head = "HTTP/1.1 200 OK\r\n\
         Content-Type: text/event-stream; charset=utf-8\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n";
    stream.write_all(head.as_bytes())?;

    if q.is_empty() {
        stream.write_all(b"data: {\"type\":\"done\"}\n\n")?;
        return stream.flush();
    }

    // Retrieval first (fast): emit the grounding sources so the UI can show them
    // before the (slower) generation starts.
    let ctx = query::answer_context(index, q, crate::rag::CONTEXT_PASSAGES);
    let sources: Vec<String> = ctx
        .iter()
        .enumerate()
        .map(|(i, (title, url, _))| {
            format!(
                "{{\"n\":{},\"title\":{},\"url\":{}}}",
                i + 1,
                json_string(title),
                json_string(url)
            )
        })
        .collect();
    stream.write_all(
        format!(
            "data: {{\"type\":\"sources\",\"sources\":[{}]}}\n\n",
            sources.join(",")
        )
        .as_bytes(),
    )?;
    stream.flush()?;

    // Stream tokens as the model produces them. Each becomes one SSE `token` event.
    let base = index.embedder().host_base();
    crate::rag::generate_stream(&base, model, q, &ctx, |tok| {
        let ev = format!(
            "data: {{\"type\":\"token\",\"text\":{}}}\n\n",
            json_string(tok)
        );
        stream
            .write_all(ev.as_bytes())
            .and_then(|_| stream.flush())
            .is_ok() // false ⇒ client gone ⇒ stop generating
    });

    stream.write_all(b"data: {\"type\":\"done\"}\n\n")?;
    stream.flush()
}

/// Render the HTML results page.
fn results_page(
    index: &Index,
    q: &str,
    opts: query::SearchOpts,
    telemetry: Option<&Telemetry>,
) -> String {
    let vertical = opts.vertical;
    let hits = if q.is_empty() {
        Vec::new()
    } else {
        // The results surface always tries for a direct answer (extractive, cheap).
        let opts = query::SearchOpts {
            answer: vertical != query::Vertical::Images,
            ..opts.clone()
        };
        let start = Instant::now();
        let hits = query::search_with(index, q, 20, opts);
        if let Some(t) = telemetry {
            t.record_search(q, hits.len(), start.elapsed());
        }
        hits
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
        if let Some(card) = crate::cards::local_card(q) {
            body.push_str(&render_rich_card(&card, None));
        }
        if let Some(correction) = did_you_mean(index, q, opts) {
            body.push_str(&render_did_you_mean(&correction, vertical));
        }
        body.push_str(&format!(
            "<p class=\"hint\">No {} results for <strong>{}</strong>.</p>",
            html_escape(vertical.label()),
            html_escape(q)
        ));
    } else {
        if let Some(card) = crate::cards::local_card(q) {
            body.push_str(&render_rich_card(&card, None));
        }
        // Featured direct answer (the top hit's best-matching passage), if confident.
        if let Some((ans, top)) = hits.first().and_then(|h| h.answer.as_ref().map(|a| (a, h))) {
            let click = click_url(q, &top.url);
            if crate::cards::is_definition_query(q) {
                let card = crate::cards::RichCard {
                    label: "Definition",
                    title: q.to_string(),
                    value: ans.clone(),
                    detail: None,
                };
                body.push_str(&render_rich_card(&card, Some((&click, &top.title))));
            } else {
                body.push_str(&format!(
                    "<div class=\"answer\">\
                       <div class=\"answer-label\">Direct answer</div>\
                       <div class=\"answer-text\">{text}</div>\
                       <a class=\"answer-src\" href=\"{url}\">{title} →</a>\
                     </div>",
                    text = html_escape(ans),
                    url = html_escape(&click),
                    title = html_escape(&top.title),
                ));
            }
        }
        if vertical == query::Vertical::Images {
            let image_count: usize = hits
                .iter()
                .map(|h| ranked_images_for_hit(q, h).len().min(IMAGES_PER_RESULT))
                .sum();
            body.push_str(&format!(
                "<div class=\"meta\">{} images from {} results</div>",
                image_count,
                hits.len()
            ));
            body.push_str(&render_image_grid(q, &hits));
        } else if vertical == query::Vertical::Fresh {
            body.push_str(&format!(
                "<div class=\"meta\">{} fresh results</div>",
                hits.len()
            ));
            for h in &hits {
                body.push_str(&render_news_result(q, h));
            }
        } else {
            body.push_str(&format!(
                "<div class=\"meta\">{} {} results</div>",
                hits.len(),
                html_escape(vertical.label())
            ));
            for h in &hits {
                let click = click_url(q, &h.url);
                body.push_str(&format!(
                    "<div class=\"result\">\
                       <a class=\"title\" href=\"{url}\">{title}</a>\
                       <div class=\"url\">{raw_url}</div>\
                       <div class=\"snippet\">{snippet}</div>\
                       <div class=\"score\">score {score:.3}</div>\
                     </div>",
                    url = html_escape(&click),
                    raw_url = html_escape(&h.url),
                    title = html_escape(&h.title),
                    // Snippet is already HTML-safe (escaped + <mark> highlights).
                    snippet = h.snippet,
                    score = h.score,
                ));
            }
        }
        let related = related_searches(index, q, vertical, &hits, opts);
        if !related.is_empty() {
            body.push_str(&render_related_searches(&related, vertical));
        }
    }

    let shell_class = if q.is_empty() {
        "shell landing"
    } else {
        "shell"
    };
    let brand = "<div class=\"brand\"><span class=\"spark\">✦</span> <b>Omni</b></div>";
    let hidden_type = if vertical == query::Vertical::All {
        String::new()
    } else {
        format!(
            "<input type=\"hidden\" name=\"type\" value=\"{}\">",
            html_escape(vertical.param())
        )
    };
    let bar = format!(
        "<div class=\"bar-wrap\">\
           <form id=\"searchform\" class=\"bar\" action=\"/search\" method=\"get\" autocomplete=\"off\">\
             <span class=\"glyph\">⌕</span>\
             <input id=\"q\" name=\"q\" value=\"{q_val}\" placeholder=\"Search Omni\" \
                spellcheck=\"false\" autofocus>\
             {hidden_type}\
             <button type=\"submit\">Search</button>\
           </form>\
           <div id=\"suggest\" class=\"suggest\"></div>\
         </div>",
        q_val = html_escape(q),
        hidden_type = hidden_type,
    );
    let tabs = if q.is_empty() {
        String::new()
    } else {
        vertical_tabs(q, vertical)
    };

    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Omni — {q_title}</title>\
         <link rel=\"icon\" href=\"/favicon.svg\">\
         <link rel=\"stylesheet\" href=\"/static/style.css?v={ver}\"></head>\
         <body><div class=\"{shell_class}\">{brand}{bar}{tabs}{body}</div>\
         <script src=\"/static/omni.js?v={ver}\" defer></script></body></html>",
        ver = env!("CARGO_PKG_VERSION"),
        tabs = tabs,
        q_title = if q.is_empty() {
            "search".to_string()
        } else {
            html_escape(q)
        },
    )
}

fn vertical_tabs(q: &str, active: query::Vertical) -> String {
    let tabs = [
        query::Vertical::All,
        query::Vertical::Images,
        query::Vertical::Fresh,
        query::Vertical::Docs,
        query::Vertical::Code,
        query::Vertical::Sites,
    ];
    let items: String = tabs
        .iter()
        .map(|&v| {
            let href = if v == query::Vertical::All {
                format!("/search?q={}", percent_encode(q))
            } else {
                format!("/search?q={}&type={}", percent_encode(q), v.param())
            };
            format!(
                "<a class=\"tab{active}\" href=\"{href}\">{label}</a>",
                active = if v == active { " active" } else { "" },
                href = html_escape(&href),
                label = html_escape(v.label()),
            )
        })
        .collect();
    format!("<nav class=\"tabs\" aria-label=\"Search verticals\">{items}</nav>")
}

fn render_image_grid(q: &str, hits: &[query::Hit]) -> String {
    let mut items = String::new();
    for h in hits {
        let click = click_url(q, &h.url);
        for image in ranked_images_for_hit(q, h)
            .into_iter()
            .take(IMAGES_PER_RESULT)
        {
            let label = if image.alt.is_empty() {
                &h.title
            } else {
                &image.alt
            };
            items.push_str(&format!(
                "<a class=\"image-card\" href=\"{page_url}\">\
                   <img src=\"{image_url}\" alt=\"{alt}\" loading=\"lazy\" referrerpolicy=\"no-referrer\">\
                   <span class=\"image-caption\">{caption}</span>\
                   <span class=\"image-source\">{source}</span>\
                 </a>",
                page_url = html_escape(&click),
                image_url = html_escape(&image.url),
                alt = html_escape(label),
                caption = html_escape(label),
                source = html_escape(&h.title),
            ));
        }
    }
    format!("<div class=\"image-grid\">{items}</div>")
}

fn ranked_images_for_hit<'a>(q: &str, h: &'a query::Hit) -> Vec<&'a crate::index::Image> {
    let terms = image_query_terms(q);
    let title = h.title.to_lowercase();
    let mut ranked: Vec<(usize, usize, &crate::index::Image)> = h
        .images
        .iter()
        .enumerate()
        .map(|(idx, image)| (image_match_score(&terms, &title, image), idx, image))
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    ranked.into_iter().map(|(_, _, image)| image).collect()
}

fn image_query_terms(q: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut terms = Vec::new();
    for part in q.split_whitespace() {
        let part = part.trim_matches('"');
        if part.eq_ignore_ascii_case("or") || part.starts_with('-') || part.contains(':') {
            continue;
        }
        for token in crate::analyze::tokenize(part) {
            if token.len() < 2 || crate::analyze::is_stopword(&token) {
                continue;
            }
            if seen.insert(token.clone()) {
                terms.push(token);
            }
        }
    }
    terms
}

fn image_match_score(terms: &[String], title: &str, image: &crate::index::Image) -> usize {
    if terms.is_empty() {
        return usize::from(!image.alt.is_empty());
    }
    let alt = image.alt.to_lowercase();
    let url = image.url.to_lowercase();
    let mut score = usize::from(!alt.is_empty());
    for term in terms {
        if !alt.is_empty() && alt.contains(term) {
            score += 8;
        }
        if url.contains(term) {
            score += 5;
        }
        if title.contains(term) {
            score += 2;
        }
    }
    score
}

fn did_you_mean(index: &Index, q: &str, opts: query::SearchOpts) -> Option<String> {
    let correction = crate::spell::correction(index, q)?;
    let opts = query::SearchOpts {
        answer: false,
        ..opts
    };
    (!query::search_with(index, &correction, 1, opts).is_empty()).then_some(correction)
}

fn render_did_you_mean(correction: &str, vertical: query::Vertical) -> String {
    let href = if vertical == query::Vertical::All {
        format!("/search?q={}", percent_encode(correction))
    } else {
        format!(
            "/search?q={}&type={}",
            percent_encode(correction),
            vertical.param()
        )
    };
    format!(
        "<p class=\"did-you-mean\">Did you mean <a href=\"{href}\">{query}</a>?</p>",
        href = html_escape(&href),
        query = html_escape(correction),
    )
}

fn related_searches(
    index: &Index,
    q: &str,
    vertical: query::Vertical,
    hits: &[query::Hit],
    opts: query::SearchOpts,
) -> Vec<String> {
    let q_terms: std::collections::HashSet<String> =
        crate::analyze::tokenize(q).into_iter().collect();
    if q_terms.is_empty() {
        return Vec::new();
    }

    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for hit in hits.iter().take(8) {
        for token in crate::analyze::tokenize(&hit.title) {
            if token.len() >= 3 && !crate::analyze::is_stopword(&token) && !q_terms.contains(&token)
            {
                *counts.entry(token).or_insert(0) += 1;
            }
        }
    }

    let mut ranked: Vec<(usize, String)> = counts.into_iter().map(|(t, n)| (n, t)).collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

    let mut out = Vec::new();
    let opts = query::SearchOpts {
        answer: false,
        vertical,
        ..opts
    };
    for (_, token) in ranked {
        let candidate = format!("{q} {token}");
        if !query::search_with(index, &candidate, 1, opts.clone()).is_empty() {
            out.push(candidate);
            if out.len() >= 6 {
                break;
            }
        }
    }
    out
}

fn render_related_searches(items: &[String], vertical: query::Vertical) -> String {
    let chips: String = items
        .iter()
        .map(|item| {
            let href = if vertical == query::Vertical::All {
                format!("/search?q={}", percent_encode(item))
            } else {
                format!(
                    "/search?q={}&type={}",
                    percent_encode(item),
                    vertical.param()
                )
            };
            format!(
                "<a class=\"related-chip\" href=\"{href}\">{label}</a>",
                href = html_escape(&href),
                label = html_escape(item),
            )
        })
        .collect();
    format!(
        "<section class=\"related-searches\"><div class=\"related-title\">Related searches</div><div class=\"related-list\">{chips}</div></section>"
    )
}

fn render_news_result(q: &str, h: &query::Hit) -> String {
    let click = click_url(q, &h.url);
    let source = source_label(&h.url);
    let date = if h.published > 0 {
        format_date(h.published)
    } else {
        h.fresh_label.unwrap_or("Fresh").to_string()
    };
    format!(
        "<article class=\"news-result\">\
           <div class=\"news-kicker\"><span>{date}</span><span>{source}</span></div>\
           <a class=\"title\" href=\"{url}\">{title}</a>\
           <div class=\"snippet\">{snippet}</div>\
           <div class=\"url\">{raw_url}</div>\
         </article>",
        date = html_escape(&date),
        source = html_escape(&source),
        url = html_escape(&click),
        raw_url = html_escape(&h.url),
        title = html_escape(&h.title),
        snippet = h.snippet,
    )
}

fn render_rich_card(card: &crate::cards::RichCard, source: Option<(&str, &str)>) -> String {
    let detail = card
        .detail
        .as_ref()
        .map(|d| format!("<div class=\"rich-detail\">{}</div>", html_escape(d)))
        .unwrap_or_default();
    let source = source
        .map(|(url, title)| {
            format!(
                "<a class=\"answer-src\" href=\"{}\">{} →</a>",
                html_escape(url),
                html_escape(title)
            )
        })
        .unwrap_or_default();
    format!(
        "<div class=\"rich-card\">\
           <div class=\"answer-label\">{label}</div>\
           <div class=\"rich-title\">{title}</div>\
           <div class=\"rich-value\">{value}</div>\
           {detail}{source}\
         </div>",
        label = html_escape(card.label),
        title = html_escape(&card.title),
        value = html_escape(&card.value),
        detail = detail,
        source = source,
    )
}

fn source_label(url: &str) -> String {
    let host = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    host.split(['/', '?', '#'])
        .next()
        .unwrap_or(host)
        .trim_start_matches("www.")
        .to_string()
}

fn format_date(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    (y + if m <= 2 { 1 } else { 0 }, m, d)
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
           <div class=\"panel\"><div class=\"panel-h\">Search telemetry <span class=\"panel-note\">current server session</span></div>\
             <ol class=\"toplist\" id=\"querylist\"></ol></div>\
           <div class=\"panel\"><div class=\"panel-h\">Clicked results <span class=\"panel-note\">current server session</span></div>\
             <ol class=\"toplist\" id=\"clicklist\"></ol></div>\
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

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(images: Vec<crate::index::Image>) -> query::Hit {
        query::Hit {
            url: "https://example.com/page".into(),
            title: "Rust media gallery".into(),
            images,
            published: 0,
            fresh_label: None,
            snippet: String::new(),
            score: 1.0,
            answer: None,
        }
    }

    #[test]
    fn image_grid_ranks_thumbnails_by_query_metadata() {
        let h = hit(vec![
            crate::index::Image {
                url: "https://cdn.example.com/banner.png".into(),
                alt: "Header artwork".into(),
            },
            crate::index::Image {
                url: "https://cdn.example.com/rust-logo.png".into(),
                alt: "Ferris Rust logo".into(),
            },
        ]);

        let ranked = ranked_images_for_hit("rust logo", &h);
        assert_eq!(ranked[0].url, "https://cdn.example.com/rust-logo.png");
        assert_eq!(ranked[1].url, "https://cdn.example.com/banner.png");
    }

    #[test]
    fn image_query_terms_ignore_search_operators() {
        assert_eq!(
            image_query_terms(r#"rust OR logo site:example.com -draft "mark""#),
            vec!["rust", "logo", "mark"]
        );
    }
}
