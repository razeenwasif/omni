//! A hot-swappable index for the running server, plus the background merger.
//!
//! The server loads the index once and serves it read-only, so on its own it
//! never reclaims tombstones or rebalances segments that accumulate (e.g. when an
//! index built with `--no-merge` is served, or after a future live-ingest path).
//! `LiveIndex` makes the served index **swappable while requests are in flight**:
//!
//!   * Readers take a cheap `Arc<Index>` **snapshot** — the lock is held only long
//!     enough to clone the `Arc`, never for the actual query — so a swap never
//!     blocks `/search` and a `/search` never blocks the swap.
//!   * The background merger builds a *new* index off a snapshot with
//!     `Index::merged_view` (which **shares** the untouched segments via `Arc` and
//!     only materializes the merged one) and then atomically replaces the pointer.
//!     In-flight readers keep using the old index until they drop their snapshot;
//!     it's freed when the last one finishes.
//!
//! This is the hand-rolled equivalent of an `arc-swap` cell — no external crate.
//! Unmerged segment files that `save`'s orphan cleanup removes may still be
//! memory-mapped by an old snapshot; on Linux unlinking a mapped file is safe (the
//! mapping stays valid until dropped), which is why the swap and the on-disk
//! cleanup can race harmlessly.

use crate::index::Index;
use crate::merge::MergePolicy;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};

/// A pointer to the current index that can be swapped atomically.
pub struct LiveIndex {
    inner: RwLock<Arc<Index>>,
}

impl LiveIndex {
    pub fn new(index: Index) -> Arc<Self> {
        Arc::new(LiveIndex {
            inner: RwLock::new(Arc::new(index)),
        })
    }

    /// The current index. The read lock is released immediately; callers run their
    /// query against the returned `Arc`, not under the lock.
    pub fn snapshot(&self) -> Arc<Index> {
        Arc::clone(&self.inner.read().unwrap())
    }

    /// Atomically replace the current index.
    pub fn swap(&self, next: Arc<Index>) {
        *self.inner.write().unwrap() = next;
    }
}

/// Persist `next` (if a dir is configured) and then atomically swap it in. The
/// shared commit path for the background merger and the `/ingest` endpoint — saving
/// before swapping keeps the on-disk index consistent with what's being served.
pub fn commit(live: &LiveIndex, next: Index, dir: Option<&std::path::Path>) {
    let next = Arc::new(next);
    if let Some(d) = dir {
        match crate::persist::save(&next, d) {
            Ok(()) => {
                let _ = crate::persist::save_ann(&next, d);
            }
            Err(e) => eprintln!("omni: commit save failed: {e}"),
        }
    }
    live.swap(next);
}

/// Serializes everything that *publishes* a new index, and remembers when the ANN
/// graph was last invalidated.
///
/// Two problems this solves, both learned the hard way:
///
///  * **Concurrency.** `/ingest` is called fire-and-forget by the browser for every
///    page it loads, so a browsing session issues them in bursts. Each one used to
///    run a full HNSW rebuild in its own connection thread with nothing bounding
///    the parallelism; a burst became ~140 simultaneous rebuilds, one index copy
///    each, ~30 GB of anonymous heap, and every core pinned. Ingests are now
///    single-file: the second one waits rather than duplicating the work.
///  * **Lost updates.** `commit` is read-snapshot → build → swap. Two publishers
///    racing means the slower one swaps in an index derived from a snapshot that
///    predates the other's docs, silently dropping them. Holding this lock across
///    the whole snapshot→publish section makes each publish atomic.
pub struct IngestGate {
    publish: Mutex<()>,
    /// `Some(t)` ⇒ the ANN is stale and `t` is the most recent invalidation.
    /// `None` ⇒ the graph matches the corpus.
    stale: Mutex<Option<Instant>>,
}

impl Default for IngestGate {
    fn default() -> Self {
        Self::new()
    }
}

impl IngestGate {
    pub fn new() -> Self {
        IngestGate {
            publish: Mutex::new(()),
            stale: Mutex::new(None),
        }
    }

    /// Hold this across snapshot → derive → `commit`. Poisoning is ignored: a
    /// panicked publisher leaves the `LiveIndex` untouched (it swaps last), so the
    /// next publisher can safely proceed.
    pub fn publishing(&self) -> MutexGuard<'_, ()> {
        self.publish.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Note that the corpus grew, so the ANN no longer covers all of it.
    pub fn touch(&self) {
        *self.stale.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    }

    /// Claim a rebuild if the graph is stale *and* nothing has been ingested for
    /// `quiet`. Waiting for the lull is what coalesces a burst of page ingests into
    /// a single rebuild instead of one per page. Clears the marker, so an ingest
    /// arriving mid-rebuild re-marks it and the next pass picks it up.
    pub fn claim_refresh(&self, quiet: Duration) -> bool {
        let mut stale = self.stale.lock().unwrap_or_else(|e| e.into_inner());
        match *stale {
            Some(t) if t.elapsed() >= quiet => {
                *stale = None;
                true
            }
            _ => false,
        }
    }
}

/// How long the corpus must be quiet before the ANN is rebuilt.
pub const ANN_QUIET: Duration = Duration::from_secs(30);

/// Rebuild the ANN graph in the background once ingests settle down.
///
/// `/ingest` no longer rebuilds the graph inline — it appends segments and shares
/// the existing graph, which stays *valid* (appending never renumbers an address)
/// but blind to the new docs. This thread closes that gap: after `quiet` with no
/// further ingest it builds a fresh graph off a snapshot — outside the publish
/// lock, since that's the slow part — then publishes it.
///
/// The graph is only installed if the live index still `extends` the snapshot it
/// was built from. A background *merge* renumbers addresses, so a graph built
/// before one would point at the wrong documents; in that case the refresh is
/// abandoned and re-armed rather than applied.
pub fn spawn_ann_refresher(
    live: Arc<LiveIndex>,
    gate: Arc<IngestGate>,
    quiet: Duration,
    dir: Option<PathBuf>,
) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(2));
        if !gate.claim_refresh(quiet) {
            continue;
        }
        let base = live.snapshot();
        let covers = base.segment_count();
        let fresh = base.build_ann_graph();

        let _publishing = gate.publishing();
        let cur = live.snapshot();
        if !cur.extends(&base) {
            gate.touch(); // a merge moved addresses under us — rebuild next pass
            continue;
        }
        let vectors = fresh.as_ref().map(|a| a.len()).unwrap_or(0);
        commit(&live, cur.with_ann(fresh, covers), dir.as_deref());
        println!("omni: ANN refreshed ({vectors} vector(s))");
    });
}

/// Background-merge configuration.
pub struct BgMerge {
    /// How often to check whether a merge is warranted.
    pub interval: Duration,
    /// Tiered policy factor (a tier merges once it holds this many segments).
    pub merge_factor: usize,
    /// Index directory; when set, a live merge is also persisted so it survives a
    /// restart. `None` ⇒ in-memory swap only.
    pub dir: Option<PathBuf>,
}

/// Spawn the background merge thread. It wakes every `interval`, and if the tiered
/// policy finds an over-full size tier, performs **one** merge step off a snapshot
/// and swaps the result in (persisting first when a dir is configured). One step
/// per wake bounds the work each cycle; an unbalanced index converges over several
/// cycles. A balanced index does nothing but sleep.
pub fn spawn_background_merger(live: Arc<LiveIndex>, cfg: BgMerge, gate: Arc<IngestGate>) {
    std::thread::spawn(move || {
        let policy = MergePolicy::new(cfg.merge_factor);
        loop {
            std::thread::sleep(cfg.interval);
            let snap = live.snapshot();
            let sizes: Vec<usize> = snap.segments().iter().map(|s| s.total_docs()).collect();
            let Some(sel) = policy.pick(&sizes) else {
                continue; // already balanced
            };
            if sel.len() < 2 {
                continue;
            }
            let before = snap.segments().len();
            let merged = snap.merged_view(&sel); // shares untouched segments
            let after = merged.segments().len();

            // The merge above ran off a snapshot and took a while; if anything was
            // published meanwhile, `merged` no longer contains it and swapping it in
            // would silently drop those documents. Verify under the publish lock
            // that the live index is still exactly what we merged from, and if not,
            // discard this step — the next cycle re-picks against the current
            // layout. (Equal length + `extends` ⇒ the same segment list.)
            let _publishing = gate.publishing();
            let cur = live.snapshot();
            if cur.segments().len() != snap.segments().len() || !cur.extends(&snap) {
                continue;
            }
            // Persist (durable) then atomically swap.
            commit(&live, merged, cfg.dir.as_deref());
            println!("omni: background merge {before} → {after} segment(s)");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Index;

    /// The refresher must wait for a lull. Without this, a burst of page ingests
    /// would trigger a rebuild each — which is the pathology the gate exists to
    /// prevent, just moved to a background thread.
    #[test]
    fn refresh_waits_for_a_lull_and_coalesces_a_burst() {
        let gate = IngestGate::new();
        let quiet = Duration::from_millis(60);

        assert!(
            !gate.claim_refresh(quiet),
            "nothing ingested ⇒ nothing to do"
        );

        // A burst: many ingests in quick succession.
        for _ in 0..20 {
            gate.touch();
        }
        assert!(
            !gate.claim_refresh(quiet),
            "must not rebuild while ingests are still arriving"
        );

        std::thread::sleep(quiet * 2);
        assert!(gate.claim_refresh(quiet), "one rebuild once things settle");
        assert!(
            !gate.claim_refresh(quiet),
            "and only one — the burst coalesced into a single rebuild"
        );

        // An ingest arriving after a claim re-arms it, so nothing is missed.
        gate.touch();
        std::thread::sleep(quiet * 2);
        assert!(
            gate.claim_refresh(quiet),
            "later ingest re-arms the refresh"
        );
    }

    /// Two publishers must not interleave their snapshot→commit sections.
    #[test]
    fn publish_lock_serializes_publishers() {
        let gate = Arc::new(IngestGate::new());
        let live = LiveIndex::new(Index::new());
        let g = gate.publishing();

        let (gate2, live2) = (Arc::clone(&gate), Arc::clone(&live));
        let t = std::thread::spawn(move || {
            let _held = gate2.publishing(); // blocks until the main thread releases
            let mut idx = Index::new();
            idx.add_document("u".into(), "T".into(), "hello");
            live2.swap(Arc::new(idx));
        });

        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            live.snapshot().doc_count(),
            0,
            "blocked while the lock is held"
        );

        drop(g);
        t.join().unwrap();
        assert_eq!(live.snapshot().doc_count(), 1, "proceeds once released");
    }

    #[test]
    fn snapshot_survives_swap() {
        let mut a = Index::new();
        a.add_document("u".into(), "T".into(), "hello world");
        let live = LiveIndex::new(a);

        // A snapshot taken before the swap keeps seeing the old index.
        let old = live.snapshot();
        assert_eq!(old.doc_count(), 1);

        let mut b = Index::new();
        b.add_document("u".into(), "T".into(), "hello world");
        b.add_document("v".into(), "T2".into(), "another document");
        live.swap(Arc::new(b));

        assert_eq!(old.doc_count(), 1, "old snapshot unchanged after swap");
        assert_eq!(live.snapshot().doc_count(), 2, "new snapshot sees the swap");
    }
}
