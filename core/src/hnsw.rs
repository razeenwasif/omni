//! HNSW — Hierarchical Navigable Small World graph for **approximate** nearest-
//! neighbor search over document embeddings.
//!
//! Brute-force cosine (`semantic_ranking`'s fallback) is exact but O(N·dim) per
//! query — fine for a few hundred docs, linear past that. HNSW trades a little
//! recall for **sub-linear** search: a layered proximity graph where upper layers
//! are sparse "express lanes" of long-range links and layer 0 holds everyone.
//! A query greedily descends the upper layers to land near its neighborhood, then
//! runs a bounded beam search (`ef`) on layer 0. Build is O(N·log N), search
//! ~O(log N) hops. (Malkov & Yashunin, 2016.)
//!
//! Scope notes for this codebase:
//!   * Vectors are **normalized** on insert, so cosine similarity is a plain dot
//!     product and the graph distance is `1 - dot` (smaller = closer).
//!   * Construction is **deterministic** (a seeded SplitMix64 picks node levels),
//!     so the same embeddings always yield the same graph.
//!   * Neighbor selection uses the paper's **diversity heuristic** (Algorithm 4),
//!     which keeps long-range edges and lifts recall over a plain "M closest".
//!   * The graph **topology** is persisted as a small `ann` sidecar (`to_bytes`/
//!     `from_bytes`); vectors are rehydrated from the segments, not stored twice.
//!   * Vectors live in RAM by default, but a **lazy** graph (`keep_ram = false`)
//!     keeps none and decodes each via a `Fetch` from the segments on demand —
//!     less memory at scale, slower per query.

use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};

/// Fetches a node's raw (un-normalized) vector by global address — used in
/// **lazy** mode to read embeddings from the index/mapping instead of a RAM copy.
pub type Fetch<'a> = &'a dyn Fn(Addr) -> Vec<f32>;

/// A global document address: `(segment index, local doc id)` — matches `query`.
pub type Addr = (usize, usize);

/// HNSW tunables.
#[derive(Clone, Copy)]
pub struct Params {
    /// Max neighbors per node on layers ≥ 1.
    pub m: usize,
    /// Max neighbors per node on layer 0 (denser; conventionally 2·M).
    pub m0: usize,
    /// Beam width while inserting — larger = better graph, slower build.
    pub ef_construction: usize,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            m: 16,
            m0: 32,
            ef_construction: 100,
        }
    }
}

/// Hard cap on a node's top layer, so a freak draw can't create a tall empty tower.
const MAX_LEVEL: usize = 16;

/// A deterministic SplitMix64 PRNG (no external crate, reproducible builds).
struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in [0, 1).
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// One graph candidate: a distance to the query and the node it belongs to.
/// Ordered by distance (ascending) via `total_cmp`, so a `BinaryHeap` (max-heap)
/// keeps the *farthest* on top — handy for bounding a result set to the `ef`
/// nearest.
#[derive(Clone, Copy)]
struct Neighbor {
    dist: f32,
    id: u32,
}
impl PartialEq for Neighbor {
    fn eq(&self, o: &Self) -> bool {
        self.dist == o.dist && self.id == o.id
    }
}
impl Eq for Neighbor {}
impl Ord for Neighbor {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.dist.total_cmp(&o.dist).then(self.id.cmp(&o.id))
    }
}
impl PartialOrd for Neighbor {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}

/// The built graph.
pub struct Hnsw {
    params: Params,
    /// Vector dimensionality.
    dim: usize,
    /// Normalized vectors, indexed by internal id. **Empty in lazy mode** — there
    /// the vectors are read from the index/mapping on demand via a `Fetch`, saving
    /// the RAM duplicate at the cost of decoding per distance (only worth it at
    /// large scale; the default keeps them here).
    vectors: Vec<Vec<f32>>,
    /// internal id → global document address.
    addrs: Vec<Addr>,
    /// `links[id][layer]` = neighbor ids of `id` on `layer` (only layers the node
    /// exists on, i.e. `0..=levels[id]`).
    links: Vec<Vec<Vec<u32>>>,
    /// Top layer each node lives on.
    levels: Vec<usize>,
    entry: Option<u32>,
    max_level: usize,
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn normalize(v: &mut [f32]) {
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        for x in v.iter_mut() {
            *x /= n;
        }
    }
}

impl Hnsw {
    /// Build a graph over `(addr, vector)` items. Construction always uses the
    /// vectors in RAM; when `keep_ram` is false they're dropped afterward, leaving
    /// a **lazy** graph that re-reads vectors via a `Fetch` at search time.
    pub fn build(items: Vec<(Addr, Vec<f32>)>, params: Params, keep_ram: bool) -> Hnsw {
        let dim = items.first().map(|(_, v)| v.len()).unwrap_or(0);
        let mut h = Hnsw {
            params,
            dim,
            vectors: Vec::with_capacity(items.len()),
            addrs: Vec::with_capacity(items.len()),
            links: Vec::with_capacity(items.len()),
            levels: Vec::with_capacity(items.len()),
            entry: None,
            max_level: 0,
        };
        // Seed mixes a constant with the item count for a stable-but-varied graph.
        let mut rng = Rng(0x6F6D_6E69_4E4E_53_u64 ^ items.len() as u64);
        for (addr, mut v) in items {
            normalize(&mut v);
            h.insert(addr, v, &mut rng);
        }
        if !keep_ram {
            h.vectors = Vec::new(); // lazy: drop the RAM copy, decode on demand
            h.vectors.shrink_to_fit();
        }
        h
    }

    /// True if this graph reads vectors lazily (no in-RAM copy) — search then
    /// requires a `Fetch`.
    pub fn is_lazy(&self) -> bool {
        self.vectors.is_empty() && !self.addrs.is_empty()
    }

    pub fn len(&self) -> usize {
        self.addrs.len()
    }
    #[allow(dead_code)] // conventional companion to len(); used in tests
    pub fn is_empty(&self) -> bool {
        self.addrs.is_empty()
    }

    /// A node's normalized vector: borrowed from RAM, or decoded via `fetch` in
    /// lazy mode.
    fn vector(&self, id: u32, fetch: Option<Fetch>) -> Cow<'_, [f32]> {
        if !self.vectors.is_empty() {
            Cow::Borrowed(&self.vectors[id as usize])
        } else if let Some(f) = fetch {
            let mut v = f(self.addrs[id as usize]);
            normalize(&mut v);
            Cow::Owned(v)
        } else {
            Cow::Owned(vec![0.0; self.dim]) // lazy graph queried without a fetch
        }
    }

    fn max_conn(&self, layer: usize) -> usize {
        if layer == 0 {
            self.params.m0
        } else {
            self.params.m
        }
    }

    /// Distance from query slice `q` to stored node `id` (1 − cosine).
    fn dist(&self, q: &[f32], id: u32, fetch: Option<Fetch>) -> f32 {
        1.0 - dot(q, &self.vector(id, fetch))
    }

    /// Draw a node's top layer: `floor(-ln(U) / ln(M))`, the standard HNSW
    /// exponential decay so each higher layer is ~1/M as populated.
    fn random_level(&self, rng: &mut Rng) -> usize {
        let u = 1.0 - rng.unit(); // (0, 1] so ln is finite
        let ml = 1.0 / (self.params.m as f64).ln();
        ((-u.ln()) * ml).floor() as usize
    }

    fn insert(&mut self, addr: Addr, v: Vec<f32>, rng: &mut Rng) {
        let id = self.vectors.len() as u32;
        let l = self.random_level(rng).min(MAX_LEVEL);
        let q = v.clone();
        self.vectors.push(v);
        self.addrs.push(addr);
        self.levels.push(l);
        self.links.push((0..=l).map(|_| Vec::new()).collect());

        if self.entry.is_none() {
            self.entry = Some(id);
            self.max_level = l;
            return;
        }

        // Phase 1: greedily descend the layers above the new node with ef=1.
        let mut cur = self.entry.unwrap();
        let top = self.max_level;
        if l < top {
            for lc in ((l + 1)..=top).rev() {
                if let Some(best) = self.search_layer(&q, &[cur], lc, 1, None).first() {
                    cur = best.id;
                }
            }
        }

        // Phase 2: from min(l, top) down to 0, beam-search and wire up neighbors.
        let mut eps = vec![cur];
        for lc in (0..=l.min(top)).rev() {
            let candidates = self.search_layer(&q, &eps, lc, self.params.ef_construction, None);
            let m = self.max_conn(lc);
            for nb in self.select_neighbors(&candidates, m) {
                self.links[id as usize][lc].push(nb);
                self.links[nb as usize][lc].push(id);
                self.prune(nb, lc);
            }
            eps = candidates.iter().map(|n| n.id).collect();
            if eps.is_empty() {
                eps = vec![cur];
            }
        }

        if l > self.max_level {
            self.max_level = l;
            self.entry = Some(id);
        }
    }

    /// Select up to `m` neighbors for a node at `q` from `candidates` (sorted
    /// nearest-first) using the paper's **diversity heuristic** (Algorithm 4):
    /// accept a candidate only if it's closer to `q` than to every neighbor
    /// already chosen. This skips redundant links into the same cluster, so the
    /// graph keeps long-range edges that make navigation (and recall) better than
    /// a plain "M closest". Underfilled results are backfilled with the nearest
    /// rejects (`keepPrunedConnections`) so connectivity isn't sacrificed.
    ///
    /// `candidates` must be sorted nearest-first; each `.dist` is the distance to
    /// the node being connected (so no separate query vector is needed here).
    fn select_neighbors(&self, candidates: &[Neighbor], m: usize) -> Vec<u32> {
        let mut selected: Vec<u32> = Vec::with_capacity(m);
        for c in candidates {
            if selected.len() >= m {
                break;
            }
            // Keep c only if no already-selected neighbor is nearer to c than q is.
            let diverse = selected.iter().all(|&r| {
                let d_cr = 1.0 - dot(&self.vectors[c.id as usize], &self.vectors[r as usize]);
                c.dist < d_cr
            });
            if diverse {
                selected.push(c.id);
            }
        }
        if selected.len() < m {
            for c in candidates {
                if selected.len() >= m {
                    break;
                }
                if !selected.contains(&c.id) {
                    selected.push(c.id);
                }
            }
        }
        selected
    }

    /// Re-select a node's neighbor list on `layer` with the diversity heuristic
    /// when it exceeds the connection limit (after a back-link was added).
    fn prune(&mut self, node: u32, layer: usize) {
        let m = self.max_conn(layer);
        if self.links[node as usize][layer].len() <= m {
            return;
        }
        let base = self.vectors[node as usize].clone();
        let mut scored: Vec<Neighbor> = self.links[node as usize][layer]
            .iter()
            .map(|&e| Neighbor {
                dist: 1.0 - dot(&base, &self.vectors[e as usize]),
                id: e,
            })
            .collect();
        scored.sort_unstable();
        self.links[node as usize][layer] = self.select_neighbors(&scored, m);
    }

    /// Beam search on a single `layer` from entry points `eps`, returning the up-to
    /// `ef` nearest nodes found, sorted nearest-first.
    fn search_layer(
        &self,
        q: &[f32],
        eps: &[u32],
        layer: usize,
        ef: usize,
        fetch: Option<Fetch>,
    ) -> Vec<Neighbor> {
        let mut visited: HashSet<u32> = HashSet::with_capacity(ef * 4);
        let mut cand: BinaryHeap<Reverse<Neighbor>> = BinaryHeap::new(); // min-dist on top
        let mut res: BinaryHeap<Neighbor> = BinaryHeap::new(); // max-dist on top
        for &e in eps {
            let nb = Neighbor {
                dist: self.dist(q, e, fetch),
                id: e,
            };
            visited.insert(e);
            cand.push(Reverse(nb));
            res.push(nb);
        }

        while let Some(Reverse(c)) = cand.pop() {
            let farthest = res.peek().map(|n| n.dist).unwrap_or(f32::INFINITY);
            if c.dist > farthest && res.len() >= ef {
                break; // nothing closer than what we already have
            }
            // Neighbors of c on this layer (absent if c doesn't reach this layer).
            let neighbors = self
                .links
                .get(c.id as usize)
                .and_then(|l| l.get(layer))
                .cloned()
                .unwrap_or_default();
            for e in neighbors {
                if !visited.insert(e) {
                    continue;
                }
                let d = self.dist(q, e, fetch);
                let farthest = res.peek().map(|n| n.dist).unwrap_or(f32::INFINITY);
                if d < farthest || res.len() < ef {
                    let nb = Neighbor { dist: d, id: e };
                    cand.push(Reverse(nb));
                    res.push(nb);
                    if res.len() > ef {
                        res.pop(); // drop the current farthest
                    }
                }
            }
        }

        let mut out = res.into_vec();
        out.sort_unstable();
        out
    }

    /// Approximate `k` nearest documents to `query` (un-normalized is fine),
    /// returned as `(addr, cosine_similarity)` sorted most-similar-first. `ef` is
    /// the layer-0 beam width (larger ⇒ better recall, slower).
    pub fn search(
        &self,
        query: &[f32],
        ef: usize,
        k: usize,
        fetch: Option<Fetch>,
    ) -> Vec<(Addr, f32)> {
        let Some(entry) = self.entry else {
            return Vec::new();
        };
        if query.is_empty() {
            return Vec::new();
        }
        let mut q = query.to_vec();
        normalize(&mut q);

        let mut cur = entry;
        for lc in (1..=self.max_level).rev() {
            if let Some(best) = self.search_layer(&q, &[cur], lc, 1, fetch).first() {
                cur = best.id;
            }
        }
        let mut res = self.search_layer(&q, &[cur], 0, ef.max(k), fetch);
        res.truncate(k);
        res.into_iter()
            .map(|n| (self.addrs[n.id as usize], 1.0 - n.dist))
            .collect()
    }

    // ---- persistence (topology only; vectors are rehydrated) ---------------

    /// Serialize the graph **topology** — params, per-node address, level, and
    /// adjacency. The vectors themselves are *not* written (they already live in
    /// the segment files); `from_bytes` rebuilds them from the index. This keeps
    /// the sidecar small and avoids storing embeddings twice.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(ANN_MAGIC);
        wv(&mut b, self.params.m as u64);
        wv(&mut b, self.params.m0 as u64);
        wv(&mut b, self.params.ef_construction as u64);
        wv(&mut b, self.max_level as u64);
        match self.entry {
            Some(e) => {
                b.push(1);
                wv(&mut b, e as u64);
            }
            None => b.push(0),
        }
        wv(&mut b, self.dim as u64);
        wv(&mut b, self.addrs.len() as u64); // node count (vectors may be empty in lazy mode)
        for id in 0..self.addrs.len() {
            let (seg, local) = self.addrs[id];
            wv(&mut b, seg as u64);
            wv(&mut b, local as u64);
            let level = self.levels[id];
            wv(&mut b, level as u64);
            for lc in 0..=level {
                let links = &self.links[id][lc];
                wv(&mut b, links.len() as u64);
                for &n in links {
                    wv(&mut b, n as u64);
                }
            }
        }
        b
    }

    /// Rebuild a graph from `to_bytes` output. When `keep_ram`, each node's vector
    /// is rehydrated via `vector_for(addr)` into RAM; otherwise the graph is left
    /// **lazy** (vectors decoded on demand at search time). Returns `None` if the
    /// bytes are malformed or a needed vector is missing (stale graph) — the caller
    /// then rebuilds from scratch.
    pub fn from_bytes(
        bytes: &[u8],
        keep_ram: bool,
        mut vector_for: impl FnMut(Addr) -> Option<Vec<f32>>,
    ) -> Option<Hnsw> {
        let mut r = Rd { b: bytes, pos: 0 };
        if r.take(ANN_MAGIC.len())? != ANN_MAGIC {
            return None;
        }
        let params = Params {
            m: r.varint()? as usize,
            m0: r.varint()? as usize,
            ef_construction: r.varint()? as usize,
        };
        let max_level = r.varint()? as usize;
        let entry = match r.byte()? {
            0 => None,
            _ => Some(r.varint()? as u32),
        };
        let dim = r.varint()? as usize;
        let n = r.varint()? as usize;
        let mut h = Hnsw {
            params,
            dim,
            vectors: Vec::with_capacity(if keep_ram { n } else { 0 }),
            addrs: Vec::with_capacity(n),
            links: Vec::with_capacity(n),
            levels: Vec::with_capacity(n),
            entry,
            max_level,
        };
        for _ in 0..n {
            let addr = (r.varint()? as usize, r.varint()? as usize);
            let level = r.varint()? as usize;
            let mut node_links = Vec::with_capacity(level + 1);
            for _ in 0..=level {
                let deg = r.varint()? as usize;
                let mut layer = Vec::with_capacity(deg);
                for _ in 0..deg {
                    layer.push(r.varint()? as u32);
                }
                node_links.push(layer);
            }
            if keep_ram {
                let mut v = vector_for(addr)?; // missing ⇒ stale graph
                normalize(&mut v);
                h.vectors.push(v);
            }
            h.addrs.push(addr);
            h.levels.push(level);
            h.links.push(node_links);
        }
        Some(h)
    }
}

/// ANN sidecar format magic (+ version).
const ANN_MAGIC: &[u8; 5] = b"OANN2";

/// Append an unsigned LEB128 varint.
fn wv(buf: &mut Vec<u8>, mut x: u64) {
    loop {
        let mut byte = (x & 0x7f) as u8;
        x >>= 7;
        if x != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if x == 0 {
            break;
        }
    }
}

/// Minimal byte reader for `from_bytes`.
struct Rd<'a> {
    b: &'a [u8],
    pos: usize,
}
impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.pos..self.pos + n)?;
        self.pos += n;
        Some(s)
    }
    fn byte(&mut self) -> Option<u8> {
        let v = *self.b.get(self.pos)?;
        self.pos += 1;
        Some(v)
    }
    fn varint(&mut self) -> Option<u64> {
        let mut result = 0u64;
        let mut shift = 0;
        loop {
            let byte = self.byte()?;
            result |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift >= 64 {
                return None;
            }
        }
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a graph over random vectors and check that ANN finds the true top-1
    /// (by exact cosine) within its top-10 for the large majority of queries.
    #[test]
    fn ann_recall_matches_brute_force() {
        let (dim, n) = (12usize, 300usize);
        let mut rng = Rng(0x1234_5678);
        let mut vecs: Vec<Vec<f32>> = Vec::new();
        for _ in 0..n {
            vecs.push(
                (0..dim)
                    .map(|_| (rng.unit() as f32) * 2.0 - 1.0)
                    .collect::<Vec<f32>>(),
            );
        }
        let items: Vec<(Addr, Vec<f32>)> = vecs
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, v)| ((0, i), v))
            .collect();
        let h = Hnsw::build(items, Params::default(), true);
        assert_eq!(h.len(), n);

        let trials = 40;
        let mut hits = 0;
        let mut qr = Rng(0x0F0F_0F0F);
        for _ in 0..trials {
            let q: Vec<f32> = (0..dim).map(|_| (qr.unit() as f32) * 2.0 - 1.0).collect();
            // Exact nearest by cosine.
            let mut qn = q.clone();
            normalize(&mut qn);
            let mut best = (usize::MAX, f32::NEG_INFINITY);
            for (i, v) in vecs.iter().enumerate() {
                let mut vn = v.clone();
                normalize(&mut vn);
                let s = dot(&qn, &vn);
                if s > best.1 {
                    best = (i, s);
                }
            }
            let ann = h.search(&q, 64, 10, None);
            if ann.iter().any(|&((_, id), _)| id == best.0) {
                hits += 1;
            }
        }
        let recall = hits as f64 / trials as f64;
        assert!(
            recall >= 0.9,
            "recall@10 too low: {hits}/{trials} = {recall}"
        );
    }

    #[test]
    fn persist_round_trips_identically() {
        let (dim, n) = (10usize, 200usize);
        let mut rng = Rng(0xABCD_1234);
        let items: Vec<(Addr, Vec<f32>)> = (0..n)
            .map(|i| {
                let v: Vec<f32> = (0..dim).map(|_| (rng.unit() as f32) * 2.0 - 1.0).collect();
                ((0usize, i), v)
            })
            .collect();
        let original = Hnsw::build(items.clone(), Params::default(), true);

        // Serialize topology, then rehydrate vectors from the same source.
        let bytes = original.to_bytes();
        let by_addr: std::collections::HashMap<Addr, Vec<f32>> = items.into_iter().collect();
        let restored =
            Hnsw::from_bytes(&bytes, true, |a| by_addr.get(&a).cloned()).expect("valid sidecar");
        assert_eq!(restored.len(), original.len());

        // Identical search results before and after a round-trip.
        let mut qr = Rng(0x5555);
        for _ in 0..20 {
            let q: Vec<f32> = (0..dim).map(|_| (qr.unit() as f32) * 2.0 - 1.0).collect();
            let a = original.search(&q, 48, 10, None);
            let b = restored.search(&q, 48, 10, None);
            assert_eq!(a.len(), b.len());
            for (x, y) in a.iter().zip(&b) {
                assert_eq!(x.0, y.0, "same neighbor order after round-trip");
            }
        }

        // A missing vector ⇒ stale graph ⇒ None (caller rebuilds).
        assert!(Hnsw::from_bytes(&bytes, true, |_| None::<Vec<f32>>).is_none());
        // Garbage bytes ⇒ None, not a panic.
        assert!(Hnsw::from_bytes(b"nope", true, |_| Some(vec![0.0; dim])).is_none());
    }

    #[test]
    fn lazy_mode_matches_ram_mode() {
        // Same graph built RAM-resident vs. lazy must return identical results —
        // lazy just fetches each vector on demand instead of holding a copy.
        let (dim, n) = (10usize, 200usize);
        let mut rng = Rng(0x2468_ACE0);
        let items: Vec<(Addr, Vec<f32>)> = (0..n)
            .map(|i| {
                let v: Vec<f32> = (0..dim).map(|_| (rng.unit() as f32) * 2.0 - 1.0).collect();
                ((0usize, i), v)
            })
            .collect();
        let by_addr: std::collections::HashMap<Addr, Vec<f32>> = items.iter().cloned().collect();

        let ram = Hnsw::build(items.clone(), Params::default(), true);
        let lazy = Hnsw::build(items, Params::default(), false);
        assert!(lazy.is_lazy() && !ram.is_lazy());
        assert_eq!(lazy.len(), n);

        // Lazy fetch returns the raw vector for an address (search re-normalizes).
        let fetch = |a: Addr| by_addr.get(&a).cloned().unwrap_or_default();

        let mut qr = Rng(0x1357);
        for _ in 0..25 {
            let q: Vec<f32> = (0..dim).map(|_| (qr.unit() as f32) * 2.0 - 1.0).collect();
            let a = ram.search(&q, 48, 10, None);
            let b = lazy.search(&q, 48, 10, Some(&fetch));
            assert_eq!(a.len(), b.len());
            for (x, y) in a.iter().zip(&b) {
                assert_eq!(x.0, y.0, "lazy and RAM modes must agree on order");
            }
        }
    }

    #[test]
    fn empty_and_tiny_graphs_are_safe() {
        let empty = Hnsw::build(vec![], Params::default(), true);
        assert!(empty.is_empty());
        assert!(empty.search(&[1.0, 0.0], 10, 5, None).is_empty());

        let one = Hnsw::build(vec![((0, 0), vec![1.0, 0.0, 0.0])], Params::default(), true);
        let got = one.search(&[0.9, 0.1, 0.0], 10, 5, None);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, (0, 0));
    }
}
