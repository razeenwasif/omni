//! BM25F — the fielded variant of BM25, the ranking function at the heart of
//! lexical search.
//!
//! Plain BM25 treats a document as one bag of words. **BM25F** recognizes that
//! a term in the *title* matters more than the same term buried in the *body*,
//! and that each field should be length-normalized against *its own* average
//! length (titles are short; bodies are long). The crucial subtlety: the fields
//! are combined **before** the term-frequency saturation, not after — otherwise
//! a strong title hit and a strong body hit would each saturate separately and
//! the weighting would misbehave.
//!
//! Per query term `t` and document `d`:
//!   weightedTf = Σ_field  w_field · tf_field / (1 - b_field + b_field·len_field/avg_field)
//!   score(t,d) = idf(t) · weightedTf / (k1 + weightedTf)
//! Document score = Σ over query terms of score(t,d).
//!
//! `w_field` boosts a field; `b_field` controls its length normalization. With a
//! single field and `w=1` this reduces to ordinary BM25.

/// BM25F hyperparameters. Defaults: title weighted 3× body, with the canonical
/// k1=1.2 and per-field b=0.75.
pub struct Bm25f {
    pub k1: f64,
    pub w_title: f64,
    pub w_body: f64,
    pub b_title: f64,
    pub b_body: f64,
}

impl Default for Bm25f {
    fn default() -> Self {
        Bm25f {
            k1: 1.2,
            w_title: 3.0,
            w_body: 1.0,
            b_title: 0.75,
            b_body: 0.75,
        }
    }
}

/// Per-field corpus statistics needed to normalize a document's field lengths.
#[derive(Clone, Copy)]
pub struct FieldStats {
    pub avg_title_len: f64,
    pub avg_body_len: f64,
}

impl Bm25f {
    /// Inverse document frequency with the standard BM25 smoothing.
    /// `df` = docs containing the term (in any field), `total` = corpus size.
    pub fn idf(&self, df: usize, total: usize) -> f64 {
        let df = df as f64;
        let total = total as f64;
        (((total - df + 0.5) / (df + 0.5)) + 1.0).ln()
    }

    /// Length-normalized, field-weighted term frequency for one field.
    fn field_component(&self, w: f64, b: f64, tf: u32, len: u32, avg: f64) -> f64 {
        if tf == 0 {
            return 0.0;
        }
        let norm = 1.0 - b + b * (len as f64 / avg);
        w * (tf as f64) / norm
    }

    /// The per-term BM25F contribution for one document.
    pub fn term_score(
        &self,
        idf: f64,
        tf_title: u32,
        tf_body: u32,
        len_title: u32,
        len_body: u32,
        stats: FieldStats,
    ) -> f64 {
        let weighted_tf = self.field_component(
            self.w_title,
            self.b_title,
            tf_title,
            len_title,
            stats.avg_title_len,
        ) + self.field_component(
            self.w_body,
            self.b_body,
            tf_body,
            len_body,
            stats.avg_body_len,
        );
        idf * (weighted_tf / (self.k1 + weighted_tf))
    }
}
