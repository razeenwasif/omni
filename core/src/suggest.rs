//! Omnibox autocomplete: prefix completion over the indexed vocabulary.
//!
//! As the user types, Flux's omnibox hits `/ac?q=...` and expects query
//! completions. We complete the **last token** of the query against a sorted
//! dictionary of words drawn from document titles (human-readable, high-signal),
//! and splice each completion back onto the rest of the query — so `rust owne`
//! suggests `rust ownership`.
//!
//! A title vocabulary is a pragmatic source that always yields readable
//! suggestions; a real query log (ranked by popularity) is the next step.

use crate::analyze;
use crate::index::Index;
use std::collections::BTreeSet;

/// Sorted, de-duplicated completion dictionary.
pub struct Suggester {
    words: Vec<String>,
}

/// Max suggestions returned per request.
const MAX_SUGGESTIONS: usize = 8;

impl Suggester {
    /// Build the dictionary from document titles: lowercase word tokens, no
    /// stop-words, length ≥ 2. Sorted so completion is a binary-search range.
    pub fn build(index: &Index) -> Self {
        let mut set: BTreeSet<String> = BTreeSet::new();
        for seg in index.segments() {
            for doc in &seg.docs {
                if doc.deleted {
                    continue;
                }
                for word in analyze::tokenize(&doc.title) {
                    if word.len() >= 2 && !analyze::is_stopword(&word) {
                        set.insert(word);
                    }
                }
            }
        }
        Suggester {
            words: set.into_iter().collect(),
        }
    }

    /// Complete the last token of `q`. Returns full-query suggestions (the
    /// untouched prefix of `q` plus each completed last token). Empty if the
    /// query is blank or ends in whitespace (nothing to complete yet).
    pub fn complete(&self, q: &str) -> Vec<String> {
        let last = match q.split_whitespace().next_back() {
            Some(t) if !q.ends_with(char::is_whitespace) => t,
            _ => return Vec::new(),
        };
        let prefix = last.to_lowercase();
        // The text before the token we're completing (preserves spacing/case).
        let head = &q[..q.len() - last.len()];

        let start = self.words.partition_point(|w| w.as_str() < prefix.as_str());
        let mut out = Vec::new();
        for w in &self.words[start..] {
            if !w.starts_with(&prefix) {
                break;
            }
            if w.as_str() == prefix {
                continue; // already fully typed
            }
            out.push(format!("{head}{w}"));
            if out.len() >= MAX_SUGGESTIONS {
                break;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> Index {
        let mut idx = Index::new();
        idx.add_document("a".into(), "Closures and Ownership".into(), "body");
        idx.add_document("b".into(), "Closing Channels".into(), "body");
        idx.add_document("c".into(), "Traits".into(), "body");
        idx
    }

    #[test]
    fn completes_last_token() {
        let s = Suggester::build(&index());
        let got = s.complete("clo");
        // "closures", "closing" both start with "clo" (sorted).
        assert!(got.contains(&"closing".to_string()));
        assert!(got.contains(&"closures".to_string()));
    }

    #[test]
    fn preserves_query_prefix() {
        let s = Suggester::build(&index());
        let got = s.complete("rust owne");
        assert_eq!(got, vec!["rust ownership".to_string()]);
    }

    #[test]
    fn blank_or_trailing_space_yields_nothing() {
        let s = Suggester::build(&index());
        assert!(s.complete("").is_empty());
        assert!(s.complete("traits ").is_empty());
    }

    #[test]
    fn unknown_prefix_yields_nothing() {
        let s = Suggester::build(&index());
        assert!(s.complete("zzz").is_empty());
    }
}
