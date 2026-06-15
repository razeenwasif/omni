//! Query-biased snippets with highlighting.
//!
//! A good result snippet shows the part of the document most relevant to the
//! query — the window of text where the query terms cluster — with those terms
//! emphasized. We slide a fixed-size window over the stored text, pick the
//! window containing the most query-term hits, and render it with matches
//! wrapped in `<mark>`. Matching is done on **stems**, so a search for "run"
//! highlights "running".
//!
//! The output is HTML-safe: every word is escaped, and only our own `<mark>`
//! tags are injected — so `server.rs` inserts the result without re-escaping.

use crate::analyze;
use std::collections::HashSet;

/// Words shown in a snippet window.
const WINDOW: usize = 32;

/// Build a highlighted snippet of `text` biased toward `stemmed_terms`.
pub fn make(text: &str, stemmed_terms: &HashSet<String>) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return String::new();
    }

    let matches: Vec<bool> = words
        .iter()
        .map(|w| word_matches(w, stemmed_terms))
        .collect();
    let n = words.len();
    let win = WINDOW.min(n);

    // Prefix sums of match flags → O(1) window match-count, O(n) best-window scan.
    let mut prefix = vec![0usize; n + 1];
    for i in 0..n {
        prefix[i + 1] = prefix[i] + matches[i] as usize;
    }
    let mut best_start = 0;
    let mut best_count = 0;
    for start in 0..=(n - win) {
        let count = prefix[start + win] - prefix[start];
        if count > best_count {
            best_count = count;
            best_start = start;
        }
    }

    let end = best_start + win;
    let mut out = String::new();
    if best_start > 0 {
        out.push_str("… ");
    }
    for (i, word) in words[best_start..end].iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        if matches[best_start + i] {
            out.push_str("<mark>");
            out.push_str(&html_escape(word));
            out.push_str("</mark>");
        } else {
            out.push_str(&html_escape(word));
        }
    }
    if end < n {
        out.push_str(" …");
    }
    out
}

/// True if any token in `word` stems to one of the query terms.
fn word_matches(word: &str, stemmed_terms: &HashSet<String>) -> bool {
    analyze::tokenize(word)
        .iter()
        .any(|tok| stemmed_terms.contains(&analyze::stem(tok)))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(ts: &[&str]) -> HashSet<String> {
        ts.iter().map(|t| analyze::stem(t)).collect()
    }

    #[test]
    fn highlights_stemmed_matches() {
        let text = "The crawler is running fast and indexing many pages quickly today";
        let snip = make(text, &terms(&["run"]));
        assert!(snip.contains("<mark>running</mark>"), "got: {snip}");
    }

    #[test]
    fn window_centers_on_query_cluster() {
        // The query word sits late in a long text; the snippet should reach it
        // and prefix an ellipsis rather than showing only the start.
        let mut words = vec!["filler"; 60];
        words.push("kangaroo");
        let text = words.join(" ");
        let snip = make(&text, &terms(&["kangaroo"]));
        assert!(snip.contains("<mark>kangaroo</mark>"));
        assert!(snip.starts_with("… "));
    }

    #[test]
    fn escapes_html() {
        let snip = make("a <script> tag here", &terms(&["tag"]));
        assert!(!snip.contains("<script>"));
        assert!(snip.contains("&lt;script&gt;"));
    }
}
