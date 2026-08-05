//! The index — a **collection of segments** (`segment.rs`).
//!
//! Documents are added to a single *writable* segment; `begin_segment` seals it
//! so the next batch (e.g. an incremental update) lands in a *new* segment
//! without rewriting the old ones. Deletes are per-segment tombstones. Search
//! (`query.rs`) runs every segment with **globally aggregated** BM25F stats
//! (so scores are comparable across segments) and merges a global top-K.
//! `compact` rebuilds everything into one tombstone-free segment.
//!
//! A global document is addressed by `(segment, local_id)`; `url_to_addr` maps a
//! live url to that address for incremental replace/delete and PageRank.

use crate::embed::{Embedder, EmbedderConfig};
use crate::score::FieldStats;
use crate::segment::Segment;
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

// Re-export the per-segment types so the rest of the crate keeps importing them
// from `index` (and so on-disk/query code is unaffected by the split).
pub use crate::segment::{content_hash, Document, Image, Posting};

/// The full index: an ordered set of segments plus index-wide config.
///
/// Segments are held behind `Arc` so an immutable segment can be **shared** by
/// more than one `Index` at once — that's what lets a background merge build a new
/// index that reuses the untouched segments and then be swapped in atomically
/// (see `merged_view` and `live.rs`) without copying or blocking readers.
pub struct Index {
    segments: Vec<Arc<Segment>>,
    /// Index of the current writable segment (where `add_document` lands), or
    /// `None` until the first add / after `begin_segment`.
    writable: Option<usize>,
    /// Live url -> global address `(segment, local_id)`.
    url_to_addr: HashMap<String, (usize, usize)>,
    /// How documents are embedded for semantic retrieval (None disables it).
    embedder: EmbedderConfig,
    /// Approximate nearest-neighbor graph over the live embeddings, for
    /// sub-linear semantic recall. `None` until `build_ann` (or too few vectors).
    ///
    /// Behind an `Arc` so a derived index can **share** the graph rather than
    /// rebuild it. `with_appended` only ever pushes segments onto the end, so every
    /// address `(segment, local)` the graph already holds stays valid: the shared
    /// graph is correct for the appended index, merely blind to the newly added
    /// docs until a background refresh rebuilds it (`live::spawn_ann_refresher`).
    ann: Option<Arc<crate::hnsw::Hnsw>>,
    /// How many leading segments `ann` was built over. Segments from this index
    /// onward were appended afterwards and are **not** in the graph, so semantic
    /// retrieval scores them exactly instead (`query::semantic_ranking`). Equal to
    /// `segments.len()` right after a build; it only lags after `with_appended`.
    ann_segments: usize,
    /// When true, the ANN graph keeps no RAM vector copy and decodes embeddings
    /// from the segments on demand (saves memory at scale; slower per query).
    ann_lazy: bool,
}

impl Index {
    pub fn new() -> Self {
        Index {
            segments: Vec::new(),
            writable: None,
            url_to_addr: HashMap::new(),
            embedder: EmbedderConfig::none(),
            ann: None,
            ann_segments: 0,
            ann_lazy: false,
        }
    }

    pub fn segments(&self) -> &[Arc<Segment>] {
        &self.segments
    }

    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// Total live documents — `N` for IDF and the avg denominators.
    pub fn doc_count(&self) -> usize {
        self.segments.iter().map(|s| s.live_count()).sum()
    }

    /// Total documents across all segments, tombstones included.
    pub fn total_docs(&self) -> usize {
        self.segments.iter().map(|s| s.total_docs()).sum()
    }

    /// Global document frequency of a term (summed across segments).
    pub fn term_df(&self, term: &str) -> usize {
        self.segments.iter().map(|s| s.term_df(term)).sum()
    }

    /// Per-field corpus averages over *all* live docs (global BM25F stats).
    pub fn field_stats(&self) -> FieldStats {
        let live: u64 = self.doc_count() as u64;
        if live == 0 {
            return FieldStats {
                avg_title_len: 1.0,
                avg_body_len: 1.0,
            };
        }
        let tt: u64 = self.segments.iter().map(|s| s.total_title_len()).sum();
        let tb: u64 = self.segments.iter().map(|s| s.total_body_len()).sum();
        FieldStats {
            avg_title_len: if tt == 0 {
                1.0
            } else {
                tt as f64 / live as f64
            },
            avg_body_len: if tb == 0 {
                1.0
            } else {
                tb as f64 / live as f64
            },
        }
    }

    // ---- building ----------------------------------------------------------

    /// Seal the current writable segment so the next `add_document` starts a new
    /// one. Used so an incremental update's docs land in a fresh segment.
    pub fn begin_segment(&mut self) {
        self.writable = None;
    }

    /// Add a document to the writable segment (creating one if needed). Returns
    /// its global address `(segment, local_id)`.
    pub fn add_document(&mut self, url: String, title: String, text: &str) -> (usize, usize) {
        self.add_document_with_images(url, title, text, Vec::new())
    }

    /// Add a document plus extracted page images to the writable segment.
    pub fn add_document_with_images(
        &mut self,
        url: String,
        title: String,
        text: &str,
        images: Vec<Image>,
    ) -> (usize, usize) {
        let seg = match self.writable {
            Some(s) => s,
            None => {
                self.segments.push(Arc::new(Segment::new()));
                let s = self.segments.len() - 1;
                self.writable = Some(s);
                s
            }
        };
        // The writable segment is freshly created here and never shared while the
        // index is being built, so it is uniquely owned.
        let local = Self::seg_mut(&mut self.segments[seg]).add_document_with_images(
            url.clone(),
            title,
            text,
            images,
        );
        self.url_to_addr.insert(url, (seg, local));
        (seg, local)
    }

    /// Append a fully-built segment (used by the loader). Does not become the
    /// writable segment.
    pub fn push_segment(&mut self, seg: Segment) {
        let s = self.segments.len();
        for (url, local) in seg.live_entries() {
            self.url_to_addr.insert(url, (s, local));
        }
        self.segments.push(Arc::new(seg));
    }

    /// Mutable access to a uniquely-owned segment. Panics if the segment is shared
    /// (an Arc clone exists) — by construction the mutating build/update paths only
    /// touch segments before they are shared with a reader or a swapped-in index.
    fn seg_mut(seg: &mut Arc<Segment>) -> &mut Segment {
        Arc::get_mut(seg).expect("segment mutated while shared")
    }

    /// Live address for a url, if indexed anywhere.
    pub fn addr_for_url(&self, url: &str) -> Option<(usize, usize)> {
        self.url_to_addr.get(url).copied()
    }

    /// `(url, addr)` for every live document — used by the incremental updater.
    pub fn live_entries(&self) -> Vec<(String, (usize, usize))> {
        self.url_to_addr
            .iter()
            .map(|(u, &a)| (u.clone(), a))
            .collect()
    }

    /// Set the publish time (unix secs) of the live document for `url`, if any.
    pub fn set_published(&mut self, url: &str, ts: i64) {
        if let Some((seg, local)) = self.addr_for_url(url) {
            Self::seg_mut(&mut self.segments[seg]).set_published(local, ts);
        }
    }

    /// Tombstone the live document for `url`, if any.
    pub fn delete_by_url(&mut self, url: &str) {
        if let Some((seg, local)) = self.url_to_addr.remove(url) {
            Self::seg_mut(&mut self.segments[seg]).delete_doc(local);
        }
    }

    // ---- PageRank ----------------------------------------------------------

    /// Global doc index (segment-major order) for a url, for building the
    /// PageRank graph over the whole index.
    pub fn url_to_global(&self, url: &str) -> Option<usize> {
        let (seg, local) = self.addr_for_url(url)?;
        Some(self.global_base(seg) + local)
    }

    fn global_base(&self, seg: usize) -> usize {
        self.segments[..seg].iter().map(|s| s.total_docs()).sum()
    }

    /// Apply a flat PageRank vector (length `total_docs`, segment-major order).
    pub fn set_ranks(&mut self, ranks: &[f64]) {
        let mut base = 0;
        for seg in self.segments.iter_mut() {
            let total = seg.total_docs();
            let seg = Self::seg_mut(seg);
            for local in 0..total {
                if let Some(&r) = ranks.get(base + local) {
                    seg.set_rank(local, r);
                }
            }
            base += total;
        }
    }

    // ---- embeddings --------------------------------------------------------

    pub fn embedder(&self) -> &EmbedderConfig {
        &self.embedder
    }

    pub fn set_embedder(&mut self, cfg: EmbedderConfig) {
        self.embedder = cfg;
    }

    pub fn clear_embeddings(&mut self) {
        for seg in self.segments.iter_mut() {
            for doc in Self::seg_mut(seg).docs.iter_mut() {
                doc.embedding = Vec::new();
                doc.emb_len = 0;
                doc.n_passages = 0;
            }
        }
        self.embedder.dim = 0;
        self.ann = None;
        self.ann_segments = 0;
    }

    // ---- approximate nearest neighbors (HNSW) ------------------------------

    /// The ANN graph for semantic recall, if one has been built.
    pub fn ann(&self) -> Option<&crate::hnsw::Hnsw> {
        self.ann.as_deref()
    }

    /// Install a prebuilt ANN graph (e.g. loaded from the `ann` sidecar).
    ///
    /// Coverage is read off the graph rather than assumed to be the whole index: a
    /// persisted graph can predate segments that `/ingest` appended after it was
    /// built (the append shares the graph instead of rebuilding it, and both are
    /// saved), and treating those segments as covered would make their documents
    /// semantically invisible until the next rebuild.
    pub fn set_ann(&mut self, ann: crate::hnsw::Hnsw) {
        self.ann_segments = ann.covered_segments().min(self.segments.len());
        self.ann = Some(Arc::new(ann));
    }

    /// How many leading segments the ANN graph covers; the rest were appended
    /// after it was built and need exact scoring.
    pub fn ann_segments(&self) -> usize {
        self.ann_segments
    }

    /// Whether the ANN graph should be lazy (no RAM vector copy). Set before
    /// building/loading the graph.
    pub fn ann_lazy(&self) -> bool {
        self.ann_lazy
    }
    pub fn set_ann_lazy(&mut self, lazy: bool) {
        self.ann_lazy = lazy;
    }

    /// Build the HNSW graph over every live, embedded document, replacing any
    /// previous one. A no-op (leaving `ann = None`, so search falls back to exact
    /// brute force) when there are fewer than `ANN_MIN` vectors — below that, exact
    /// cosine is both faster and strictly better than an approximation.
    pub fn build_ann(&mut self) {
        self.ann = self.build_ann_graph();
        self.ann_segments = self.segments.len();
    }

    /// Build the HNSW graph **without** installing it. This is the expensive part
    /// of `build_ann` (it re-materializes every passage vector), split out so the
    /// background refresher can do it off a read-only snapshot, outside any lock,
    /// and publish the result with `with_ann`.
    pub fn build_ann_graph(&self) -> Option<Arc<crate::hnsw::Hnsw>> {
        const ANN_MIN: usize = 64;
        // One graph node per *passage*, tagged with its doc address + passage index
        // (grouped by doc so a lazy reload can cache per-doc decodes).
        let mut items: Vec<((usize, usize), u32, Vec<f32>)> = Vec::new();
        for (si, seg) in self.segments.iter().enumerate() {
            for local in 0..seg.total_docs() {
                if seg.is_live(local) && seg.docs[local].emb_len > 0 {
                    for (pi, pv) in seg.passages(local).into_iter().enumerate() {
                        items.push(((si, local), pi as u32, pv));
                    }
                }
            }
        }
        let keep_ram = !self.ann_lazy;
        (items.len() >= ANN_MIN).then(|| {
            Arc::new(crate::hnsw::Hnsw::build(
                items,
                crate::hnsw::Params::default(),
                keep_ram,
            ))
        })
    }

    /// Embed every live document missing a vector. Only **in-memory** segments
    /// are touched — a loaded segment's vectors are immutable on disk, so to
    /// (re)embed an existing index you compact it first (`compact` rebuilds an
    /// in-memory segment, which this then embeds). Returns how many were embedded.
    pub fn embed_missing(&mut self, embedder: &Embedder) -> usize {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Phase A — gather work under an immutable borrow. Pre-chunk each doc into
        // its title-prefixed passage prompts so the worker threads never touch the
        // index. `(seg, doc)` index pairs let us write results back afterwards.
        struct Work {
            seg: usize,
            doc: usize,
            prompts: Vec<String>,
        }
        let (words_per, overlap, max_passages) = crate::passages::params();
        let mut work: Vec<Work> = Vec::new();
        for (si, seg) in self.segments.iter().enumerate() {
            if seg.is_lazy() {
                continue;
            }
            for (di, doc) in seg.docs.iter().enumerate() {
                if doc.deleted || doc.emb_len > 0 {
                    continue;
                }
                let passages = crate::passages::chunk(&doc.text, words_per, overlap, max_passages);
                if passages.is_empty() {
                    continue;
                }
                let prompts = passages
                    .iter()
                    .map(|p| format!("{}. {}", doc.title, p))
                    .collect();
                work.push(Work {
                    seg: si,
                    doc: di,
                    prompts,
                });
            }
        }
        let todo = work.len();
        let show = todo > 200; // only narrate large batches, not a few ingested docs

        // Phase B — embed passages in parallel. nomic-embed-text is tiny (~0.3 GB
        // VRAM) and Ollama keeps one copy resident, so N concurrent HTTP requests
        // share a single loaded model: this hides per-call connection latency
        // without multiplying VRAM. The hash embedder benefits too (pure CPU).
        // Each worker owns a contiguous slice and returns `(work_idx, np, flat)`;
        // results carry their index so write-back stays deterministic.
        let n_workers = todo.min(8).max(1);
        let done = AtomicUsize::new(0);
        let mut results: Vec<(usize, u32, Vec<f32>)> = Vec::with_capacity(todo);
        if todo > 0 {
            let work_ref = &work;
            let done_ref = &done;
            std::thread::scope(|s| {
                let per = todo.div_ceil(n_workers);
                let handles: Vec<_> = (0..n_workers)
                    .map(|w| {
                        let start = w * per;
                        let end = (start + per).min(todo);
                        s.spawn(move || {
                            let mut out: Vec<(usize, u32, Vec<f32>)> = Vec::new();
                            for wi in start..end {
                                let mut flat: Vec<f32> = Vec::new();
                                let mut np: u32 = 0;
                                for prompt in &work_ref[wi].prompts {
                                    let v = embedder.embed_doc(prompt);
                                    if v.is_empty() {
                                        continue; // skip a failed passage
                                    }
                                    flat.extend_from_slice(&v);
                                    np += 1;
                                }
                                if np > 0 {
                                    out.push((wi, np, flat));
                                }
                                let c = done_ref.fetch_add(1, Ordering::Relaxed) + 1;
                                if show && c % 100 == 0 {
                                    eprint!("\romni: embedding {c}/{todo} docs…");
                                    let _ = std::io::stderr().flush();
                                }
                            }
                            out
                        })
                    })
                    .collect();
                for h in handles {
                    if let Ok(part) = h.join() {
                        results.extend(part);
                    }
                }
            });
        }
        if show {
            eprintln!(
                "\romni: embedded {}/{todo} docs                 ",
                results.len()
            );
        }

        // Phase C — write embeddings back (mutable borrow). The per-passage dim is
        // uniform for a given embedder; take it from the first vector produced.
        let mut dim = self.embedder.dim;
        if dim == 0 {
            if let Some((_, np, flat)) = results.iter().find(|(_, np, _)| *np > 0) {
                dim = flat.len() / *np as usize;
            }
        }
        let mut embedded = 0;
        for (wi, np, flat) in results {
            let (seg, doc) = (work[wi].seg, work[wi].doc);
            let d = &mut Self::seg_mut(&mut self.segments[seg]).docs[doc];
            d.emb_len = dim as u32; // per-passage dim
            d.n_passages = np;
            d.embedding = flat;
            embedded += 1;
        }
        self.embedder.dim = dim;
        embedded
    }

    // ---- compaction / merging ----------------------------------------------

    /// Merge **all** segments into one tombstone-free segment, reclaiming the
    /// space held by deleted documents. Postings are *remapped*, not re-tokenized,
    /// so nothing is lost to the stored-text cap; ranks and embeddings are kept.
    pub fn compact(&mut self) {
        let all: Vec<usize> = (0..self.segments.len()).collect();
        let merged = self.build_merged(&all);
        self.segments = vec![Arc::new(merged)];
        self.writable = None;
        self.rebuild_url_map();
    }

    /// Merge a **subset** of segments (by index) into one new segment, leaving the
    /// rest untouched. The merged segment is appended; the consumed ones are
    /// dropped (their live docs survive in the merged segment, tombstones don't).
    /// Used by the automatic tiered merge policy (`maybe_merge`).
    pub fn merge_segments(&mut self, sel: &[usize]) {
        if sel.len() < 2 {
            return; // nothing to combine
        }
        let merged = self.build_merged(sel);
        let drop: std::collections::HashSet<usize> = sel.iter().copied().collect();
        let kept: Vec<Arc<Segment>> = std::mem::take(&mut self.segments)
            .into_iter()
            .enumerate()
            .filter_map(|(i, seg)| (!drop.contains(&i)).then_some(seg))
            .collect();
        self.segments = kept;
        self.segments.push(Arc::new(merged));
        self.writable = None;
        self.rebuild_url_map();
    }

    /// A **non-mutating** append: produce a *new* `Index` that **shares** (via
    /// `Arc`) all of `self`'s segments and adds `other`'s non-empty segments after
    /// them. Used by the live `/ingest` endpoint — the staged docs become a new
    /// segment appended to a snapshot, then swapped in (`live.rs`); the background
    /// merger folds it into the tiers later.
    pub fn with_appended(&self, other: &Index) -> Index {
        let mut segments: Vec<Arc<Segment>> = self.segments.clone();
        for s in &other.segments {
            if s.total_docs() > 0 {
                segments.push(Arc::clone(s));
            }
        }
        let mut idx = Index {
            segments,
            writable: None,
            url_to_addr: HashMap::new(),
            embedder: self.embedder.clone(),
            // Share the existing graph — do NOT rebuild it here. Appending never
            // renumbers an existing address, so the old graph stays valid; it just
            // doesn't know the new docs yet, and they remain findable lexically
            // until `spawn_ann_refresher` rebuilds off-thread.
            //
            // Rebuilding inline used to cost a full HNSW build (every passage
            // vector re-materialized) *per ingested page*, inside the request
            // thread, with no concurrency limit — which is how a browsing session's
            // auto-ingests turned into 140 concurrent rebuilds and 30 GB of RSS.
            ann: self.ann.clone(),
            // Coverage does *not* grow: the appended segments aren't in the graph.
            // Semantic retrieval scores them exactly until the refresher catches up.
            ann_segments: self.ann_segments,
            ann_lazy: self.ann_lazy,
        };
        idx.rebuild_url_map();
        idx
    }

    /// Whether `self` is `base` plus zero or more **appended** segments — i.e. the
    /// two share a common segment prefix by identity. When true, any address valid
    /// in `base` is still valid in `self`, so an ANN graph built against `base` can
    /// be installed on `self` (`with_ann`). A merge renumbers addresses and breaks
    /// this, which is exactly what it's here to detect.
    pub fn extends(&self, base: &Index) -> bool {
        self.segments.len() >= base.segments.len()
            && base
                .segments
                .iter()
                .zip(&self.segments)
                .all(|(a, b)| Arc::ptr_eq(a, b))
    }

    /// A cheap derived index with `ann` swapped in: segments are shared via `Arc`
    /// and only the url map is rebuilt. Used by the background ANN refresher to
    /// publish a freshly built graph without disturbing in-flight readers.
    ///
    /// `covers` is how many leading segments the graph was built over — the
    /// refresher builds off an older snapshot, so this is that snapshot's segment
    /// count, not necessarily `self`'s.
    pub fn with_ann(&self, ann: Option<Arc<crate::hnsw::Hnsw>>, covers: usize) -> Index {
        let mut idx = Index {
            segments: self.segments.clone(),
            writable: None,
            url_to_addr: HashMap::new(),
            embedder: self.embedder.clone(),
            ann,
            ann_segments: covers.min(self.segments.len()),
            ann_lazy: self.ann_lazy,
        };
        idx.rebuild_url_map();
        idx
    }

    /// Add the curated essential-site launch cards (`bangs::cards`) to the index in
    /// a fresh segment, skipping any url already present. Returns how many were
    /// added. Run on the build path behind `--essential`.
    ///
    /// The cards are given **top-authority rank** (the current max PageRank), an
    /// intentional editorial pin so a search for a site's name (`github`,
    /// `overleaf`) surfaces its card rather than some high-PageRank page that merely
    /// mentions it. (Where there's no PageRank, all ranks are equal and the title
    /// match decides anyway.)
    pub fn add_essential_sites(&mut self) -> usize {
        let cards = crate::bangs::cards();
        if cards.iter().all(|(u, _, _)| self.addr_for_url(u).is_some()) {
            return 0;
        }
        let boost = self.max_rank();
        self.begin_segment();
        let mut added = 0;
        for (url, title, body) in cards {
            if self.addr_for_url(&url).is_none() {
                let (seg, local) = self.add_document(url, title, &body);
                Self::seg_mut(&mut self.segments[seg]).set_rank(local, boost);
                added += 1;
            }
        }
        added
    }

    /// The largest PageRank in the index (0.0 if none assigned).
    fn max_rank(&self) -> f64 {
        self.segments
            .iter()
            .flat_map(|s| s.docs.iter())
            .map(|d| d.rank)
            .fold(0.0_f64, f64::max)
    }

    /// A **non-mutating** merge: produce a *new* `Index` that combines the segments
    /// named in `sel` into one and **shares** (via `Arc`) the untouched segments
    /// with `self`. This is what a background merge runs against a read-only
    /// snapshot — the result is then swapped in atomically (`live.rs`) while
    /// in-flight readers keep using the old index until they drop it.
    pub fn merged_view(&self, sel: &[usize]) -> Index {
        let merged = Arc::new(self.build_merged(sel));
        let drop: std::collections::HashSet<usize> = sel.iter().copied().collect();
        let mut segments: Vec<Arc<Segment>> = self
            .segments
            .iter()
            .enumerate()
            .filter(|(i, _)| !drop.contains(i))
            .map(|(_, s)| Arc::clone(s)) // share, don't copy
            .collect();
        segments.push(merged);

        let mut idx = Index {
            segments,
            writable: None,
            url_to_addr: HashMap::new(),
            embedder: self.embedder.clone(),
            ann: None,
            ann_segments: 0,
            ann_lazy: self.ann_lazy,
        };
        idx.rebuild_url_map();
        // Addresses shifted, so the ANN must be rebuilt over the new layout.
        idx.build_ann();
        idx
    }

    /// Apply `policy` repeatedly until the segment layout is balanced, executing
    /// each merge it picks. Returns the number of merges performed. Segments stay
    /// immutable on disk between updates; this is run on the build/update path
    /// (offline), then the merged index is saved.
    pub fn maybe_merge(&mut self, policy: &crate::merge::MergePolicy) -> usize {
        let mut merges = 0;
        loop {
            let sizes: Vec<usize> = self.segments.iter().map(|s| s.total_docs()).collect();
            match policy.pick(&sizes) {
                Some(sel) if sel.len() >= 2 => {
                    self.merge_segments(&sel);
                    merges += 1;
                }
                _ => break,
            }
        }
        merges
    }

    /// Build one in-memory segment from the live documents of the segments named
    /// in `sel`, remapping their postings to fresh contiguous local ids. The
    /// shared core of `compact` (all segments) and `merge_segments` (a subset).
    fn build_merged(&self, sel: &[usize]) -> Segment {
        let mut merged = Segment::new();
        // Assign new local ids to live docs, in the given segment order.
        let mut remap: HashMap<(usize, usize), usize> = HashMap::new();
        for &s in sel {
            let seg = &self.segments[s];
            for (local, doc) in seg.docs.iter().enumerate() {
                if doc.deleted {
                    continue;
                }
                let nid = merged.total_docs();
                remap.insert((s, doc.id), nid);
                let mut d = doc.clone();
                d.id = nid;
                // Materialize the cold fields from the (possibly lazy) source so
                // the merged in-memory segment owns them.
                if seg.is_lazy() {
                    d.text = seg.text(local).into_owned();
                    d.embedding = seg.embedding(local).into_owned();
                }
                merged.push_document(d);
            }
        }
        // Remap postings of live docs to the new ids.
        let mut term_post: HashMap<String, Vec<Posting>> = HashMap::new();
        for &s in sel {
            let seg = &self.segments[s];
            for (term, plist) in seg.all_postings() {
                for p in &plist {
                    if let Some(&nid) = remap.get(&(s, p.doc_id)) {
                        term_post.entry(term.clone()).or_default().push(Posting {
                            doc_id: nid,
                            tf_title: p.tf_title,
                            tf_body: p.tf_body,
                            positions: p.positions.clone(),
                        });
                    }
                }
            }
        }
        for (term, mut plist) in term_post {
            plist.sort_by_key(|p| p.doc_id);
            merged.insert_postings(term, plist);
        }
        merged
    }

    /// Rebuild the url → `(segment, local)` map from scratch (after the segment
    /// set changes, e.g. a merge).
    fn rebuild_url_map(&mut self) {
        self.url_to_addr.clear();
        for (s, seg) in self.segments.iter().enumerate() {
            for (url, local) in seg.live_entries() {
                self.url_to_addr.insert(url, (s, local));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query;

    /// Same three docs, either in one segment or split across two.
    fn build(segmented: bool) -> Index {
        let mut idx = Index::new();
        idx.add_document("a".into(), "Rust".into(), "rust ownership and borrowing");
        idx.add_document("b".into(), "Go".into(), "go goroutines and channels");
        if segmented {
            idx.begin_segment();
        }
        idx.add_document(
            "c".into(),
            "Book".into(),
            "the rust programming language ownership",
        );
        idx
    }

    #[test]
    fn multi_segment_search_matches_single_segment() {
        let single = build(false);
        let multi = build(true);
        assert_eq!(single.segment_count(), 1);
        assert_eq!(multi.segment_count(), 2);
        assert_eq!(single.doc_count(), multi.doc_count());

        // Globally-aggregated stats ⇒ identical ranking whether or not the docs
        // are split across segments.
        for q in ["rust ownership", "go channels", "programming language"] {
            let s = query::search(&single, q, 10);
            let m = query::search(&multi, q, 10);
            assert_eq!(s.len(), m.len(), "result count differs for {q:?}");
            for (a, b) in s.iter().zip(&m) {
                assert_eq!(a.url, b.url, "order differs for {q:?}");
                assert!((a.score - b.score).abs() < 1e-9, "score differs for {q:?}");
            }
        }
    }

    #[test]
    fn compaction_merges_segments_and_drops_tombstones() {
        let mut idx = build(true); // 2 segments
        idx.delete_by_url("b"); // tombstone "b"
        let before = query::search(&idx, "rust ownership", 10);

        idx.compact();
        assert_eq!(idx.segment_count(), 1, "merged to one segment");
        assert_eq!(idx.doc_count(), 2, "a + c live");
        assert_eq!(idx.total_docs(), 2, "tombstone space reclaimed");

        // Search is unchanged by compaction, and the deleted doc stays gone.
        let after = query::search(&idx, "rust ownership", 10);
        assert_eq!(before.len(), after.len());
        for (x, y) in before.iter().zip(&after) {
            assert_eq!(x.url, y.url);
            assert!((x.score - y.score).abs() < 1e-9);
        }
        assert!(
            query::search(&idx, "goroutines", 10).is_empty(),
            "deleted doc gone"
        );
    }

    #[test]
    fn partial_merge_preserves_search() {
        // Three single-doc segments.
        let mut idx = Index::new();
        idx.add_document("a".into(), "Rust".into(), "rust ownership and borrowing");
        idx.begin_segment();
        idx.add_document("b".into(), "Go".into(), "go goroutines and channels");
        idx.begin_segment();
        idx.add_document(
            "c".into(),
            "Book".into(),
            "the rust programming language ownership",
        );
        assert_eq!(idx.segment_count(), 3);

        let queries = ["rust ownership", "go channels", "programming"];
        let before: Vec<_> = queries.iter().map(|q| query::search(&idx, q, 10)).collect();

        // Merge only the first two segments; the third is untouched.
        idx.merge_segments(&[0, 1]);
        assert_eq!(idx.segment_count(), 2, "0+1 merged, 2 kept");
        assert_eq!(idx.doc_count(), 3, "all docs still live");

        // Global stats are partition-invariant ⇒ identical ranking.
        for (q, b) in queries.iter().zip(&before) {
            let a = query::search(&idx, q, 10);
            assert_eq!(a.len(), b.len(), "count for {q:?}");
            for (x, y) in a.iter().zip(b) {
                assert_eq!(x.url, y.url, "order for {q:?}");
                assert!((x.score - y.score).abs() < 1e-9, "score for {q:?}");
            }
        }
    }

    /// A restart must not lose the distinction between "in the graph" and "appended
    /// after it". Both the index and the (older) graph are persisted on every
    /// ingest, so on load the graph legitimately covers fewer segments than exist —
    /// assuming otherwise makes freshly ingested pages semantically invisible until
    /// the next rebuild.
    #[test]
    fn loaded_ann_coverage_comes_from_the_graph_not_the_segment_count() {
        let mut base = Index::new();
        for i in 0..80 {
            base.add_document(format!("u{i}"), format!("Doc {i}"), "rust ownership");
        }
        base.set_embedder(crate::embed::EmbedderConfig {
            kind: crate::embed::KIND_HASH,
            dim: 32,
            url: String::new(),
            model: String::new(),
        });
        base.embed_missing(&crate::embed::Embedder::Hash { dim: 32 });
        base.build_ann();
        let graph = base.build_ann_graph().expect("graph over 80 docs");

        // Stand in for a reload: an index with an extra appended segment, handed the
        // graph that predates it.
        let mut staged = Index::new();
        staged.add_document("new".into(), "New".into(), "go channels");
        staged.set_embedder(base.embedder().clone());
        staged.embed_missing(&crate::embed::Embedder::Hash { dim: 32 });
        let mut reloaded = base.with_appended(&staged);
        assert_eq!(reloaded.segment_count(), 2);

        let Ok(graph) = Arc::try_unwrap(graph) else {
            unreachable!("sole owner")
        };
        reloaded.set_ann(graph);
        assert_eq!(
            reloaded.ann_segments(),
            1,
            "coverage must come from the graph's own addresses, not segment_count()"
        );
        assert!(
            query::search(&reloaded, "channels", 10)
                .iter()
                .any(|r| r.url == "new"),
            "a doc outside the loaded graph's coverage must still be retrievable"
        );
    }

    /// `with_appended` must not rebuild the ANN — that inline rebuild, once per
    /// ingested page and unbounded in parallelism, is what melted the server. It
    /// hands the existing graph to the appended index instead, which is only sound
    /// because appending never renumbers an address.
    #[test]
    fn with_appended_shares_the_ann_instead_of_rebuilding() {
        let mut base = Index::new();
        for i in 0..80 {
            base.add_document(format!("u{i}"), format!("Doc {i}"), "rust ownership");
        }
        base.set_embedder(crate::embed::EmbedderConfig {
            kind: crate::embed::KIND_HASH,
            dim: 32,
            url: String::new(),
            model: String::new(),
        });
        base.embed_missing(&crate::embed::Embedder::Hash { dim: 32 });
        base.build_ann();
        let before = base.ann().expect("80 docs is over ANN_MIN").len();

        // The ingest path embeds staged docs before appending, so mirror that.
        let mut staged = Index::new();
        staged.add_document("new".into(), "New".into(), "go channels");
        staged.set_embedder(base.embedder().clone());
        staged.embed_missing(&crate::embed::Embedder::Hash { dim: 32 });

        let grown = base.with_appended(&staged);

        // Same graph object, not a rebuild: identical size, and the new segment is
        // present but not yet represented in it.
        let after = grown.ann().expect("graph carried over").len();
        assert_eq!(before, after, "ANN was rebuilt during append");
        assert_eq!(grown.segment_count(), base.segment_count() + 1);
        assert_eq!(
            grown.ann_segments(),
            base.segment_count(),
            "coverage must not claim the appended segment"
        );
        // The appended doc is still findable — lexically, and via the exact
        // fallback — which is what makes the deferred rebuild acceptable.
        // ...and it is still retrievable, because segments outside the graph's
        // coverage are scored exactly rather than skipped.
        for q in ["channels", "go channels", "goroutines channels"] {
            assert!(
                query::search(&grown, q, 10).iter().any(|r| r.url == "new"),
                "appended doc must stay findable for {q:?} before the ANN catches up"
            );
        }
    }

    /// `extends` is the guard that decides whether a graph built off an older
    /// snapshot may still be installed. It must say yes to an append and no to a
    /// merge, because a merge renumbers every address.
    #[test]
    fn extends_accepts_appends_and_rejects_merges() {
        let mut base = Index::new();
        base.add_document("a".into(), "Rust".into(), "rust ownership");
        base.begin_segment();
        base.add_document("b".into(), "Go".into(), "go channels");

        let mut staged = Index::new();
        staged.add_document("c".into(), "C".into(), "c pointers");

        let appended = base.with_appended(&staged);
        assert!(appended.extends(&base), "append keeps existing addresses");
        assert!(base.extends(&base), "an index extends itself");
        assert!(
            !base.extends(&appended),
            "the shorter index does not extend the longer one"
        );

        let merged = base.merged_view(&[0, 1]);
        assert!(
            !merged.extends(&base),
            "a merge moves addresses, so a stale graph must be rejected"
        );
    }

    #[test]
    fn merged_view_shares_segments_and_preserves_search() {
        let mut idx = Index::new();
        idx.add_document("a".into(), "Rust".into(), "rust ownership and borrowing");
        idx.begin_segment();
        idx.add_document("b".into(), "Go".into(), "go goroutines and channels");
        idx.begin_segment();
        idx.add_document(
            "c".into(),
            "Book".into(),
            "the rust programming language ownership",
        );
        assert_eq!(idx.segment_count(), 3);
        let before = query::search(&idx, "rust ownership", 10);

        // Non-mutating merge of segments 0 and 1.
        let view = idx.merged_view(&[0, 1]);
        assert_eq!(idx.segment_count(), 3, "original is untouched");
        assert_eq!(view.segment_count(), 2, "0+1 merged, 2 kept");
        assert_eq!(view.doc_count(), 3);

        // The kept segment is the *same allocation*, shared not copied.
        assert!(
            Arc::ptr_eq(&idx.segments()[2], &view.segments()[0]),
            "untouched segment is shared via Arc"
        );

        let after = query::search(&view, "rust ownership", 10);
        assert_eq!(before.len(), after.len());
        for (x, y) in before.iter().zip(&after) {
            assert_eq!(x.url, y.url);
            assert!((x.score - y.score).abs() < 1e-9);
        }
    }

    #[test]
    fn maybe_merge_bounds_segment_count() {
        use crate::merge::MergePolicy;
        let mut idx = Index::new();
        // 12 single-doc segments (a stand-in for 12 incremental updates).
        for i in 0..12 {
            if i > 0 {
                idx.begin_segment();
            }
            idx.add_document(
                format!("u{i}"),
                format!("T{i}"),
                &format!("document {i} on rust"),
            );
        }
        assert_eq!(idx.segment_count(), 12);
        let before = query::search(&idx, "rust", 20);

        let merges = idx.maybe_merge(&MergePolicy::new(4));
        assert!(merges >= 1, "an over-full small tier should be merged");
        assert!(idx.segment_count() < 12, "segment count shrank");
        assert_eq!(idx.doc_count(), 12, "no documents lost");

        // The same set of documents is still retrieved (these tie on score, so
        // only membership is guaranteed, not order).
        let after = query::search(&idx, "rust", 20);
        assert_eq!(before.len(), after.len());
        let bset: std::collections::HashSet<_> = before.iter().map(|h| h.url.clone()).collect();
        let aset: std::collections::HashSet<_> = after.iter().map(|h| h.url.clone()).collect();
        assert_eq!(bset, aset, "same docs retrieved after merge");
    }
}
