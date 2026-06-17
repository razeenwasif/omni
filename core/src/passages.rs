//! Splitting a document's stored text into **passages** for passage-level dense
//! retrieval. Whole-doc embeddings dilute a long page into one averaged vector;
//! embedding each passage instead lets a doc's relevant paragraph be found on its
//! own (its semantic score becomes the *best* passage's — max-pooling). This is
//! also the unit a reranker and a future RAG answer mode operate on.

/// Default passage size (words), overlap between adjacent windows, and per-doc cap.
/// ~150 words ≈ a paragraph and sits well under `nomic-embed-text`'s context; the
/// cap bounds embed cost. `OVERLAP` shares words across a window boundary so a
/// sentence straddling two windows isn't split out of both — 0 keeps the original
/// contiguous behavior (the proven default until overlap is measured to help).
pub const WORDS_PER: usize = 150;
pub const OVERLAP: usize = 0;
pub const MAX_PASSAGES: usize = 6;

/// Build-time chunk parameters, overridable for tuning experiments via the env
/// (`OMNI_WORDS_PER`, `OMNI_OVERLAP`, `OMNI_MAX_PASSAGES`). A whole-doc control,
/// for example, is `OMNI_MAX_PASSAGES=1 OMNI_WORDS_PER=100000` (one passage
/// spanning the doc). Query time re-chunks with these same params to recover a
/// passage's text, so build and serve must agree — keep the env consistent (or
/// rely on the defaults).
pub fn params() -> (usize, usize, usize) {
    let words_per = std::env::var("OMNI_WORDS_PER")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(WORDS_PER);
    let overlap = std::env::var("OMNI_OVERLAP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(OVERLAP);
    let max = std::env::var("OMNI_MAX_PASSAGES")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(MAX_PASSAGES);
    (words_per, overlap, max)
}

/// Split `text` into up to `max` passages of about `words_per` words each, with
/// adjacent windows sharing `overlap` words (the stride is `words_per - overlap`).
/// `overlap == 0` gives contiguous, non-overlapping windows. Empty text → no
/// passages.
pub fn chunk(text: &str, words_per: usize, overlap: usize, max: usize) -> Vec<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() || words_per == 0 || max == 0 {
        return Vec::new();
    }
    let stride = words_per.saturating_sub(overlap).max(1);
    let mut out = Vec::new();
    let mut i = 0;
    while i < words.len() && out.len() < max {
        let end = (i + words_per).min(words.len());
        out.push(words[i..end].join(" "));
        if end >= words.len() {
            break; // the window already reached the end — no duplicate tail
        }
        i += stride;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_into_capped_windows() {
        let text = (1..=1000)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let ps = chunk(&text, 150, 0, 6);
        assert_eq!(ps.len(), 6, "capped at max");
        assert_eq!(ps[0].split_whitespace().count(), 150);
        // Windows are contiguous and non-overlapping.
        assert!(ps[0].starts_with("1 2 3"));
        assert!(ps[1].starts_with("151 152"));
    }

    #[test]
    fn overlap_shares_words_across_the_boundary() {
        let text = (1..=1000)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        // 150-word windows, 30-word overlap → stride 120: window 2 starts at 121.
        let ps = chunk(&text, 150, 30, 6);
        assert_eq!(ps[0].split_whitespace().count(), 150);
        assert!(ps[0].starts_with("1 2 3"));
        assert!(ps[1].starts_with("121 122"), "got {:?}", &ps[1][..12]);
        // The tail of window 1 reappears at the head of window 2 (the overlap).
        assert!(ps[0].split_whitespace().last() == Some("150"));
        assert!(ps[1].split_whitespace().nth(29) == Some("150"));
    }

    #[test]
    fn short_text_one_passage_empty_none() {
        assert_eq!(chunk("just a few words", 150, 0, 6).len(), 1);
        assert_eq!(chunk("just a few words", 150, 30, 6).len(), 1);
        assert!(chunk("", 150, 0, 6).is_empty());
        assert!(chunk("   ", 150, 0, 6).is_empty());
    }
}
