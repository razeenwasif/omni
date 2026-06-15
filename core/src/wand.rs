//! WAND (Weak-AND) — dynamic pruning for fast top-K retrieval, run **per
//! segment**. The caller (`query.rs`) supplies *globally* aggregated IDF (per
//! term) and field-length averages so scores are comparable across segments;
//! this module never computes collection stats itself. Returned `doc_id`s are
//! segment-local — the caller pairs them with the segment index.
//!
//! Naive scoring walks *every* posting of *every* query term. WAND skips the
//! documents that provably can't enter the top-K, returning the *exact same*
//! top-K. The idea:
//!   * Give each term an **upper bound** — the most it can contribute.
//!   * Keep the running **threshold** θ = the K-th best score so far.
//!   * Sort cursors by current doc id; the first where the cumulative bound
//!     exceeds θ is the **pivot** — the smallest doc that *could* beat θ.
//!
//! The production retriever is **Block-Max WAND** (further down): it stores
//! per-block score maxima so the bound for a candidate is its block's max — far
//! tighter — and skips whole blocks of the weak tail at once. Plain WAND is the
//! reference baseline.

use crate::score::{Bm25f, FieldStats};
use crate::segment::{Posting, Segment};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;

/// A scored document. `doc_id` is segment-local.
pub struct Scored {
    pub doc_id: usize,
    pub score: f64,
}

/// Retrieval work counters, for measuring/demonstrating pruning.
#[derive(Default)]
pub struct Stats {
    /// Documents fully scored (the expensive work).
    pub full_evaluations: usize,
    /// Main-loop iterations (block skipping keeps this far below one-per-posting).
    pub iterations: usize,
}

/// A min-heap entry; the heap evicts the lowest score when full.
struct HeapItem {
    score: f64,
    doc_id: usize,
}
impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.score == other.score && self.doc_id == other.doc_id
    }
}
impl Eq for HeapItem {}
impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.score
            .total_cmp(&other.score)
            .then(self.doc_id.cmp(&other.doc_id))
    }
}
impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// BM25F score of a single posting in `seg`, given the term's (global) idf.
fn posting_score(bm25: &Bm25f, stats: FieldStats, seg: &Segment, idf: f64, p: &Posting) -> f64 {
    let d = &seg.docs[p.doc_id];
    bm25.term_score(idf, p.tf_title, p.tf_body, d.len_title, d.len_body, stats)
}

// ---- plain WAND (reference baseline) ---------------------------------------

/// A cursor over one term's posting list (owns the decoded list).
#[allow(dead_code)] // plain WAND is the reference baseline; production uses Block-Max
struct Cursor {
    postings: Arc<[Posting]>,
    pos: usize,
    idf: f64,
    ub: f64,
}
#[allow(dead_code)]
impl Cursor {
    fn doc(&self) -> Option<usize> {
        self.postings.get(self.pos).map(|p| p.doc_id)
    }
    fn advance_to(&mut self, target: usize) {
        while self.pos < self.postings.len() && self.postings[self.pos].doc_id < target {
            self.pos += 1;
        }
    }
}

#[allow(dead_code)]
pub fn retrieve_bm25(
    seg: &Segment,
    terms: &[String],
    idf: &HashMap<&str, f64>,
    stats: FieldStats,
    k: usize,
) -> Vec<Scored> {
    retrieve_bm25_counted(seg, terms, idf, stats, k).0
}

#[allow(dead_code)]
pub fn retrieve_bm25_counted(
    seg: &Segment,
    terms: &[String],
    idf: &HashMap<&str, f64>,
    stats: FieldStats,
    k: usize,
) -> (Vec<Scored>, usize) {
    let mut full_evaluations = 0usize;
    if k == 0 {
        return (Vec::new(), full_evaluations);
    }
    let bm25 = Bm25f::default();

    let mut cursors: Vec<Cursor> = Vec::new();
    for term in terms {
        let (Some(postings), Some(&term_idf)) = (seg.postings(term), idf.get(term.as_str())) else {
            continue;
        };
        let ub = postings
            .iter()
            .map(|p| posting_score(&bm25, stats, seg, term_idf, p))
            .fold(0.0_f64, f64::max);
        cursors.push(Cursor {
            postings,
            pos: 0,
            idf: term_idf,
            ub,
        });
    }
    if cursors.is_empty() {
        return (Vec::new(), full_evaluations);
    }

    let mut heap: BinaryHeap<Reverse<HeapItem>> = BinaryHeap::new();
    let mut threshold = 0.0_f64;

    loop {
        let mut active: Vec<usize> = (0..cursors.len())
            .filter(|&i| cursors[i].doc().is_some())
            .collect();
        if active.is_empty() {
            break;
        }
        active.sort_by_key(|&i| cursors[i].doc().unwrap());

        let mut cum = 0.0;
        let mut pivot_rank = None;
        for (rank, &ci) in active.iter().enumerate() {
            cum += cursors[ci].ub;
            if cum > threshold {
                pivot_rank = Some(rank);
                break;
            }
        }
        let Some(prank) = pivot_rank else {
            break;
        };
        let pivot_doc = cursors[active[prank]].doc().unwrap();

        if cursors[active[0]].doc().unwrap() == pivot_doc {
            full_evaluations += 1;
            let mut score = 0.0;
            for &ci in &active {
                if cursors[ci].doc() == Some(pivot_doc) {
                    let p = &cursors[ci].postings[cursors[ci].pos];
                    score += posting_score(&bm25, stats, seg, cursors[ci].idf, p);
                    cursors[ci].pos += 1;
                }
            }
            push_top_k(&mut heap, k, score, pivot_doc);
            if heap.len() >= k {
                threshold = heap.peek().unwrap().0.score;
            }
        } else {
            let ci = active[..prank]
                .iter()
                .copied()
                .filter(|&i| cursors[i].doc() != Some(pivot_doc))
                .max_by(|&a, &b| cursors[a].ub.total_cmp(&cursors[b].ub))
                .unwrap();
            cursors[ci].advance_to(pivot_doc);
        }
    }

    (drain_sorted(heap), full_evaluations)
}

/// Insert `(score, doc_id)` into a bounded min-heap of size `k`.
fn push_top_k(heap: &mut BinaryHeap<Reverse<HeapItem>>, k: usize, score: f64, doc_id: usize) {
    if heap.len() < k {
        heap.push(Reverse(HeapItem { score, doc_id }));
    } else if score > heap.peek().unwrap().0.score {
        heap.pop();
        heap.push(Reverse(HeapItem { score, doc_id }));
    }
}

fn drain_sorted(heap: BinaryHeap<Reverse<HeapItem>>) -> Vec<Scored> {
    let mut out: Vec<Scored> = heap
        .into_iter()
        .map(|Reverse(h)| Scored {
            doc_id: h.doc_id,
            score: h.score,
        })
        .collect();
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

// ---- Block-Max WAND --------------------------------------------------------
//
// Plain WAND bounds each term by a single global maximum. Block-Max WAND stores
// the maximum score within each fixed-size *block* of a posting list, so the
// bound for a candidate is the max of just its block — far tighter when scores
// vary along the list. When the current blocks can't beat θ we skip the
// shallowest cursor past its whole block (jumping over weak tails), and a later
// high-scoring block is still found because we re-pivot rather than stop.

/// Postings per block. A power of two; 128 is a common choice.
const BLOCK_SIZE: usize = 128;

/// A cursor carrying per-block maxima for Block-Max WAND (owns its postings).
struct BlockCursor {
    postings: Arc<[Posting]>,
    pos: usize,
    idf: f64,
    ub: f64, // global upper bound (max over all blocks)
    block_max: Vec<f64>,
}
impl BlockCursor {
    fn doc(&self) -> Option<usize> {
        self.postings.get(self.pos).map(|p| p.doc_id)
    }
    fn advance_to(&mut self, target: usize) {
        while self.pos < self.postings.len() && self.postings[self.pos].doc_id < target {
            self.pos += 1;
        }
    }
    fn current_block_max(&self) -> f64 {
        self.block_max[self.pos / BLOCK_SIZE]
    }
    fn current_block_end(&self) -> usize {
        let block = self.pos / BLOCK_SIZE;
        let last = ((block + 1) * BLOCK_SIZE).min(self.postings.len()) - 1;
        self.postings[last].doc_id
    }
}

/// Block-Max WAND over one segment. Identical top-K to exhaustive scoring; less
/// work. Tombstoned docs are scored but never admitted to the top-K.
pub fn retrieve_bm25_blockmax(
    seg: &Segment,
    terms: &[String],
    idf: &HashMap<&str, f64>,
    stats: FieldStats,
    k: usize,
) -> Vec<Scored> {
    retrieve_bm25_blockmax_counted(seg, terms, idf, stats, k).0
}

pub fn retrieve_bm25_blockmax_counted(
    seg: &Segment,
    terms: &[String],
    idf: &HashMap<&str, f64>,
    stats: FieldStats,
    k: usize,
) -> (Vec<Scored>, Stats) {
    let mut wstats = Stats::default();
    if k == 0 {
        return (Vec::new(), wstats);
    }
    let bm25 = Bm25f::default();

    let mut cs: Vec<BlockCursor> = Vec::new();
    for term in terms {
        let (Some(postings), Some(&term_idf)) = (seg.postings(term), idf.get(term.as_str())) else {
            continue;
        };
        // Precompute the max score per block.
        let mut block_max = Vec::new();
        let mut i = 0;
        while i < postings.len() {
            let end = (i + BLOCK_SIZE).min(postings.len());
            let m = postings[i..end]
                .iter()
                .map(|p| posting_score(&bm25, stats, seg, term_idf, p))
                .fold(0.0_f64, f64::max);
            block_max.push(m);
            i = end;
        }
        let ub = block_max.iter().copied().fold(0.0_f64, f64::max);
        cs.push(BlockCursor {
            postings,
            pos: 0,
            idf: term_idf,
            ub,
            block_max,
        });
    }
    if cs.is_empty() {
        return (Vec::new(), wstats);
    }

    let mut heap: BinaryHeap<Reverse<HeapItem>> = BinaryHeap::new();
    let mut threshold = 0.0_f64;

    loop {
        wstats.iterations += 1;
        let mut active: Vec<usize> = (0..cs.len()).filter(|&i| cs[i].doc().is_some()).collect();
        if active.is_empty() {
            break;
        }
        active.sort_by_key(|&i| cs[i].doc().unwrap());

        // Pivot via *block* maxima (canonical BMW): accumulate each cursor's
        // current-block max until it exceeds θ.
        let mut cum = 0.0;
        let mut pivot_rank = None;
        for (rank, &ci) in active.iter().enumerate() {
            cum += cs[ci].current_block_max();
            if cum > threshold {
                pivot_rank = Some(rank);
                break;
            }
        }

        let Some(prank) = pivot_rank else {
            // No doc reachable in the current block configuration can beat θ.
            // Skip the cursor whose current block ends earliest past that block,
            // then re-pivot (a later block may still score high).
            let &shallow = active
                .iter()
                .min_by_key(|&&ci| cs[ci].current_block_end())
                .unwrap();
            let target = cs[shallow].current_block_end() + 1;
            cs[shallow].advance_to(target);
            continue;
        };
        let pivot_doc = cs[active[prank]].doc().unwrap();

        if cs[active[0]].doc().unwrap() == pivot_doc {
            wstats.full_evaluations += 1;
            let mut score = 0.0;
            for &ci in &active {
                if cs[ci].doc() == Some(pivot_doc) {
                    let p = &cs[ci].postings[cs[ci].pos];
                    score += posting_score(&bm25, stats, seg, cs[ci].idf, p);
                    cs[ci].pos += 1;
                }
            }
            // Tombstones are scored (keeps the math simple) but never admitted.
            if seg.is_live(pivot_doc) {
                push_top_k(&mut heap, k, score, pivot_doc);
                if heap.len() >= k {
                    threshold = heap.peek().unwrap().0.score;
                }
            }
        } else {
            let ci = active[..prank]
                .iter()
                .copied()
                .filter(|&i| cs[i].doc().unwrap() < pivot_doc)
                .max_by(|&a, &b| cs[a].ub.total_cmp(&cs[b].ub))
                .unwrap();
            cs[ci].advance_to(pivot_doc);
        }
    }

    (drain_sorted(heap), wstats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::Segment;

    /// Local idf map for a single segment (single-segment = global).
    fn idf_of<'a>(seg: &Segment, terms: &'a [String]) -> HashMap<&'a str, f64> {
        let bm = Bm25f::default();
        let n = seg.live_count();
        terms
            .iter()
            .map(|t| (t.as_str(), bm.idf(seg.term_df(t), n)))
            .collect()
    }

    fn stats_of(seg: &Segment) -> FieldStats {
        let live = seg.live_count() as f64;
        let tt = seg.total_title_len() as f64;
        let tb = seg.total_body_len() as f64;
        FieldStats {
            avg_title_len: if live == 0.0 || tt == 0.0 {
                1.0
            } else {
                tt / live
            },
            avg_body_len: if live == 0.0 || tb == 0.0 {
                1.0
            } else {
                tb / live
            },
        }
    }

    /// Exhaustive BM25F top-K, the ground truth for WAND.
    fn naive(seg: &Segment, terms: &[String], k: usize) -> Vec<(usize, f64)> {
        let bm25 = Bm25f::default();
        let idf = idf_of(seg, terms);
        let stats = stats_of(seg);
        let mut scores: HashMap<usize, f64> = HashMap::new();
        for term in terms {
            if let Some(postings) = seg.postings(term) {
                for p in postings.iter() {
                    let s = posting_score(&bm25, stats, seg, idf[term.as_str()], p);
                    *scores.entry(p.doc_id).or_insert(0.0) += s;
                }
            }
        }
        let mut v: Vec<(usize, f64)> = scores.into_iter().collect();
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then(a.0.cmp(&b.0)));
        v.truncate(k);
        v
    }

    fn bmw(seg: &Segment, terms: &[String], k: usize) -> Vec<Scored> {
        retrieve_bm25_blockmax(seg, terms, &idf_of(seg, terms), stats_of(seg), k)
    }

    fn corpus() -> Segment {
        let vocab = [
            "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta",
        ];
        let mut seg = Segment::new();
        for i in 0..200 {
            let mut body = String::new();
            for (j, w) in vocab.iter().enumerate() {
                let count = ((i * 7 + j * 13) % 5) as usize;
                for _ in 0..count {
                    body.push_str(w);
                    body.push(' ');
                }
            }
            seg.add_document(format!("doc{i}"), format!("Title {i}"), &body);
        }
        seg
    }

    #[test]
    fn wand_matches_exhaustive_top_k() {
        let seg = corpus();
        for query in [
            vec!["alpha".to_string()],
            vec!["alpha".to_string(), "beta".to_string()],
            vec!["gamma".to_string(), "delta".to_string(), "zeta".to_string()],
        ] {
            for k in [1usize, 5, 20] {
                let want = naive(&seg, &query, k);
                let got = retrieve_bm25(&seg, &query, &idf_of(&seg, &query), stats_of(&seg), k);
                assert_eq!(got.len(), want.len(), "len for {query:?} k={k}");
                for (g, w) in got.iter().zip(&want) {
                    assert!((g.score - w.1).abs() < 1e-9, "score {query:?} k={k}");
                }
            }
        }
    }

    #[test]
    fn empty_and_missing_terms() {
        let seg = corpus();
        let a = vec!["alpha".to_string()];
        assert!(retrieve_bm25(&seg, &a, &idf_of(&seg, &a), stats_of(&seg), 0).is_empty());
        let n = vec!["nonexistent".to_string()];
        assert!(retrieve_bm25(&seg, &n, &idf_of(&seg, &n), stats_of(&seg), 5).is_empty());
    }

    #[test]
    fn wand_prunes_work_on_selective_queries() {
        let mut seg = Segment::new();
        for i in 0..500 {
            let mut body = String::from("common common ");
            if i % 50 == 0 {
                body.push_str("rare rare rare rare rare ");
            }
            seg.add_document(format!("d{i}"), format!("T{i}"), &body);
        }
        let terms = vec!["common".to_string(), "rare".to_string()];
        let total_postings = 500 + 10;
        let (hits, full_evals) =
            retrieve_bm25_counted(&seg, &terms, &idf_of(&seg, &terms), stats_of(&seg), 5);
        assert_eq!(hits.len(), 5);
        assert!(
            full_evals < total_postings / 2,
            "expected heavy pruning, got {full_evals}"
        );
        let want = naive(&seg, &terms, 5);
        for (g, w) in hits.iter().zip(&want) {
            assert!((g.score - w.1).abs() < 1e-9);
        }
    }

    #[test]
    fn blockmax_matches_exhaustive_top_k() {
        let seg = corpus();
        for query in [
            vec!["alpha".to_string()],
            vec!["alpha".to_string(), "beta".to_string()],
            vec!["gamma".to_string(), "delta".to_string(), "zeta".to_string()],
        ] {
            for k in [1usize, 5, 20] {
                let want = naive(&seg, &query, k);
                let got = bmw(&seg, &query, k);
                assert_eq!(got.len(), want.len(), "len {query:?} k={k}");
                for (g, w) in got.iter().zip(&want) {
                    assert!((g.score - w.1).abs() < 1e-9, "score {query:?} k={k}");
                }
            }
        }
    }

    #[test]
    fn blockmax_prunes_more_than_plain_wand() {
        let mut seg = Segment::new();
        for i in 0..2000 {
            let tf = if i < 100 { 100 - i } else { 1 };
            let body = vec!["term"; tf].join(" ");
            seg.add_document(format!("d{i}"), format!("T{i}"), &body);
        }
        let terms = vec!["term".to_string()];
        let idf = idf_of(&seg, &terms);
        let st = stats_of(&seg);
        let (_, wand_evals) = retrieve_bm25_counted(&seg, &terms, &idf, st, 10);
        let (bmw_hits, bmw_stats) = retrieve_bm25_blockmax_counted(&seg, &terms, &idf, st, 10);
        let want = naive(&seg, &terms, 10);
        assert_eq!(bmw_hits.len(), want.len());
        for (g, w) in bmw_hits.iter().zip(&want) {
            assert!((g.score - w.1).abs() < 1e-9);
        }
        assert!(
            bmw_stats.full_evaluations < wand_evals,
            "block-max {} vs WAND {wand_evals}",
            bmw_stats.full_evaluations
        );
        assert!(bmw_stats.iterations < 300, "got {}", bmw_stats.iterations);
    }

    #[test]
    fn blockmax_handles_low_then_high_lists() {
        let mut seg = Segment::new();
        for i in 0..3000 {
            let tf = if i >= 2950 { 50 } else { 1 };
            let body = vec!["signal"; tf].join(" ");
            seg.add_document(format!("d{i}"), String::new(), &body);
        }
        let q = vec!["signal".to_string()];
        let got = bmw(&seg, &q, 10);
        let want = naive(&seg, &q, 10);
        assert_eq!(got.len(), 10);
        for (g, w) in got.iter().zip(&want) {
            assert!((g.score - w.1).abs() < 1e-9);
        }
        assert!(
            got.iter().all(|s| s.doc_id >= 2950),
            "must surface the strong tail"
        );
    }

    #[test]
    fn blockmax_skips_long_tail_in_few_iterations() {
        let mut seg = Segment::new();
        for i in 0..5000 {
            let tf = if i < 50 { 50 - (i % 50) } else { 1 };
            let body = vec!["widget"; tf].join(" ");
            seg.add_document(format!("d{i}"), String::new(), &body);
        }
        let q = vec!["widget".to_string()];
        let (hits, st) =
            retrieve_bm25_blockmax_counted(&seg, &q, &idf_of(&seg, &q), stats_of(&seg), 10);
        let want = naive(&seg, &q, 10);
        for (g, w) in hits.iter().zip(&want) {
            assert!((g.score - w.1).abs() < 1e-9);
        }
        assert!(
            st.iterations < 200,
            "expected block-rate iterations, got {}",
            st.iterations
        );
    }
}
