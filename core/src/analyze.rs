//! Text analysis: tokenization, stop-words, and stemming.
//!
//! The analyzer is the bridge between raw text and index terms, and it must be
//! applied **identically at index time and query time** — otherwise a query for
//! "running" would never find a document indexed under "run". The pipeline is:
//!
//!   raw text → tokenize → drop stop-words → stem → index terms
//!
//! **Stop-words** are ultra-common words (the, of, and…) that carry little
//! discriminating signal; dropping them shrinks the index and speeds queries.
//! **Stemming** collapses morphological variants to a common root (connect,
//! connected, connecting, connection → connect) so they match each other.
//!
//! We keep each kept term's **original token position** (the offset in the full
//! token stream, stop-words included), so phrase queries stay accurate across
//! removed stop-words — see `query.rs`.

/// A common English stop-word list. Small and deliberately conservative; an
/// over-aggressive list hurts queries like "to be or not to be".
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "from", "if", "in", "into",
    "is", "it", "no", "not", "of", "on", "or", "such", "that", "the", "their", "then", "there",
    "these", "they", "this", "to", "was", "we", "will", "with", "you", "your",
];

pub fn is_stopword(term: &str) -> bool {
    STOPWORDS.binary_search(&term).is_ok()
}

/// Tokenizer: lowercase, split on non-alphanumeric, drop empties. This is the
/// *raw* token stream — no stop-word/stem filtering — used for positions and for
/// the display title.
pub fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect()
}

/// Analyze text into `(term, position)` pairs: stemmed, stop-words removed, but
/// each surviving term tagged with its position in the *raw* token stream.
pub fn analyze_positions(text: &str) -> Vec<(String, u32)> {
    tokenize(text)
        .into_iter()
        .enumerate()
        .filter(|(_, tok)| !is_stopword(tok))
        .map(|(pos, tok)| (stem(&tok), pos as u32))
        .collect()
}

/// Analyze text into just its index terms (stemmed, stop-words removed). Used
/// for query free terms and title-term sets.
pub fn analyze_terms(text: &str) -> Vec<String> {
    tokenize(text)
        .into_iter()
        .filter(|tok| !is_stopword(tok))
        .map(|tok| stem(&tok))
        .collect()
}

// ---- Porter stemmer --------------------------------------------------------
//
// A faithful implementation of the Porter (1980) algorithm. It strips suffixes
// in five ordered phases, each guarded by conditions on the stem's "measure"
// (its count of vowel→consonant sequences) so we don't over-stem short words.
// Only applied to all-alphabetic tokens of length > 2.

/// Stem an English word. Non-alphabetic or very short tokens pass through.
pub fn stem(word: &str) -> String {
    if word.len() <= 2 || !word.bytes().all(|b| b.is_ascii_lowercase()) {
        return word.to_string();
    }
    let mut w: Vec<u8> = word.bytes().collect();
    step1a(&mut w);
    step1b(&mut w);
    step1c(&mut w);
    step2(&mut w);
    step3(&mut w);
    step4(&mut w);
    step5(&mut w);
    String::from_utf8(w).unwrap_or_else(|_| word.to_string())
}

fn is_cons(w: &[u8], i: usize) -> bool {
    match w[i] {
        b'a' | b'e' | b'i' | b'o' | b'u' => false,
        b'y' => i == 0 || !is_cons(w, i - 1),
        _ => true,
    }
}

/// Measure: the number of vowel→consonant transitions (the `m` in Porter).
fn measure(w: &[u8]) -> usize {
    let mut n = 0;
    let mut prev_vowel = false;
    for i in 0..w.len() {
        let vowel = !is_cons(w, i);
        if prev_vowel && !vowel {
            n += 1;
        }
        prev_vowel = vowel;
    }
    n
}

fn has_vowel(w: &[u8]) -> bool {
    (0..w.len()).any(|i| !is_cons(w, i))
}

fn double_cons_end(w: &[u8]) -> bool {
    let n = w.len();
    n >= 2 && w[n - 1] == w[n - 2] && is_cons(w, n - 1)
}

/// Ends consonant-vowel-consonant where the final consonant isn't w, x, or y.
fn cvc(w: &[u8]) -> bool {
    let n = w.len();
    n >= 3
        && is_cons(w, n - 3)
        && !is_cons(w, n - 2)
        && is_cons(w, n - 1)
        && !matches!(w[n - 1], b'w' | b'x' | b'y')
}

fn ends_with(w: &[u8], suf: &str) -> bool {
    w.ends_with(suf.as_bytes())
}

/// Replace suffix `suf` with `rep` (unconditionally).
fn set_suffix(w: &mut Vec<u8>, suf: &str, rep: &str) {
    w.truncate(w.len() - suf.len());
    w.extend_from_slice(rep.as_bytes());
}

/// Stem (the part before `suf`) has measure satisfying `min`.
fn stem_measure_at_least(w: &[u8], suf: &str, min: usize) -> bool {
    measure(&w[..w.len() - suf.len()]) >= min
}

fn step1a(w: &mut Vec<u8>) {
    if ends_with(w, "sses") {
        set_suffix(w, "sses", "ss");
    } else if ends_with(w, "ies") {
        set_suffix(w, "ies", "i");
    } else if ends_with(w, "ss") {
        // keep
    } else if ends_with(w, "s") {
        set_suffix(w, "s", "");
    }
}

fn step1b(w: &mut Vec<u8>) {
    if ends_with(w, "eed") {
        if stem_measure_at_least(w, "eed", 1) {
            set_suffix(w, "eed", "ee");
        }
        return;
    }
    let mut removed = false;
    if ends_with(w, "ed") && has_vowel(&w[..w.len() - 2]) {
        set_suffix(w, "ed", "");
        removed = true;
    } else if ends_with(w, "ing") && has_vowel(&w[..w.len() - 3]) {
        set_suffix(w, "ing", "");
        removed = true;
    }
    if removed {
        if ends_with(w, "at") || ends_with(w, "bl") || ends_with(w, "iz") {
            w.push(b'e');
        } else if double_cons_end(w)
            && !(ends_with(w, "l") || ends_with(w, "s") || ends_with(w, "z"))
        {
            w.pop();
        } else if measure(w) == 1 && cvc(w) {
            w.push(b'e');
        }
    }
}

fn step1c(w: &mut Vec<u8>) {
    if ends_with(w, "y") && has_vowel(&w[..w.len() - 1]) {
        let n = w.len();
        w[n - 1] = b'i';
    }
}

/// Apply the first matching (suffix → replacement) pair whose stem has m > 0.
fn replace_table(w: &mut Vec<u8>, table: &[(&str, &str)], min: usize) {
    for &(suf, rep) in table {
        if ends_with(w, suf) {
            if stem_measure_at_least(w, suf, min) {
                set_suffix(w, suf, rep);
            }
            return; // Porter applies only the longest/first matching rule
        }
    }
}

fn step2(w: &mut Vec<u8>) {
    const T: &[(&str, &str)] = &[
        ("ational", "ate"),
        ("tional", "tion"),
        ("enci", "ence"),
        ("anci", "ance"),
        ("izer", "ize"),
        ("abli", "able"),
        ("alli", "al"),
        ("entli", "ent"),
        ("eli", "e"),
        ("ousli", "ous"),
        ("ization", "ize"),
        ("ation", "ate"),
        ("ator", "ate"),
        ("alism", "al"),
        ("iveness", "ive"),
        ("fulness", "ful"),
        ("ousness", "ous"),
        ("aliti", "al"),
        ("iviti", "ive"),
        ("biliti", "ble"),
    ];
    replace_table(w, T, 1);
}

fn step3(w: &mut Vec<u8>) {
    const T: &[(&str, &str)] = &[
        ("icate", "ic"),
        ("ative", ""),
        ("alize", "al"),
        ("iciti", "ic"),
        ("ical", "ic"),
        ("ful", ""),
        ("ness", ""),
    ];
    replace_table(w, T, 1);
}

fn step4(w: &mut Vec<u8>) {
    // (m>1) suffix → "" for these. "ion" only after s or t.
    const T: &[&str] = &[
        "al", "ance", "ence", "er", "ic", "able", "ible", "ant", "ement", "ment", "ent", "ou",
        "ism", "ate", "iti", "ous", "ive", "ize",
    ];
    for &suf in T {
        if ends_with(w, suf) {
            if stem_measure_at_least(w, suf, 2) {
                set_suffix(w, suf, "");
            }
            return;
        }
    }
    if ends_with(w, "ion") {
        let stem = &w[..w.len() - 3];
        if measure(stem) >= 2 && matches!(stem.last(), Some(b's') | Some(b't')) {
            set_suffix(w, "ion", "");
        }
    }
}

fn step5(w: &mut Vec<u8>) {
    // 5a: remove a trailing e if (m>1), or (m==1 and not cvc).
    if ends_with(w, "e") {
        let stem = &w[..w.len() - 1];
        let m = measure(stem);
        if m > 1 || (m == 1 && !cvc(stem)) {
            w.pop();
        }
    }
    // 5b: (m>1 and double consonant ending in l) → drop one l.
    if measure(w) > 1 && double_cons_end(w) && ends_with(w, "l") {
        w.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopwords_sorted_for_binary_search() {
        let mut sorted = STOPWORDS.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, STOPWORDS, "STOPWORDS must stay sorted");
    }

    #[test]
    fn porter_known_cases() {
        let cases = [
            ("caresses", "caress"),
            ("ponies", "poni"),
            ("cats", "cat"),
            ("running", "run"),
            ("happy", "happi"),
            ("relational", "relat"),
            ("conditional", "condit"),
            ("rationalization", "ration"),
            ("connection", "connect"),
            ("connected", "connect"),
            ("agreed", "agre"),
            ("plastered", "plaster"),
        ];
        for (input, expected) in cases {
            assert_eq!(stem(input), expected, "stem({input})");
        }
    }

    #[test]
    fn variants_collapse_to_same_stem() {
        let root = stem("connect");
        for w in ["connected", "connecting", "connection", "connections"] {
            assert_eq!(stem(w), root, "{w} should stem to {root}");
        }
    }

    #[test]
    fn positions_track_raw_stream_across_stopwords() {
        // "search the engine" → search@0, engine@2 (the@1 is a stop-word).
        let got = analyze_positions("search the engine");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].1, 0);
        assert_eq!(got[1].1, 2);
    }
}
