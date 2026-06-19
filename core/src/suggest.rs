//! Omnibox autocomplete: session-aware query suggestions plus indexed fallbacks.
//!
//! As the user types, Flux's omnibox hits `/ac?q=...` and expects query
//! completions. The best suggestions are full queries from the current Omni
//! session, ranked by frequency and recency. When the session has not learned
//! enough yet, we fall back to indexed title phrases and finally to the old
//! last-token title-word completion — so `rust owne` can still suggest
//! `rust ownership`.
//!
//! This intentionally stays in-memory and std-only. Persisting the query log
//! would be a small extension once the local privacy/storage policy is settled.

use crate::analyze;
use crate::index::Index;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::RwLock;

pub struct Suggester {
    state: RwLock<State>,
}

struct State {
    /// Sorted, de-duplicated title-word completion dictionary.
    words: Vec<String>,
    /// Normalized title phrases with their observed frequency.
    phrases: Vec<Phrase>,
    /// Queries issued during this server process.
    queries: HashMap<String, QueryStat>,
    clock: u64,
}

#[derive(Clone)]
struct Phrase {
    text: String,
    freq: u32,
}

#[derive(Clone)]
struct QueryStat {
    count: u32,
    last_seen: u64,
}

/// Max suggestions returned per request.
const MAX_SUGGESTIONS: usize = 8;
const MAX_SESSION_QUERIES: usize = 512;

impl Suggester {
    /// Build fallback dictionaries from document titles. Full title phrases
    /// produce useful suggestions for prefix queries; title words preserve the
    /// previous last-token completion behavior.
    pub fn build(index: &Index) -> Self {
        let mut words: BTreeSet<String> = BTreeSet::new();
        let mut phrases: BTreeMap<String, u32> = BTreeMap::new();
        for seg in index.segments() {
            for doc in &seg.docs {
                if doc.deleted {
                    continue;
                }
                for word in analyze::tokenize(&doc.title) {
                    if word.len() >= 2 && !analyze::is_stopword(&word) {
                        words.insert(word);
                    }
                }
                if let Some(phrase) = normalize_query(&doc.title) {
                    *phrases.entry(phrase).or_insert(0) += 1;
                }
            }
        }
        Suggester {
            state: RwLock::new(State {
                words: words.into_iter().collect(),
                phrases: phrases
                    .into_iter()
                    .map(|(text, freq)| Phrase { text, freq })
                    .collect(),
                queries: HashMap::new(),
                clock: 0,
            }),
        }
    }

    /// Record a user-issued search query for the current server session. Repeats
    /// increase the query's rank; newer queries win ties.
    pub fn record_query(&self, q: &str) {
        if q.trim_start().starts_with('!') {
            return; // bang navigation is not a search-intent suggestion.
        }
        let Some(q) = normalize_query(q) else {
            return;
        };

        let mut state = self.state.write().unwrap();
        state.clock += 1;
        let now = state.clock;
        let stat = state.queries.entry(q).or_insert(QueryStat {
            count: 0,
            last_seen: 0,
        });
        stat.count = stat.count.saturating_add(1);
        stat.last_seen = now;

        if state.queries.len() > MAX_SESSION_QUERIES {
            let remove = state
                .queries
                .iter()
                .min_by_key(|(_, s)| (s.count, s.last_seen))
                .map(|(q, _)| q.clone());
            if let Some(q) = remove {
                state.queries.remove(&q);
            }
        }
    }

    /// Complete `q`. Learned full-query suggestions are returned first; indexed
    /// title phrases and title-word completions fill the rest.
    pub fn complete(&self, q: &str) -> Vec<String> {
        let Some(prefix) = normalize_query(q) else {
            return Vec::new();
        };
        let state = self.state.read().unwrap();
        let mut ranked: Vec<(i64, String)> = Vec::new();

        for (query, stat) in &state.queries {
            if query.starts_with(&prefix) && query != &prefix {
                let score = 1_000_000 + (stat.count as i64 * 1_000) + stat.last_seen as i64;
                ranked.push((score, query.clone()));
            }
        }

        for phrase in &state.phrases {
            if phrase.text.starts_with(&prefix) && phrase.text != prefix {
                let score = 100_000 + phrase.freq as i64;
                ranked.push((score, phrase.text.clone()));
            }
        }

        // Preserve the old behavior as a low-priority fallback: complete the
        // final token from the title vocabulary and splice it back into the query.
        if !q.ends_with(char::is_whitespace) {
            if let Some(last) = q.split_whitespace().next_back() {
                let token_prefix = last.to_lowercase();
                let head = &q[..q.len() - last.len()];
                let start = state
                    .words
                    .partition_point(|w| w.as_str() < token_prefix.as_str());
                for w in &state.words[start..] {
                    if !w.starts_with(&token_prefix) {
                        break;
                    }
                    if w.as_str() == token_prefix {
                        continue;
                    }
                    ranked.push((10_000, format!("{head}{w}")));
                }
            }
        }

        ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for (_, item) in ranked {
            if seen.insert(item.clone()) {
                out.push(item);
                if out.len() >= MAX_SUGGESTIONS {
                    break;
                }
            }
        }
        out
    }
}

fn normalize_query(q: &str) -> Option<String> {
    let tokens: Vec<String> = analyze::tokenize(q)
        .into_iter()
        .filter(|t| t.len() >= 2 || t.chars().any(|c| c.is_ascii_digit()))
        .collect();
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" "))
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

    #[test]
    fn learned_queries_rank_before_title_fallbacks() {
        let s = Suggester::build(&index());
        s.record_query("closures borrow checker");
        s.record_query("closures borrow checker");
        s.record_query("closing channels in go");
        let got = s.complete("clo");
        assert_eq!(got[0], "closures borrow checker");
        assert!(got.contains(&"closing channels in go".to_string()));
    }

    #[test]
    fn learned_queries_can_complete_after_space() {
        let s = Suggester::build(&index());
        s.record_query("rust ownership");
        assert_eq!(s.complete("rust "), vec!["rust ownership".to_string()]);
    }

    #[test]
    fn title_phrase_prefixes_are_suggested() {
        let s = Suggester::build(&index());
        assert_eq!(
            s.complete("closures and"),
            vec!["closures and ownership".to_string()]
        );
    }
}
