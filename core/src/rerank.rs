//! Cross-encoder reranking via a local LLM (RankGPT-style **listwise** reranker).
//!
//! Hybrid retrieval (BM25F ⊕ dense, fused by RRF) is a fast *bi-encoder* stage:
//! query and documents are scored independently. The accuracy ceiling in modern
//! IR comes from a **cross-encoder** that reads the query and a passage *together*
//! and judges their relevance jointly. Omni's Ollama has no dedicated rerank
//! endpoint, so we use an instruction-tuned LLM as the cross-encoder: one
//! `/api/chat` call lists the top candidates and asks for them back in relevance
//! order (the RankGPT approach, which is competitive on BEIR).
//!
//! It's one extra model call per query (~seconds + VRAM), so it's **opt-in**
//! (`&rerank=1`) — a "deep search" mode, off by default. Any failure (model down,
//! unparseable reply) returns `None` and the caller keeps the hybrid order, so it
//! can only help, never break, a query.
//!
//! **Honest finding** (measured with `scripts/eval.py`, graded nDCG@10, on the
//! ~12k-doc corpus): a *locally-runnable* LLM reranker does **not** beat the
//! eval-tuned hybrid here. A small model (phi4-mini 3.8B) actively *hurt*
//! (0.67 → 0.34); mid/large instruct models (gemma4 e4b/12b) were *neutral*
//! (≈0.666, they echo the hybrid order). The hybrid is already strong enough that
//! a generative reranker adds nothing — a real gain needs a **distilled
//! cross-encoder** (e.g. bge-reranker via ONNX), which is a dependency Omni
//! doesn't carry. So this ships as pluggable scaffolding (swap `&rr_model=`, or a
//! future `/api/rerank`), not a default. The eval harness earning its keep.

use crate::embed;

/// Default reranker model when `&rerank=1` is set — the lightest local instruct
/// model that's at least *neutral* (a weaker one degrades results). Pluggable via
/// `&rr_model=`. Reranking is off unless explicitly requested.
pub const DEFAULT_MODEL: &str = "gemma4:e4b-it-qat";

/// How many top hybrid candidates to rerank. Larger than `k` so the reranker can
/// promote a relevant doc from below the fold into the visible results, but small
/// enough to keep the listwise task easy for a modest local model (and cheap).
pub const POOL: usize = 10;

/// Re-order `docs` (each `(title, snippet)`) by relevance to `query` using the
/// listwise LLM reranker at `base` (an Ollama origin, e.g. `http://localhost:11434`).
/// Returns a best-first permutation of `0..docs.len()`, or `None` on any failure.
pub fn order(
    base: &str,
    model: &str,
    query: &str,
    docs: &[(String, String)],
) -> Option<Vec<usize>> {
    if docs.len() < 2 {
        return None;
    }
    let body = format!(
        "{{\"model\":{},\"messages\":[{{\"role\":\"user\",\"content\":{}}}],\
          \"stream\":false,\"keep_alive\":\"5m\",\
          \"options\":{{\"temperature\":0,\"num_predict\":128}}}}",
        embed::json_str(model),
        embed::json_str(&build_prompt(query, docs)),
    );
    // Opt-in + possibly-cold model ⇒ a generous read window.
    let resp = embed::http_post_json(&format!("{base}/api/chat"), &body, 90)?;
    let content = crate::json::extract_string_field(&resp, "content")?;
    parse_order(&content, docs.len())
}

fn build_prompt(query: &str, docs: &[(String, String)]) -> String {
    let mut p = String::with_capacity(256 + docs.len() * 200);
    p.push_str("You are a search-result reranker. Rank the passages by how well they answer the query.\n\n");
    p.push_str("Query: ");
    p.push_str(query.trim());
    p.push_str("\n\nPassages:\n");
    for (i, (title, snippet)) in docs.iter().enumerate() {
        let s: String = snippet.chars().take(240).collect();
        p.push_str(&format!("[{}] {} — {}\n", i + 1, title.trim(), s.trim()));
    }
    p.push_str(
        "\nOutput ONLY the passage numbers from most to least relevant, comma-separated \
         (e.g. 3,1,4,2). Include every number exactly once. No other text.",
    );
    p
}

/// Parse the model's "3,1,4,2" reply into a 0-based permutation, leniently:
/// take in-range integers in order (ignoring duplicates and any prose), then
/// append any candidates the model omitted (original order) so nothing is dropped.
fn parse_order(reply: &str, n: usize) -> Option<Vec<usize>> {
    fn push_num(num: &mut String, order: &mut Vec<usize>, seen: &mut [bool]) {
        if let Ok(v) = num.parse::<usize>() {
            if v >= 1 && v <= seen.len() && !seen[v - 1] {
                seen[v - 1] = true;
                order.push(v - 1);
            }
        }
        num.clear();
    }
    let mut order = Vec::with_capacity(n);
    let mut seen = vec![false; n];
    let mut num = String::new();
    for c in reply.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else {
            push_num(&mut num, &mut order, &mut seen);
        }
    }
    push_num(&mut num, &mut order, &mut seen);
    if order.is_empty() {
        return None; // unparseable ⇒ keep the hybrid order
    }
    for (i, &done) in seen.iter().enumerate() {
        if !done {
            order.push(i);
        }
    }
    Some(order)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clean_permutation() {
        assert_eq!(parse_order("3,1,4,2", 4), Some(vec![2, 0, 3, 1]));
    }

    #[test]
    fn lenient_parse_fills_and_dedups() {
        // Prose around numbers, a duplicate, and a missing id (3) → appended last.
        assert_eq!(
            parse_order("Ranking: 2, then 1, then 2 again, and 4.", 4),
            Some(vec![1, 0, 3, 2])
        );
        // Out-of-range ids ignored.
        assert_eq!(parse_order("9,1,7,2", 3), Some(vec![0, 1, 2]));
        // No digits → None (keep original order).
        assert!(parse_order("I cannot rank these.", 3).is_none());
    }
}
