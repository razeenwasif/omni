//! Conservative "Did you mean" query correction.
//!
//! Corrections are drawn from the indexed corpus itself, so Omni suggests terms
//! it can actually search. The server only shows a suggestion after verifying the
//! corrected query returns results.

use crate::analyze;
use crate::index::Index;
use std::collections::HashMap;

pub fn correction(index: &Index, query: &str) -> Option<String> {
    let tokens = analyze::tokenize(query);
    if tokens.is_empty() || tokens.len() > 8 {
        return None;
    }

    let vocab = vocabulary(index);
    if vocab.is_empty() {
        return None;
    }

    let mut changed = false;
    let mut corrected = Vec::with_capacity(tokens.len());
    for token in tokens {
        if token.len() < 4 || analyze::is_stopword(&token) || vocab.contains_key(&token) {
            corrected.push(token);
            continue;
        }
        match best_term(&token, &vocab) {
            Some(best) => {
                changed = true;
                corrected.push(best);
            }
            None => corrected.push(token),
        }
    }

    changed.then(|| corrected.join(" "))
}

fn vocabulary(index: &Index) -> HashMap<String, u32> {
    let mut vocab = HashMap::new();
    for seg in index.segments() {
        for doc in &seg.docs {
            if doc.deleted {
                continue;
            }
            for token in analyze::tokenize(&doc.title) {
                if token.len() >= 3 && !analyze::is_stopword(&token) {
                    *vocab.entry(token).or_insert(0) += 3;
                }
            }
            for token in analyze::tokenize(&doc.url) {
                if token.len() >= 3 && !analyze::is_stopword(&token) {
                    *vocab.entry(token).or_insert(0) += 1;
                }
            }
        }
    }
    vocab
}

fn best_term(token: &str, vocab: &HashMap<String, u32>) -> Option<String> {
    let mut best: Option<(usize, u32, String)> = None;
    for (term, &freq) in vocab {
        let len_diff = token.len().abs_diff(term.len());
        if len_diff > 2 {
            continue;
        }
        let Some(dist) = edit_distance_at_most(token, term, 2) else {
            continue;
        };
        if dist == 0 || dist > 2 {
            continue;
        }
        let item = (dist, u32::MAX - freq, term.clone());
        if best.as_ref().map(|b| item < *b).unwrap_or(true) {
            best = Some(item);
        }
    }
    best.map(|(_, _, term)| term)
}

fn edit_distance_at_most(a: &str, b: &str, max: usize) -> Option<usize> {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > max {
        return None;
    }

    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        let mut row_min = cur[0];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
            row_min = row_min.min(cur[j + 1]);
        }
        if row_min > max {
            return None;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    (prev[b.len()] <= max).then_some(prev[b.len()])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> Index {
        let mut idx = Index::new();
        idx.add_document(
            "https://doc.rust-lang.org/book/ownership.html".into(),
            "Rust ownership and borrowing".into(),
            "body",
        );
        idx.add_document(
            "https://example.com/search-engine".into(),
            "Search engine ranking".into(),
            "body",
        );
        idx
    }

    #[test]
    fn corrects_against_index_vocabulary() {
        assert_eq!(
            correction(&index(), "rust ownrship"),
            Some("rust ownership".to_string())
        );
        assert_eq!(
            correction(&index(), "serch engne"),
            Some("search engine".to_string())
        );
    }

    #[test]
    fn leaves_known_or_far_terms_alone() {
        assert_eq!(correction(&index(), "rust ownership"), None);
        assert_eq!(correction(&index(), "zzzz qqqq"), None);
    }

    #[test]
    fn edit_distance_is_bounded() {
        assert_eq!(edit_distance_at_most("ownrship", "ownership", 2), Some(1));
        assert_eq!(edit_distance_at_most("abcd", "wxyz", 2), None);
    }
}
