//! Merge policy — decide *which* segments to combine so the segment count stays
//! bounded, without rewriting the whole index on every update.
//!
//! The tension is the classic LSM/Lucene one. An incremental update appends a
//! small new segment and never touches the old ones (cheap writes), but search
//! cost grows with the segment count, and tombstoned space is only reclaimed by a
//! merge. Compacting *everything* after each update would fix that but costs
//! O(index) per update — defeating the point of incremental segments.
//!
//! The middle ground is a **tiered (log-size) policy** (Lucene's `LogMergePolicy`,
//! Tantivy's default): bucket segments into size *levels* on a log scale, and once
//! a level holds at least `merge_factor` segments, merge the smallest of them into
//! one. The merged result lands in a higher level, so similarly-sized segments are
//! combined repeatedly — like leveled compaction. This keeps the segment count at
//! ~ `merge_factor · log(N)` while each document is rewritten only O(log N) times
//! over its life, giving amortized-linear total merge work.
//!
//! This module only makes the *decision* (`pick`); `Index::maybe_merge` executes
//! it by calling `Index::merge_segments`.

use std::collections::BTreeMap;

/// Tunables for the tiered merge policy.
pub struct MergePolicy {
    /// Merge a level once it holds at least this many segments. Doubles as the log
    /// base for bucketing segments into size levels. Lucene's default is 10.
    pub merge_factor: usize,
    /// A segment this large (docs) is considered "big enough" and is never chosen
    /// for an automatic merge — re-merging it rewrites a lot for little gain.
    /// (A full `compact` still merges it.)
    pub max_merged_docs: usize,
}

impl Default for MergePolicy {
    fn default() -> Self {
        MergePolicy {
            merge_factor: 10,
            max_merged_docs: 500_000,
        }
    }
}

impl MergePolicy {
    /// A policy with the given merge factor (clamped to ≥ 2 — a factor of 1 would
    /// merge forever) and default size ceiling.
    pub fn new(merge_factor: usize) -> Self {
        MergePolicy {
            merge_factor: merge_factor.max(2),
            ..MergePolicy::default()
        }
    }

    /// The log-size level of a segment of `size` docs: `floor(log_factor(size))`.
    /// Segments in the same level are within a `merge_factor`× size band.
    fn level(&self, size: usize) -> u32 {
        if size <= 1 {
            return 0;
        }
        (size as f64).log(self.merge_factor as f64).floor() as u32
    }

    /// Choose the next group of segment indices to merge, given each segment's
    /// size (doc count), or `None` if the layout is already balanced.
    ///
    /// Strategy: bucket eligible segments by level, then take the **lowest** level
    /// that is over-full (≥ `merge_factor` segments) — merging the cheapest
    /// (smallest) segments first — and return up to `merge_factor` of them. The
    /// caller loops, so an over-full level is drained over successive picks and the
    /// merged results cascade upward.
    pub fn pick(&self, sizes: &[usize]) -> Option<Vec<usize>> {
        let mut by_level: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
        for (i, &s) in sizes.iter().enumerate() {
            if s == 0 || s >= self.max_merged_docs {
                continue; // empty (unused) or already big — leave it alone
            }
            by_level.entry(self.level(s)).or_default().push(i);
        }
        // BTreeMap iterates levels low→high, so we hit the smallest tier first.
        for (_level, mut idxs) in by_level {
            if idxs.len() >= self.merge_factor {
                idxs.sort_by_key(|&i| sizes[i]);
                idxs.truncate(self.merge_factor);
                return Some(idxs);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balanced_layout_needs_no_merge() {
        let p = MergePolicy::new(10);
        // One big + a couple small: no level is over-full.
        assert!(p.pick(&[1000, 5, 3]).is_none());
        assert!(p.pick(&[]).is_none());
        assert!(p.pick(&[42]).is_none());
    }

    #[test]
    fn over_full_small_tier_is_picked_smallest_first() {
        let p = MergePolicy::new(3); // merge once a level has 3
                                     // Five level-0 segments (size < 3) ⇒ pick the 3 smallest.
        let sizes = vec![2, 1, 2, 1, 100];
        let pick = p.pick(&sizes).expect("over-full level-0 tier");
        assert_eq!(pick.len(), 3);
        for &i in &pick {
            assert!(sizes[i] < 3, "only small segments chosen");
        }
    }

    #[test]
    fn huge_segments_are_left_alone() {
        let mut p = MergePolicy::new(2);
        p.max_merged_docs = 50;
        // Three big segments above the ceiling: never auto-merged.
        assert!(p.pick(&[100, 100, 100]).is_none());
        // …but small ones below it still merge.
        assert!(p.pick(&[100, 5, 6]).is_some());
    }

    #[test]
    fn repeated_picks_converge() {
        // Simulate the maybe_merge loop: keep merging until balanced. Must
        // terminate (the whole point of a log-size policy).
        let p = MergePolicy::new(4);
        let mut sizes = vec![1; 40]; // 40 tiny segments
        let mut rounds = 0;
        while let Some(sel) = p.pick(&sizes) {
            let merged: usize = sel.iter().map(|&i| sizes[i]).sum();
            // Remove merged segments (high→low to keep indices valid), add result.
            let mut s = sel.clone();
            s.sort_unstable();
            for &i in s.iter().rev() {
                sizes.remove(i);
            }
            sizes.push(merged);
            rounds += 1;
            assert!(rounds < 100, "merge loop must converge");
        }
        let total: usize = sizes.iter().sum();
        assert_eq!(total, 40, "no documents lost across merges");
        assert!(sizes.len() < 40, "segment count shrank");
    }
}
