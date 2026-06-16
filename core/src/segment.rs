//! A **segment** — one self-contained, searchable mini-index.
//!
//! A segment has two modes:
//!   * **In-memory** (writable) — built by `add_document`; postings live in a
//!     `HashMap`. This is the segment an update is currently filling, and what
//!     compaction produces.
//!   * **Lazy** (immutable, loaded from disk) — the segment's bytes are held
//!     (memory-mapped or owned). Posting lists are **decoded on demand** per
//!     queried term (cached), and the big stored fields — `text` and
//!     `embedding` — are likewise read from the mapping only when needed
//!     (`Segment::text`/`embedding`). At load we build just a term→byte-range
//!     dictionary and the small per-doc fields (url, title, lengths, rank,
//!     tombstone), so opening a large index is cheap and most of its bytes are
//!     never touched.
//!
//! A segment owns its dense local `doc_id` space; a global document is addressed
//! by `(segment, local_id)` at the index level.

use crate::analyze;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Position gap inserted between the title field and the body field, so a phrase
/// query can't match across the field boundary.
const FIELD_GAP: u32 = 100;

/// Cap on the stored body text (chars) — used for snippets *and* passage
/// chunking, so it's generous enough to hold several passages, not just the lead.
const STORED_TEXT_CAP: usize = 8000;

/// One stored document. In a lazy (loaded) segment the small fields are eager,
/// but the large `text` and `embedding` are left empty here and fetched on demand
/// from the mapping via `Segment::text`/`Segment::embedding` (`emb_len` records
/// the vector length so it can be skipped/decoded without holding it in memory).
#[derive(Clone)]
pub struct Document {
    pub id: usize,
    pub url: String,
    pub title: String,
    pub text: String,
    pub len_title: u32,
    pub len_body: u32,
    pub rank: f64,
    pub content_hash: u64,
    pub deleted: bool,
    /// Passage embeddings, **flattened**: `n_passages × emb_len` floats (one
    /// vector per passage of the doc; see `passages.rs`). Empty if not embedded.
    pub embedding: Vec<f32>,
    /// Per-passage embedding dimension (0 = not embedded). Known even when the
    /// vectors are lazily on disk.
    pub emb_len: u32,
    /// Number of passage vectors stored in `embedding`.
    pub n_passages: u32,
    /// Publish time as unix seconds (0 = unknown), for the freshness boost.
    pub published: i64,
}

impl Document {
    #[allow(dead_code)]
    pub fn len(&self) -> u32 {
        self.len_title + self.len_body
    }
}

/// A posting: one document, per-field term frequencies, and sorted positions.
#[derive(Clone)]
pub struct Posting {
    pub doc_id: usize,
    pub tf_title: u32,
    pub tf_body: u32,
    pub positions: Vec<u32>,
}

/// Where a term's posting blob lives in a lazy segment's bytes.
pub struct TermMeta {
    pub offset: usize,
    pub len: usize,
    pub df: usize,
}

/// The byte source for a lazy segment.
enum Bytes {
    Owned(Vec<u8>),
    #[cfg(unix)]
    Mapped(crate::mmap::Mmap),
}
impl Bytes {
    fn as_slice(&self) -> &[u8] {
        match self {
            Bytes::Owned(v) => v.as_slice(),
            #[cfg(unix)]
            Bytes::Mapped(m) => m.as_slice(),
        }
    }
}

/// One self-contained, searchable segment.
pub struct Segment {
    /// Stored fields (eager), addressed by local doc id.
    pub docs: Vec<Document>,
    url_to_id: HashMap<String, usize>,
    live_count: usize,
    total_title_len: u64,
    total_body_len: u64,

    /// In-memory mode: term → posting list.
    mem_postings: HashMap<String, Vec<Posting>>,
    /// Lazy mode: the segment bytes + a term→byte-range dictionary + a decode
    /// cache. `data` is `None` for an in-memory segment.
    data: Option<Bytes>,
    term_index: HashMap<String, TermMeta>,
    cache: RwLock<HashMap<String, Arc<[Posting]>>>,
    /// Lazy mode: byte offset of each doc's cold fields (text, then embedding).
    doc_offsets: Vec<usize>,
}

impl Segment {
    /// A fresh in-memory (writable) segment.
    pub fn new() -> Self {
        Segment {
            docs: Vec::new(),
            url_to_id: HashMap::new(),
            live_count: 0,
            total_title_len: 0,
            total_body_len: 0,
            mem_postings: HashMap::new(),
            data: None,
            term_index: HashMap::new(),
            cache: RwLock::new(HashMap::new()),
            doc_offsets: Vec::new(),
        }
    }

    fn new_lazy(data: Bytes) -> Self {
        let mut s = Segment::new();
        s.data = Some(data);
        s
    }

    /// Create a lazy segment backed by owned bytes (used by the non-mmap loader).
    pub fn new_lazy_owned(bytes: Vec<u8>) -> Self {
        Segment::new_lazy(Bytes::Owned(bytes))
    }

    /// Create a lazy segment backed by a memory map.
    #[cfg(unix)]
    pub fn new_lazy_mmap(m: crate::mmap::Mmap) -> Self {
        Segment::new_lazy(Bytes::Mapped(m))
    }

    /// Install the term→byte-range dictionary (lazy mode, from the loader).
    pub fn set_term_index(&mut self, index: HashMap<String, TermMeta>) {
        self.term_index = index;
    }

    /// The raw on-disk bytes backing a lazy segment (its content-addressed file
    /// contents), or `None` for an in-memory segment. Lets `save` reuse a loaded
    /// segment's file without re-encoding it.
    pub fn raw_bytes(&self) -> Option<&[u8]> {
        self.data.as_ref().map(|d| d.as_slice())
    }

    // ---- stats / liveness --------------------------------------------------

    pub fn live_count(&self) -> usize {
        self.live_count
    }
    pub fn total_docs(&self) -> usize {
        self.docs.len()
    }
    pub fn total_title_len(&self) -> u64 {
        self.total_title_len
    }
    pub fn total_body_len(&self) -> u64 {
        self.total_body_len
    }
    pub fn is_live(&self, doc_id: usize) -> bool {
        !self.docs[doc_id].deleted
    }

    /// True for a loaded (immutable, lazily-decoded) segment; false for an
    /// in-memory writable one.
    pub fn is_lazy(&self) -> bool {
        self.data.is_some()
    }

    /// `(url, local_id)` for every live document in this segment.
    pub fn live_entries(&self) -> Vec<(String, usize)> {
        self.url_to_id
            .iter()
            .map(|(u, &i)| (u.clone(), i))
            .collect()
    }

    /// Document frequency of a term (postings incl. tombstones).
    pub fn term_df(&self, term: &str) -> usize {
        if let Some(v) = self.mem_postings.get(term) {
            return v.len();
        }
        self.term_index.get(term).map(|m| m.df).unwrap_or(0)
    }

    // ---- postings (lazy in mapped mode) ------------------------------------

    /// The posting list for a term, decoding from disk on first access (lazy
    /// mode) and caching it. Returns an owned `Arc` so callers don't hold a
    /// borrow into the cache/mapping.
    pub fn postings(&self, term: &str) -> Option<Arc<[Posting]>> {
        if let Some(v) = self.mem_postings.get(term) {
            return Some(Arc::from(v.clone()));
        }
        let meta = self.term_index.get(term)?;
        if let Some(a) = self.cache.read().unwrap().get(term) {
            return Some(a.clone());
        }
        let slice = &self.data.as_ref()?.as_slice()[meta.offset..meta.offset + meta.len];
        let decoded: Arc<[Posting]> = Arc::from(crate::persist::decode_postings(slice));
        self.cache
            .write()
            .unwrap()
            .insert(term.to_string(), decoded.clone());
        Some(decoded)
    }

    /// The stored body text for a doc — decoded from the mapping on demand in
    /// lazy mode, borrowed from memory in in-memory mode.
    pub fn text(&self, doc_id: usize) -> Cow<'_, str> {
        match &self.data {
            Some(b) => Cow::Owned(crate::persist::decode_doc_text(
                b.as_slice(),
                self.doc_offsets[doc_id],
            )),
            None => Cow::Borrowed(&self.docs[doc_id].text),
        }
    }

    /// The doc's **flat** passage-embedding blob (`n_passages × emb_len` floats),
    /// decoded from the mapping on demand in lazy mode. Empty if not embedded.
    pub fn embedding(&self, doc_id: usize) -> Cow<'_, [f32]> {
        let d = &self.docs[doc_id];
        let total = d.emb_len as usize * d.n_passages as usize;
        match &self.data {
            Some(_) if total == 0 => Cow::Owned(Vec::new()),
            Some(b) => Cow::Owned(crate::persist::decode_doc_embedding(
                b.as_slice(),
                self.doc_offsets[doc_id],
                total,
            )),
            None => Cow::Borrowed(&self.docs[doc_id].embedding),
        }
    }

    /// The doc's passage vectors as owned rows (`n_passages` of length `emb_len`).
    /// Decodes the blob once. Empty if the doc isn't embedded.
    pub fn passages(&self, doc_id: usize) -> Vec<Vec<f32>> {
        let dim = self.docs[doc_id].emb_len as usize;
        if dim == 0 {
            return Vec::new();
        }
        self.embedding(doc_id)
            .chunks(dim)
            .map(|c| c.to_vec())
            .collect()
    }

    /// Positions of `term` within a local doc, if present (owned copy).
    pub fn positions(&self, term: &str, doc_id: usize) -> Option<Vec<u32>> {
        let postings = self.postings(term)?;
        let idx = postings.binary_search_by_key(&doc_id, |p| p.doc_id).ok()?;
        Some(postings[idx].positions.clone())
    }

    /// All (term, postings) pairs — decoding everything in lazy mode. Used by
    /// compaction and serialization, which need the full segment.
    pub fn all_postings(&self) -> Vec<(String, Vec<Posting>)> {
        if self.data.is_none() {
            return self
                .mem_postings
                .iter()
                .map(|(t, v)| (t.clone(), v.clone()))
                .collect();
        }
        let bytes = self.data.as_ref().unwrap().as_slice();
        self.term_index
            .iter()
            .map(|(t, m)| {
                (
                    t.clone(),
                    crate::persist::decode_postings(&bytes[m.offset..m.offset + m.len]),
                )
            })
            .collect()
    }

    // ---- building (in-memory mode) -----------------------------------------

    /// Add a document (in-memory mode only). Returns the new local doc id.
    pub fn add_document(&mut self, url: String, title: String, text: &str) -> usize {
        let id = self.docs.len();

        let title_terms = analyze::analyze_positions(&title);
        let body_terms = analyze::analyze_positions(text);
        let body_base = analyze::tokenize(&title).len() as u32 + FIELD_GAP;
        let len_title = title_terms.len() as u32;
        let len_body = body_terms.len() as u32;

        struct Acc {
            tf_title: u32,
            tf_body: u32,
            positions: Vec<u32>,
        }
        let mut acc: HashMap<String, Acc> = HashMap::new();
        for (term, pos) in title_terms {
            let e = acc.entry(term).or_insert(Acc {
                tf_title: 0,
                tf_body: 0,
                positions: Vec::new(),
            });
            e.tf_title += 1;
            e.positions.push(pos);
        }
        for (term, pos) in body_terms {
            let e = acc.entry(term).or_insert(Acc {
                tf_title: 0,
                tf_body: 0,
                positions: Vec::new(),
            });
            e.tf_body += 1;
            e.positions.push(body_base + pos);
        }
        for (term, mut a) in acc {
            a.positions.sort_unstable();
            self.mem_postings.entry(term).or_default().push(Posting {
                doc_id: id,
                tf_title: a.tf_title,
                tf_body: a.tf_body,
                positions: a.positions,
            });
        }

        let content_hash = content_hash(&title, text);
        let stored = store_text(text, STORED_TEXT_CAP);
        self.docs.push(Document {
            id,
            url: url.clone(),
            title,
            text: stored,
            len_title,
            len_body,
            rank: 0.0,
            content_hash,
            deleted: false,
            embedding: Vec::new(),
            emb_len: 0,
            n_passages: 0,
            published: 0,
        });
        self.live_count += 1;
        self.total_title_len += len_title as u64;
        self.total_body_len += len_body as u64;
        self.url_to_id.insert(url, id);
        id
    }

    /// Insert an already-built posting list (in-memory mode; used by compaction).
    pub fn insert_postings(&mut self, term: String, postings: Vec<Posting>) {
        self.mem_postings.insert(term, postings);
    }

    /// Append a doc to a *lazy* segment: its small fields (in `doc`) plus the
    /// byte `offset` of its cold fields (text, embedding) within the segment
    /// bytes, for on-demand decoding.
    pub fn push_lazy_doc(&mut self, doc: Document, offset: usize) {
        self.doc_offsets.push(offset);
        self.push_document(doc);
    }

    /// Append a fully-formed document record (loader / compaction). Maintains
    /// live stats and the url map according to the doc's tombstone.
    pub fn push_document(&mut self, doc: Document) {
        let id = doc.id;
        if !doc.deleted {
            self.live_count += 1;
            self.total_title_len += doc.len_title as u64;
            self.total_body_len += doc.len_body as u64;
            self.url_to_id.insert(doc.url.clone(), id);
        }
        self.docs.push(doc);
    }

    /// Mark a local doc deleted (tombstone). Idempotent.
    pub fn delete_doc(&mut self, doc_id: usize) {
        let doc = &mut self.docs[doc_id];
        if doc.deleted {
            return;
        }
        doc.deleted = true;
        self.live_count -= 1;
        self.total_title_len -= doc.len_title as u64;
        self.total_body_len -= doc.len_body as u64;
        if self.url_to_id.get(&doc.url) == Some(&doc_id) {
            let url = doc.url.clone();
            self.url_to_id.remove(&url);
        }
    }

    /// Set the PageRank of a local doc.
    pub fn set_rank(&mut self, doc_id: usize, rank: f64) {
        self.docs[doc_id].rank = rank;
    }

    /// Set the publish time (unix secs) of a local doc.
    pub fn set_published(&mut self, doc_id: usize, ts: i64) {
        self.docs[doc_id].published = ts;
    }
}

/// Stable 64-bit FNV-1a hash of a document's indexed content.
pub fn content_hash(title: &str, text: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    let mix = |h: &mut u64, b: u8| {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    for b in title.bytes() {
        mix(&mut h, b);
    }
    mix(&mut h, 0);
    for b in text.bytes() {
        mix(&mut h, b);
    }
    h
}

fn store_text(text: &str, max_chars: usize) -> String {
    let cleaned: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.chars().count() <= max_chars {
        cleaned
    } else {
        cleaned.chars().take(max_chars).collect()
    }
}
