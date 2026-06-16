//! Generative answer mode (RAG) — synthesize a grounded answer from the best
//! passages of the top results, served at `GET /answer`.
//!
//! This is the heavier sibling of the extractive `Hit::answer` (query.rs). Where
//! that returns one best passage verbatim (free, always on), this asks a local LLM
//! to *compose* a short answer using **only** the retrieved passages and to cite
//! them with `[n]`. It's a model call per request (seconds + VRAM), so it's a
//! separate opt-in endpoint, never auto-run from the results page. Any failure
//! (model down, empty reply) returns `None`; the caller falls back to the
//! extractive answer.
//!
//! Grounding the model in retrieved passages (rather than its own weights) is the
//! whole point: answers stay tied to the indexed corpus and carry citations the
//! user can click, which is exactly what passage-level retrieval unlocked.

use crate::embed;

/// Default generation model. `gemma4:12b-it-qat` gives noticeably better grounded
/// answers than the tiny instruct models and sits ~9 GB on a 24 GB card with
/// headroom (it won't max VRAM). Override per request with `&model=`.
pub const DEFAULT_MODEL: &str = "gemma4:12b-it-qat";

/// How many top passages to ground the answer in. Enough for coverage, few enough
/// to keep the prompt tight and the model focused.
pub const CONTEXT_PASSAGES: usize = 5;

/// Generate a grounded answer to `query` from `passages` (each `(title, url,
/// passage_text)`, already ranked). Returns the answer text, or `None` on any
/// failure. `base` is an Ollama origin, e.g. `http://localhost:11434`.
pub fn generate(
    base: &str,
    model: &str,
    query: &str,
    passages: &[(String, String, String)],
) -> Option<String> {
    if passages.is_empty() || base.is_empty() {
        return None;
    }
    // `think:false` matters: gemma *-it-qat are reasoning models that otherwise
    // spend the token budget in a `thinking` field and leave `content` empty.
    let body = format!(
        "{{\"model\":{},\"messages\":[{{\"role\":\"user\",\"content\":{}}}],\
          \"stream\":false,\"think\":false,\"keep_alive\":\"5m\",\
          \"options\":{{\"temperature\":0.2,\"num_predict\":400}}}}",
        embed::json_str(model),
        embed::json_str(&build_prompt(query, passages)),
    );
    // Generation + a possibly-cold model ⇒ a generous read window.
    let resp = embed::http_post_json(&format!("{base}/api/chat"), &body, 180)?;
    let content = crate::json::extract_string_field(&resp, "content")?;
    let answer = content.trim();
    if answer.is_empty() {
        None
    } else {
        Some(answer.to_string())
    }
}

/// Like `generate`, but streams the answer **token by token** to `on_token` as the
/// model produces it (Ollama `/api/chat` with `"stream":true`). `on_token` returns
/// `false` to stop early (e.g. the client disconnected). Returns `true` if any
/// token was emitted. Same `think:false` requirement as `generate`.
pub fn generate_stream(
    base: &str,
    model: &str,
    query: &str,
    passages: &[(String, String, String)],
    mut on_token: impl FnMut(&str) -> bool,
) -> bool {
    if passages.is_empty() || base.is_empty() {
        return false;
    }
    let body = format!(
        "{{\"model\":{},\"messages\":[{{\"role\":\"user\",\"content\":{}}}],\
          \"stream\":true,\"think\":false,\"keep_alive\":\"5m\",\
          \"options\":{{\"temperature\":0.2,\"num_predict\":400}}}}",
        embed::json_str(model),
        embed::json_str(&build_prompt(query, passages)),
    );
    let mut got = false;
    embed::http_post_stream(&format!("{base}/api/chat"), &body, 180, |line| {
        // Each line: {"message":{"role":"assistant","content":"<delta>"},"done":...}.
        let mut keep = !line.contains("\"done\":true");
        if let Some(delta) = crate::json::extract_string_field(line, "content") {
            if !delta.is_empty() {
                got = true;
                if !on_token(&delta) {
                    keep = false;
                }
            }
        }
        keep
    });
    got
}

/// Build the grounding prompt: the question plus numbered sources, with strict
/// instructions to answer *only* from them and cite with `[n]`.
fn build_prompt(query: &str, passages: &[(String, String, String)]) -> String {
    let mut p = String::with_capacity(512 + passages.len() * 400);
    p.push_str(
        "You are Omni's answer assistant. Using ONLY the numbered sources below, write a \
         concise, direct answer (2-5 sentences) to the question. Cite the sources you use \
         inline with their number, like [1] or [2]. Do not use any outside knowledge. If the \
         sources do not contain the answer, say you don't have enough information.\n\n",
    );
    p.push_str("Question: ");
    p.push_str(query.trim());
    p.push_str("\n\nSources:\n");
    for (i, (title, _url, text)) in passages.iter().enumerate() {
        // Cap each passage so the prompt stays bounded for the local model.
        let t: String = text
            .split_whitespace()
            .take(180)
            .collect::<Vec<_>>()
            .join(" ");
        p.push_str(&format!("[{}] {}: {}\n", i + 1, title.trim(), t.trim()));
    }
    p.push_str("\nAnswer:");
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_context_yields_none() {
        assert!(generate("http://localhost:11434", DEFAULT_MODEL, "q", &[]).is_none());
        // An empty base (non-Ollama embedder) is also a no-op, not a bad request.
        let ctx = vec![("T".into(), "u".into(), "body".into())];
        assert!(generate("", DEFAULT_MODEL, "q", &ctx).is_none());
    }

    #[test]
    fn prompt_lists_numbered_sources_and_question() {
        let ctx = vec![
            ("Vec".into(), "u1".into(), "a growable array".into()),
            ("HashMap".into(), "u2".into(), "a key value map".into()),
        ];
        let p = build_prompt("rust collections", &ctx);
        assert!(p.contains("Question: rust collections"));
        assert!(p.contains("[1] Vec: a growable array"));
        assert!(p.contains("[2] HashMap: a key value map"));
        assert!(p.contains("ONLY the numbered sources"));
    }
}
