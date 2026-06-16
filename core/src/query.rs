//! Query execution: turn a query string into a ranked list of documents.
//!
//! The pipeline:
//!   1. Parse the query into free terms plus any `"quoted phrases"`, analyzing
//!      both the same way documents are analyzed (stop-words + stemming).
//!   2. Recall a candidate pool by **BM25F** (title-weighted), via WAND pruning
//!      for free-term queries or exhaustive scoring when a phrase is present.
//!   3. Add a **proximity bonus** when distinct query terms cluster together.
//!   4. If the query has phrases, **filter** to docs where the phrase terms are
//!      positionally adjacent (true phrase match via the positional index).
//!   5. Fold in **PageRank** as a mild, query-independent authority multiplier.
//!   6. Build a query-biased, highlighted snippet and return the top-K.
//!
//! This is the two-phase "cheap recall → rich re-rank" architecture: BM25F+WAND
//! finds candidates, then proximity/PageRank/phrase signals reorder them.

use crate::analyze;
use crate::embed;
use crate::index::Index;
use crate::score::Bm25f;
use crate::segment::Segment;
use crate::snippet;
use crate::wand;
use std::collections::{HashMap, HashSet};

/// A global document address: `(segment index, local doc id)`.
type Addr = (usize, usize);

/// One ranked search result.
pub struct Hit {
    pub url: String,
    pub title: String,
    /// HTML-safe, highlighted snippet (already escaped — insert as-is).
    pub snippet: String,
    pub score: f64,
    /// Extractive "direct answer": the doc's passage that best matches the query,
    /// set only on the top hit and only when `SearchOpts::answer` is on and the
    /// match is confident enough. Plain text (not HTML — escape at render time).
    pub answer: Option<String>,
}

/// Strength of the proximity bonus when distinct query terms appear near each
/// other. Scaled by the matched terms' IDF sum and the tightness of the span.
const PROXIMITY_WEIGHT: f64 = 2.0;

/// How strongly PageRank influences the final order. The multiplier is
/// `1 + PAGERANK_WEIGHT * (rank / max_rank)`, so authority breaks ties between
/// textually-similar docs without overwhelming relevance.
const PAGERANK_WEIGHT: f64 = 0.5;

/// How strongly recency nudges the order, and its decay scale. The boost is
/// `1 + FRESHNESS_WEIGHT * exp(-age_days / FRESHNESS_TAU_DAYS)` for docs with a
/// known publish date (undated docs are neutral). Kept small — relevance and
/// authority dominate; freshness is only a gentle tiebreak (and many reference
/// pages have no date, so it must never punish them).
const FRESHNESS_WEIGHT: f64 = 0.10;
const FRESHNESS_TAU_DAYS: f64 = 365.0;

/// A parsed query: free terms plus phrases.
struct ParsedQuery {
    /// Every analyzed query term, deduplicated — used for BM25, title boost,
    /// proximity, and snippet highlighting.
    terms: Vec<String>,
    /// Quoted phrases. Each is a list of `(stemmed_term, relative_offset)`,
    /// where the offset preserves gaps left by stop-words inside the phrase, so
    /// `"search the engine"` matches docs with that exact spacing.
    phrases: Vec<Vec<(String, u32)>>,
}

/// Split a raw query into free terms and `"quoted phrases"`, analyzing tokens.
fn parse_query(query: &str) -> ParsedQuery {
    let mut phrases = Vec::new();
    let mut term_set: HashSet<String> = HashSet::new();
    let mut terms = Vec::new();

    let mut push_term = |t: String, terms: &mut Vec<String>| {
        if term_set.insert(t.clone()) {
            terms.push(t);
        }
    };

    let mut rest = query;
    while let Some(open) = rest.find('"') {
        for t in analyze::analyze_terms(&rest[..open]) {
            push_term(t, &mut terms);
        }
        let after = &rest[open + 1..];
        match after.find('"') {
            Some(close) => {
                let phrase = analyze::analyze_positions(&after[..close]);
                for (t, _) in &phrase {
                    push_term(t.clone(), &mut terms);
                }
                if phrase.len() >= 2 {
                    // Normalize offsets so the first term sits at 0.
                    let base = phrase[0].1;
                    phrases.push(phrase.into_iter().map(|(t, p)| (t, p - base)).collect());
                }
                rest = &after[close + 1..];
            }
            None => {
                for t in analyze::analyze_terms(after) {
                    push_term(t, &mut terms);
                }
                rest = "";
                break;
            }
        }
    }
    for t in analyze::analyze_terms(rest) {
        push_term(t, &mut terms);
    }

    ParsedQuery { terms, phrases }
}

/// Run `query` against `index`, returning up to `k` ranked hits.
///
/// Each segment is searched independently, but with **globally aggregated**
/// BM25F stats (collection size, document frequencies, field averages) so scores
/// are comparable across segments; results are merged into one global top-K.
/// Convenience entry with default options (used widely by tests).
#[allow(dead_code)]
pub fn search(index: &Index, query: &str, k: usize) -> Vec<Hit> {
    search_with(index, query, k, SearchOpts::default())
}

/// Knobs for tuning retrieval (used by the eval harness and the `sw`/`lex`/
/// `rerank` query params).
#[derive(Clone)]
pub struct SearchOpts {
    /// Weight of the semantic ranking in the hybrid fusion relative to lexical
    /// (1.0 = equal RRF, the default). `0.0` disables semantic entirely
    /// (lexical-only), even when the index is embedded.
    pub semantic_weight: f64,
    /// Cross-encoder rerank the top candidates with a local LLM (opt-in; adds a
    /// model call per query). See `rerank.rs`.
    pub rerank: bool,
    /// Reranker model override (else `rerank::DEFAULT_MODEL`).
    pub rerank_model: Option<String>,
    /// Attach an extractive **direct answer** (the top hit's best-matching passage)
    /// to `Hit::answer`. Needs an embedded index; costs one query embedding plus a
    /// cosine scan of the top doc's passages. No LLM generation.
    pub answer: bool,
}

impl Default for SearchOpts {
    fn default() -> Self {
        SearchOpts {
            semantic_weight: DEFAULT_SEMANTIC_WEIGHT,
            rerank: false,
            rerank_model: None,
            answer: false,
        }
    }
}

/// Minimum query↔passage cosine for a passage to be shown as a direct answer.
/// `nomic-embed-text` puts genuinely on-topic passages around 0.6–0.75; below this
/// we'd rather show nothing than a confidently-wrong paragraph.
const ANSWER_MIN_SIM: f32 = 0.6;

/// Tuned default weight for the semantic ranking in the hybrid fusion. Tuned with
/// `scripts/eval.py`, which uses **graded relevance / nDCG@10** (each query has a
/// set of relevant docs with grades, not one exact target) on the ~7.8k-doc corpus.
/// Graded nDCG@10 peaks at `sw≈1–2` (0.68 vs 0.51 lexical-only) and success@10 hits
/// **1.0** at `sw=2` — a balanced 2:1 hybrid surfaces the most of each relevant
/// *family*. (An earlier single-exact-target metric over-favored high weights by
/// only rewarding the one canonical page; graded relevance is the better instrument.)
/// `sw=`/`lex` query params override per request.
pub const DEFAULT_SEMANTIC_WEIGHT: f64 = 2.0;

/// Run `query` against `index` with explicit retrieval options.
pub fn search_with(index: &Index, query: &str, k: usize, opts: SearchOpts) -> Vec<Hit> {
    let bm25 = Bm25f::default();
    let total = index.doc_count();
    let stats = index.field_stats();
    let parsed = parse_query(query);
    let segs = index.segments();

    // Global IDF per query term (df summed across segments).
    let mut idf: HashMap<&str, f64> = HashMap::new();
    for term in &parsed.terms {
        idf.insert(term.as_str(), bm25.idf(index.term_df(term), total));
    }

    // 1. Phase A — BM25F recall per segment (block-max WAND), merged. Phrase
    //    queries fall back to exhaustive per-segment scoring. The pool per
    //    segment is larger than `k` so re-ranking below can promote within it.
    let pool = k.saturating_mul(5).max(100);
    let mut scores: HashMap<Addr, f64> = HashMap::new();
    if parsed.phrases.is_empty() {
        for (si, seg) in segs.iter().enumerate() {
            for s in wand::retrieve_bm25_blockmax(seg, &parsed.terms, &idf, stats, pool) {
                scores.insert((si, s.doc_id), s.score);
            }
        }
    } else {
        for (si, seg) in segs.iter().enumerate() {
            for term in &parsed.terms {
                let Some(postings) = seg.postings(term) else {
                    continue;
                };
                let term_idf = idf[term.as_str()];
                for p in postings.iter() {
                    if !seg.is_live(p.doc_id) {
                        continue; // skip tombstones
                    }
                    let d = &seg.docs[p.doc_id];
                    *scores.entry((si, p.doc_id)).or_insert(0.0) += bm25.term_score(
                        term_idf,
                        p.tf_title,
                        p.tf_body,
                        d.len_title,
                        d.len_body,
                        stats,
                    );
                }
            }
        }
    }

    // 2. Proximity bonus: reward docs where the distinct query terms cluster.
    if parsed.terms.len() >= 2 {
        for (&(si, local), score) in scores.iter_mut() {
            if let Some(bonus) = proximity_bonus(&segs[si], local, &parsed.terms, &idf) {
                *score += bonus;
            }
        }
    }

    // 3. Phrase filter: keep only docs containing every phrase (AND semantics).
    if !parsed.phrases.is_empty() {
        scores.retain(|&(si, local), _| {
            parsed
                .phrases
                .iter()
                .all(|phrase| contains_phrase(&segs[si], local, phrase))
        });
    }

    // 4. PageRank fold (normalized over the global max; relevance dominates).
    let max_rank = segs
        .iter()
        .flat_map(|s| s.docs.iter())
        .map(|d| d.rank)
        .fold(0.0_f64, f64::max);
    if max_rank > 0.0 {
        for (&(si, local), score) in scores.iter_mut() {
            let norm = segs[si].docs[local].rank / max_rank;
            *score *= 1.0 + PAGERANK_WEIGHT * norm;
        }
    }

    // 4b. Freshness: a mild recency boost for docs with a known publish date.
    //     Undated docs (most reference pages) are left neutral, never penalized.
    let now = now_unix();
    if now > 0 {
        for (&(si, local), score) in scores.iter_mut() {
            let published = segs[si].docs[local].published;
            if published > 0 {
                let age_days = (now - published).max(0) as f64 / 86_400.0;
                let recency = (-age_days / FRESHNESS_TAU_DAYS).exp();
                *score *= 1.0 + FRESHNESS_WEIGHT * recency;
            }
        }
    }

    // 5. Lexical ranking (full pool, sorted by score).
    let mut lexical: Vec<(Addr, f64)> = scores.iter().map(|(&a, &s)| (a, s)).collect();
    lexical.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Embed the query once (reused by semantic fusion, the reranker, and the
    // answer step) so we never round-trip the embedder more than necessary. Only
    // when the index is embedded and something below actually needs it.
    let qvec: Option<Vec<f32>> = {
        let cfg = index.embedder();
        if cfg.enabled() && (opts.semantic_weight > 0.0 || opts.rerank || opts.answer) {
            embed::Embedder::from_config(cfg)
                .map(|e| e.embed_query(query))
                .filter(|v| !v.is_empty())
        } else {
            None
        }
    };

    // 6. Hybrid: if embedded (and semantic isn't disabled), fuse the lexical and
    //    semantic rankings with weighted Reciprocal Rank Fusion.
    let mut ranked: Vec<(Addr, f64)> = if opts.semantic_weight > 0.0 {
        match qvec
            .as_deref()
            .and_then(|qv| semantic_ranking(index, qv, &scores, &parsed, pool))
        {
            Some(semantic) => rrf_weighted(
                &[(ids(&lexical), 1.0), (semantic, opts.semantic_weight)],
                RRF_K,
            ),
            None => lexical,
        }
    } else {
        lexical // semantic disabled (lexical-only)
    };

    // 7. Optional cross-encoder rerank of the top candidates (opt-in; a model
    //    call per query, so it's a "deep search" mode — see rerank.rs).
    if opts.rerank && ranked.len() > 1 {
        let qterms: HashSet<String> = parsed.terms.iter().cloned().collect();
        rerank_pool(index, query, &mut ranked, &qterms, qvec.as_deref(), &opts);
    }
    ranked.truncate(k);

    // 8. RAG answer mode: the top hit's best-matching passage, returned verbatim as
    //    an extractive direct answer (no generation — the passage *is* the answer).
    let answer = if opts.answer {
        ranked
            .first()
            .zip(qvec.as_deref())
            .and_then(|(&((si, local), _score), qv)| {
                let (i, sim) = best_passage(&segs[si], local, qv)?;
                (sim >= ANSWER_MIN_SIM)
                    .then(|| passage_text(&segs[si], local, i, 0))
                    .flatten()
            })
    } else {
        None
    };

    // Build snippets only for the survivors.
    let term_set: HashSet<String> = parsed.terms.iter().cloned().collect();
    let mut hits: Vec<Hit> = ranked
        .into_iter()
        .map(|((si, local), score)| {
            let seg = &segs[si];
            let text = seg.text(local); // decoded from the mapping on demand
            Hit {
                url: seg.docs[local].url.clone(),
                title: seg.docs[local].title.clone(),
                snippet: snippet::make(text.as_ref(), &term_set),
                score,
                answer: None,
            }
        })
        .collect();
    if let (Some(a), Some(h)) = (answer, hits.first_mut()) {
        h.answer = Some(a);
    }
    hits
}

/// Top-`n` results as `(title, url, best-passage-text)` for grounding a generative
/// answer (`rag::generate`). Runs the normal hybrid search, then for each hit pulls
/// the passage that best matches the query. Empty when the index isn't embedded or
/// the query can't be embedded (a generative answer needs dense grounding).
pub fn answer_context(index: &Index, query: &str, n: usize) -> Vec<(String, String, String)> {
    let cfg = index.embedder();
    if !cfg.enabled() {
        return Vec::new();
    }
    let qv = match embed::Embedder::from_config(cfg).map(|e| e.embed_query(query)) {
        Some(v) if !v.is_empty() => v,
        _ => return Vec::new(),
    };
    let hits = search_with(index, query, n, SearchOpts::default());
    let segs = index.segments();
    hits.into_iter()
        .filter_map(|h| {
            let (si, local) = index.addr_for_url(&h.url)?;
            let (i, _) = best_passage(&segs[si], local, &qv)?;
            let text = passage_text(&segs[si], local, i, 0)?;
            Some((h.title, h.url, text))
        })
        .collect()
}

/// The doc's best-matching passage for `qv`: its index and cosine. `None` if the
/// doc has no stored passage vectors.
fn best_passage(seg: &Segment, local: usize, qv: &[f32]) -> Option<(usize, f32)> {
    seg.passages(local)
        .iter()
        .enumerate()
        .map(|(i, p)| (i, embed::cosine(qv, p)))
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
}

/// Plain text of passage `i`, re-chunked from the doc's stored text with the same
/// chunker used at build time (so the index aligns with the stored passage
/// vectors). HTML entities that survived crawling are decoded so the passage reads
/// as prose (for the answer card *and* the reranker). `max_words == 0` returns the
/// whole passage; otherwise it's truncated.
fn passage_text(seg: &Segment, local: usize, i: usize, max_words: usize) -> Option<String> {
    let text = seg.text(local);
    let (words_per, max) = crate::passages::params();
    let p = crate::passages::chunk(text.as_ref(), words_per, max)
        .into_iter()
        .nth(i)?;
    let p = if max_words == 0 {
        p
    } else {
        p.split_whitespace()
            .take(max_words)
            .collect::<Vec<_>>()
            .join(" ")
    };
    Some(decode_entities(&p))
}

/// Decode the HTML entities that survive into stored crawl text (numeric `&#NN;` /
/// `&#xHH;` and the common named ones) so a direct answer reads as plain prose;
/// anything unrecognized passes through untouched. The result is later
/// HTML-escaped at render, so decode-then-escape stays XSS-safe.
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        match after.find(';') {
            Some(semi) if semi <= 10 => match entity_char(&after[..semi]) {
                Some(ch) => {
                    out.push(ch);
                    rest = &after[semi + 1..];
                }
                None => {
                    out.push('&');
                    rest = after;
                }
            },
            _ => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Map an entity body (the text between `&` and `;`) to its character.
fn entity_char(ent: &str) -> Option<char> {
    if let Some(num) = ent.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse::<u32>().ok()?,
        };
        return char::from_u32(code);
    }
    Some(match ent {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "rsquo" => '\u{2019}',
        "lsquo" => '\u{2018}',
        "rdquo" => '\u{201D}',
        "ldquo" => '\u{201C}',
        "mdash" => '\u{2014}',
        "ndash" => '\u{2013}',
        "hellip" => '\u{2026}',
        "laquo" => '\u{00AB}',
        "raquo" => '\u{00BB}',
        "copy" => '\u{00A9}',
        "reg" => '\u{00AE}',
        "trade" => '\u{2122}',
        "deg" => '\u{00B0}',
        _ => return None,
    })
}

/// Rerank the top `rerank::POOL` of `ranked` in place with the LLM cross-encoder.
/// On any failure the order is left unchanged (reranking only ever helps).
fn rerank_pool(
    index: &Index,
    query: &str,
    ranked: &mut [(Addr, f64)],
    terms: &HashSet<String>,
    qv: Option<&[f32]>,
    opts: &SearchOpts,
) {
    let pool = ranked.len().min(crate::rerank::POOL);
    if pool < 2 {
        return;
    }
    let segs = index.segments();
    let docs: Vec<(String, String)> = ranked[..pool]
        .iter()
        .map(|&((si, local), _)| {
            let seg = &segs[si];
            let title = seg.docs[local].title.clone();
            // Feed the reranker the doc's *semantically* best passage — the real
            // dense-retrieval unit it should judge — rather than a keyword lead
            // snippet. Falls back to a query-biased snippet when there's no query
            // vector (lexical-only) or the doc has no passages.
            let passage = qv
                .and_then(|qv| best_passage(seg, local, qv))
                .and_then(|(i, _)| passage_text(seg, local, i, 80))
                .unwrap_or_else(|| crate::snippet::plain(&seg.text(local), terms, 60));
            (title, passage)
        })
        .collect();
    let base = index.embedder().host_base();
    let model = opts
        .rerank_model
        .as_deref()
        .unwrap_or(crate::rerank::DEFAULT_MODEL);
    if let Some(new_order) = crate::rerank::order(&base, model, query, &docs) {
        // (Addr, f64) is Copy, so collect the reordered prefix then write it back.
        let reordered: Vec<(Addr, f64)> = new_order.iter().map(|&i| ranked[i]).collect();
        ranked[..pool].copy_from_slice(&reordered);
    }
}

/// Current unix time in seconds (0 if the clock is unavailable/before epoch).
fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Reciprocal Rank Fusion constant (standard default).
const RRF_K: f64 = 60.0;

fn ids(ranked: &[(Addr, f64)]) -> Vec<Addr> {
    ranked.iter().map(|&(a, _)| a).collect()
}

/// A semantic ranking of doc addresses by cosine to the (pre-computed) query
/// vector `qv` — a doc's score is its best passage's cosine. Uses the HNSW graph
/// when present, else exact brute force over the embedded corpus.
fn semantic_ranking(
    index: &Index,
    qv: &[f32],
    lexical_pool: &HashMap<Addr, f64>,
    parsed: &ParsedQuery,
    pool: usize,
) -> Option<Vec<Addr>> {
    // Fast path: for a free-term query, recall semantic neighbors from the HNSW
    // graph in ~O(log N) instead of scanning every vector. (Phrase queries keep
    // the exact path below, scoring only the small phrase-filtered pool.)
    if parsed.phrases.is_empty() {
        if let Some(ann) = index.ann() {
            // The graph indexes *passages*; lazy graphs decode a passage vector via
            // this fetch (RAM graphs ignore it). Both `ann` and the closure borrow
            // the index immutably — fine.
            let fetch = |(s, l): Addr, pi: u32| {
                index
                    .segments()
                    .get(s)
                    .and_then(|seg| seg.passages(l).into_iter().nth(pi as usize))
                    .unwrap_or_default()
            };
            // Over-fetch passage hits (a doc has several), then collapse to the best
            // passage per doc — results are sorted best-first, so first-seen wins.
            let ef = (pool * 2).max(64);
            let raw = ann.search(qv, ef, pool * crate::passages::MAX_PASSAGES, Some(&fetch));
            if !raw.is_empty() {
                let mut seen: HashSet<Addr> = HashSet::new();
                let mut docs: Vec<Addr> = Vec::new();
                for (addr, _sim) in raw {
                    if seen.insert(addr) {
                        docs.push(addr);
                    }
                }
                docs.truncate(pool);
                return Some(docs);
            }
        }
    }

    let segs = index.segments();

    // Candidate set: with a phrase, restrict to docs that passed the filter;
    // otherwise the whole live, embedded corpus (exact brute-force fallback when
    // no ANN graph was built). A doc's score is its **best passage**'s cosine.
    let mut scored: Vec<(Addr, f32)> = Vec::new();
    let consider = |si: usize, local: usize, scored: &mut Vec<(Addr, f32)>| {
        let seg = &segs[si];
        if seg.docs[local].emb_len > 0 {
            let best = seg
                .passages(local)
                .iter()
                .map(|p| embed::cosine(qv, p))
                .fold(f32::NEG_INFINITY, f32::max);
            if best.is_finite() {
                scored.push(((si, local), best));
            }
        }
    };
    if parsed.phrases.is_empty() {
        for (si, seg) in segs.iter().enumerate() {
            for local in 0..seg.total_docs() {
                if seg.is_live(local) {
                    consider(si, local, &mut scored);
                }
            }
        }
    } else {
        for &(si, local) in lexical_pool.keys() {
            consider(si, local, &mut scored);
        }
    }

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(pool);
    Some(scored.into_iter().map(|(a, _)| a).collect())
}

/// Weighted Reciprocal Rank Fusion: combine several rankings into one. An item's
/// fused score is Σ wᵢ/(K + rankᵢ) over the rankings it appears in (rank 1-based).
/// Robust and parameter-light — no score normalization needed; the per-ranking
/// weight lets us emphasize one signal (e.g. semantic) over another.
fn rrf_weighted<T: Copy + Eq + std::hash::Hash>(
    rankings: &[(Vec<T>, f64)],
    k_const: f64,
) -> Vec<(T, f64)> {
    let mut fused: HashMap<T, f64> = HashMap::new();
    for (ranking, weight) in rankings {
        for (i, &item) in ranking.iter().enumerate() {
            *fused.entry(item).or_insert(0.0) += weight / (k_const + (i + 1) as f64);
        }
    }
    let mut out: Vec<(T, f64)> = fused.into_iter().collect();
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    out
}

#[cfg(test)]
fn rrf<T: Copy + Eq + std::hash::Hash>(rankings: &[Vec<T>], k_const: f64) -> Vec<(T, f64)> {
    let weighted: Vec<(Vec<T>, f64)> = rankings.iter().map(|r| (r.clone(), 1.0)).collect();
    rrf_weighted(&weighted, k_const)
}

/// Proximity bonus for a doc: find the smallest window of text covering one
/// occurrence of each *present* query term; tighter clusters score higher. The
/// bonus is scaled by the matched terms' IDF so clustering rare terms matters
/// more. Returns `None` if fewer than two query terms occur in the doc.
fn proximity_bonus(
    seg: &Segment,
    doc_id: usize,
    terms: &[String],
    idf: &HashMap<&str, f64>,
) -> Option<f64> {
    // Positions are decoded on demand (owned), so keep them alive while we slice.
    let mut owned: Vec<Vec<u32>> = Vec::new();
    let mut idf_sum = 0.0;
    for term in terms {
        if let Some(pos) = seg.positions(term, doc_id) {
            idf_sum += idf[term.as_str()];
            owned.push(pos);
        }
    }
    if owned.len() < 2 {
        return None;
    }
    let lists: Vec<&[u32]> = owned.iter().map(|v| v.as_slice()).collect();
    let span = min_cover_span(&lists)?;
    // span is the distance spanned by one of each term; +1 so adjacent terms
    // (span == terms-1) give the strongest, finite bonus.
    Some(PROXIMITY_WEIGHT * idf_sum / (1.0 + span as f64))
}

/// Smallest `max - min` window containing at least one position from every list.
/// Classic merge-and-slide over all positions tagged with their list id.
fn min_cover_span(lists: &[&[u32]]) -> Option<u32> {
    let k = lists.len();
    // Merge all (position, list_id) pairs, ascending by position.
    let mut all: Vec<(u32, usize)> = Vec::new();
    for (id, list) in lists.iter().enumerate() {
        for &p in *list {
            all.push((p, id));
        }
    }
    if all.is_empty() {
        return None;
    }
    all.sort_unstable();

    let mut have = vec![0usize; k];
    let mut distinct = 0;
    let mut best = u32::MAX;
    let mut left = 0;
    for right in 0..all.len() {
        if have[all[right].1] == 0 {
            distinct += 1;
        }
        have[all[right].1] += 1;
        // Shrink from the left while the window still covers all k lists.
        while distinct == k {
            best = best.min(all[right].0 - all[left].0);
            have[all[left].1] -= 1;
            if have[all[left].1] == 0 {
                distinct -= 1;
            }
            left += 1;
        }
    }
    (best != u32::MAX).then_some(best)
}

/// True if `doc_id` contains `phrase` with the recorded relative offsets, using
/// positions. `phrase` is `(term, offset)` normalized so the first term is at 0.
fn contains_phrase(seg: &Segment, doc_id: usize, phrase: &[(String, u32)]) -> bool {
    let mut positions: Vec<(Vec<u32>, u32)> = Vec::with_capacity(phrase.len());
    for (term, offset) in phrase {
        match seg.positions(term, doc_id) {
            Some(pos) => positions.push((pos, *offset)),
            None => return false,
        }
    }

    'anchor: for &start in &positions[0].0 {
        for (pos_list, offset) in positions.iter().skip(1) {
            let want = start + *offset;
            if pos_list.binary_search(&want).is_err() {
                continue 'anchor;
            }
        }
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrf_rewards_agreement() {
        // Doc 5 is #1 in both rankings → it should win the fusion.
        let fused = rrf(&[vec![5, 1, 2], vec![5, 3, 4]], 60.0);
        assert_eq!(fused[0].0, 5);
        // Every input doc appears in the fused output exactly once.
        assert_eq!(fused.len(), 5);
    }

    #[test]
    fn decode_entities_handles_named_numeric_and_passthrough() {
        // Named, numeric decimal, and hex entities all decode.
        assert_eq!(
            decode_entities("Tim &amp; Harry&rsquo;s &#8212; done &#x2014;"),
            "Tim & Harry\u{2019}s \u{2014} done \u{2014}"
        );
        // A bare ampersand and an unknown/over-long entity pass through untouched.
        assert_eq!(decode_entities("a & b"), "a & b");
        assert_eq!(decode_entities("&notanentity;"), "&notanentity;");
        // No ampersand → cheap identity.
        assert_eq!(decode_entities("plain text"), "plain text");
    }
}
