//! On-disk index format — a **directory of segment files** (Stage 2a).
//!
//! The index directory holds:
//!   * `manifest`          — format version, embedder config, ordered list of the
//!                           live segments' ids.
//!   * `seg-<id>.seg`      — one *immutable* segment: stored fields (url, title,
//!                           text, field lengths, content hash, embedding) and
//!                           the varint/delta-compressed posting lists. The file
//!                           is **content-addressed** (`id` = a hash of its
//!                           bytes), so an unchanged segment keeps the same name
//!                           and is never rewritten — an incremental flush only
//!                           writes the *new* segment.
//!   * `seg-<id>.del`      — mutable tombstone bitset (absent ⇒ none deleted).
//!   * `seg-<id>.rank`     — mutable PageRank values, f64 per doc (absent ⇒ 0).
//!
//! Keeping deletes and ranks in small sidecars is why segments stay immutable:
//! an update that tombstones a doc or recomputes PageRank rewrites only kilobytes
//! of sidecar, never the big posting data. (Stage 2b will mmap each `.seg` and
//! decode postings lazily instead of loading them all into memory.)
//!
//! Posting compression is the classic pair: **delta-encode** sorted doc ids and
//! positions (store the gaps), then **varint (LEB128)** so small gaps cost a byte.

use crate::embed::EmbedderConfig;
use crate::index::{Document, Index, Posting};
use crate::segment::{Segment, TermMeta};
use std::borrow::Cow;
use std::collections::HashMap;
use std::io;
use std::path::Path;

const MANIFEST_MAGIC: &[u8; 8] = b"OMNIDIR\x07"; // magic + format version 7
const SEG_MAGIC: &[u8; 4] = b"OSG5"; // layout v5: per-doc passage embeddings (dim + count)

// ---- public API ------------------------------------------------------------

/// Write `index` into the directory `dir` (created if needed). Idempotent:
/// segments whose content is unchanged keep their file and aren't rewritten;
/// `.del`/`.rank` sidecars and the manifest are refreshed; orphaned files
/// (e.g. after compaction) are removed.
pub fn save(index: &Index, dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;

    let mut live: Vec<u64> = Vec::new();
    for seg in index.segments() {
        if seg.total_docs() == 0 {
            continue; // skip an empty (freshly begun, unused) segment
        }
        // A lazy (loaded) segment already holds its exact file bytes, so we hash
        // those directly — no re-encode, and its content-addressed file already
        // exists, so we skip the write. Only freshly-built segments are encoded.
        let bytes: Cow<[u8]> = match seg.raw_bytes() {
            Some(b) => Cow::Borrowed(b),
            None => Cow::Owned(encode_segment(seg)),
        };
        let id = fnv64(&bytes);
        live.push(id);

        let seg_path = dir.join(seg_name(id, "seg"));
        if !seg_path.exists() {
            atomic_write(&seg_path, &bytes)?;
        }
        write_sidecars(dir, id, seg)?;
    }

    atomic_write(
        &dir.join("manifest"),
        &encode_manifest(index.embedder(), &live),
    )?;
    remove_orphans(dir, &live)?;
    Ok(())
}

// ---- ANN (HNSW) sidecar ----------------------------------------------------

/// A fingerprint of the index's *composition* — its ordered segment ids plus the
/// embedder — so a persisted ANN graph can be detected as stale (a merge, update,
/// or embedder change shifts the segment ids and invalidates the graph's
/// `(segment, local)` addresses). Read from the manifest so save/load agree.
fn manifest_signature(dir: &Path) -> io::Result<u64> {
    let manifest = std::fs::read(dir.join("manifest"))?;
    let (emb, ids) = decode_manifest(&manifest)?;
    let mut buf = Vec::with_capacity(ids.len() * 8 + 16);
    for id in &ids {
        buf.extend_from_slice(&id.to_le_bytes());
    }
    buf.push(emb.kind);
    buf.extend_from_slice(&(emb.dim as u64).to_le_bytes());
    Ok(fnv64(&buf))
}

/// Persist the index's ANN graph as a small `ann` sidecar (topology only — the
/// vectors stay in the segment files). No-op if no graph is built. Call after
/// `save`, so the manifest it fingerprints is current.
pub fn save_ann(index: &Index, dir: &Path) -> io::Result<()> {
    let Some(ann) = index.ann() else {
        return Ok(());
    };
    let sig = manifest_signature(dir)?;
    let mut buf = Vec::new();
    buf.extend_from_slice(&sig.to_le_bytes());
    buf.extend_from_slice(&ann.to_bytes());
    atomic_write(&dir.join("ann"), &buf)
}

/// Try to load the `ann` sidecar into `index`, rehydrating node vectors from the
/// index's embeddings. Returns `false` (so the caller rebuilds) if the file is
/// absent, malformed, or **stale** (its signature ≠ the current manifest's, or a
/// referenced vector is missing).
pub fn load_ann(index: &mut Index, dir: &Path) -> bool {
    let Ok(bytes) = std::fs::read(dir.join("ann")) else {
        return false;
    };
    if bytes.len() < 8 {
        return false;
    }
    let Ok(sig) = manifest_signature(dir) else {
        return false;
    };
    if u64::from_le_bytes(bytes[..8].try_into().unwrap()) != sig {
        return false; // stale: composition or embedder changed
    }
    let keep_ram = !index.ann_lazy(); // lazy mode reads vectors on demand, none in RAM
    let graph = {
        let idx: &Index = index; // immutable reborrow, scoped to the rehydrate
                                 // Nodes are passages, grouped by doc, so a 1-entry cache makes rehydrating
                                 // a doc's passage vectors O(decode-once-per-doc) instead of per-passage.
        let mut cache: Option<((usize, usize), Vec<Vec<f32>>)> = None;
        crate::hnsw::Hnsw::from_bytes(&bytes[8..], keep_ram, |(s, l), pi| {
            if cache.as_ref().map(|(a, _)| *a) != Some((s, l)) {
                let seg = idx.segments().get(s)?;
                if l >= seg.total_docs() {
                    return None;
                }
                cache = Some(((s, l), seg.passages(l)));
            }
            cache
                .as_ref()
                .and_then(|(_, ps)| ps.get(pi as usize).cloned())
        })
    };
    match graph {
        Some(g) => {
            index.set_ann(g);
            true
        }
        None => false,
    }
}

/// Load an index directory (reads each segment fully into memory).
pub fn load(path: &Path) -> io::Result<Index> {
    load_impl(path, false)
}

/// Like `load`, but memory-maps each segment file for the decode instead of
/// heap-reading it (no full-file copy). Falls back to a read on non-unix.
pub fn load_mmap(path: &Path) -> io::Result<Index> {
    load_impl(path, true)
}

fn load_impl(dir: &Path, mmap: bool) -> io::Result<Index> {
    let manifest = std::fs::read(dir.join("manifest"))?;
    let (embedder, ids) = decode_manifest(&manifest)?;

    let mut index = Index::new();
    index.set_embedder(embedder);
    for id in ids {
        let seg_path = dir.join(seg_name(id, "seg"));
        let mut seg = open_segment(&seg_path, mmap)?;
        apply_sidecars(dir, id, &mut seg)?;
        index.push_segment(seg);
    }
    Ok(index)
}

/// Open a segment file as a **lazy** segment: stored fields are decoded eagerly,
/// but posting lists stay in the (mmapped or owned) bytes and are decoded on
/// demand. Only the term→byte-range dictionary is built up front.
#[allow(unused_variables)]
fn open_segment(path: &Path, mmap: bool) -> io::Result<Segment> {
    #[cfg(unix)]
    if mmap {
        let m = crate::mmap::Mmap::open(path)?;
        let (docs, term_index) = parse_seg_header(m.as_slice())?;
        let mut seg = Segment::new_lazy_mmap(m);
        for (d, off) in docs {
            seg.push_lazy_doc(d, off);
        }
        seg.set_term_index(term_index);
        return Ok(seg);
    }
    let bytes = std::fs::read(path)?;
    let (docs, term_index) = parse_seg_header(&bytes)?;
    let mut seg = Segment::new_lazy_owned(bytes);
    for (d, off) in docs {
        seg.push_lazy_doc(d, off);
    }
    seg.set_term_index(term_index);
    Ok(seg)
}

// ---- segment file (immutable) ----------------------------------------------

/// Serialize a segment's immutable data: stored fields (no rank/tombstone — those
/// live in sidecars) then the posting lists.
fn encode_segment(seg: &Segment) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(SEG_MAGIC);

    // Per doc: small/eager fields first, then the cold (`text`, `embedding`)
    // fields the loader can skip and decode lazily.
    write_varint(&mut buf, seg.docs.len() as u64);
    for doc in &seg.docs {
        write_str(&mut buf, &doc.url);
        write_str(&mut buf, &doc.title);
        write_varint(&mut buf, doc.len_title as u64);
        write_varint(&mut buf, doc.len_body as u64);
        write_varint(&mut buf, doc.content_hash);
        write_varint(&mut buf, doc.published.max(0) as u64); // 0 = unknown
        write_varint(&mut buf, doc.emb_len as u64); // per-passage dim
        write_varint(&mut buf, doc.n_passages as u64); // passage count
                                                       // --- cold fields (offset recorded by the loader from here) ---
        write_str(&mut buf, &doc.text);
        // Flat passage vectors: n_passages × emb_len f32.
        for &f in &doc.embedding {
            buf.extend_from_slice(&f.to_le_bytes());
        }
    }

    // Term dictionary: each posting list is a length-prefixed blob so the loader
    // can index terms without decoding postings. Sorted for a deterministic
    // (stable content-addressed) encoding.
    let mut terms = seg.all_postings();
    terms.sort_by(|a, b| a.0.cmp(&b.0));
    write_varint(&mut buf, terms.len() as u64);
    for (term, postings) in &terms {
        write_str(&mut buf, term);
        let blob = encode_postings_blob(postings);
        write_varint(&mut buf, blob.len() as u64);
        buf.extend_from_slice(&blob);
    }
    buf
}

/// Encode one term's posting list: count, then delta+varint doc ids / positions.
fn encode_postings_blob(postings: &[Posting]) -> Vec<u8> {
    let mut b = Vec::new();
    write_varint(&mut b, postings.len() as u64);
    let mut prev_doc = 0u64;
    for p in postings {
        let doc_id = p.doc_id as u64;
        write_varint(&mut b, doc_id - prev_doc);
        prev_doc = doc_id;
        write_varint(&mut b, p.tf_title as u64);
        write_varint(&mut b, p.tf_body as u64);
        write_varint(&mut b, p.positions.len() as u64);
        let mut prev_pos = 0u32;
        for &pos in &p.positions {
            write_varint(&mut b, (pos - prev_pos) as u64);
            prev_pos = pos;
        }
    }
    b
}

/// Decode one term's posting blob (the inverse of `encode_postings_blob`).
/// Called lazily by `Segment` on first access to a term.
pub(crate) fn decode_postings(blob: &[u8]) -> Vec<Posting> {
    let mut r = Cursor { data: blob, pos: 0 };
    let count = r.read_varint().unwrap_or(0) as usize;
    let mut out = Vec::with_capacity(count);
    let mut prev_doc = 0u64;
    for _ in 0..count {
        let Ok(gap) = r.read_varint() else { break };
        let doc_id = prev_doc + gap;
        prev_doc = doc_id;
        let tf_title = r.read_varint().unwrap_or(0) as u32;
        let tf_body = r.read_varint().unwrap_or(0) as u32;
        let pos_count = r.read_varint().unwrap_or(0) as usize;
        let mut positions = Vec::with_capacity(pos_count);
        let mut prev_pos = 0u32;
        for _ in 0..pos_count {
            let pos = prev_pos + r.read_varint().unwrap_or(0) as u32;
            positions.push(pos);
            prev_pos = pos;
        }
        out.push(Posting {
            doc_id: doc_id as usize,
            tf_title,
            tf_body,
            positions,
        });
    }
    out
}

/// Decode a doc's cold `text` field at `offset` (start of the cold section).
pub(crate) fn decode_doc_text(bytes: &[u8], offset: usize) -> String {
    let mut r = Cursor {
        data: bytes,
        pos: offset,
    };
    r.read_str().unwrap_or_default()
}

/// Decode a doc's cold passage-embedding blob (`count` f32s = n_passages × dim) —
/// skips `text` first.
pub(crate) fn decode_doc_embedding(bytes: &[u8], offset: usize, count: usize) -> Vec<f32> {
    let mut r = Cursor {
        data: bytes,
        pos: offset,
    };
    let text_len = r.read_varint().unwrap_or(0) as usize;
    r.pos += text_len; // skip text
    let mut v = Vec::with_capacity(count);
    for _ in 0..count {
        match r.read_f32() {
            Ok(f) => v.push(f),
            Err(_) => break,
        }
    }
    v
}

/// Parse a segment file's header: decode the small stored fields (eager), record
/// each doc's cold-field offset for lazy `text`/`embedding`, and build the term
/// dictionary — all without decoding postings or the big stored fields.
#[allow(clippy::type_complexity)]
fn parse_seg_header(
    bytes: &[u8],
) -> io::Result<(Vec<(Document, usize)>, HashMap<String, TermMeta>)> {
    let mut r = Cursor {
        data: bytes,
        pos: 0,
    };
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic)?;
    if &magic != SEG_MAGIC {
        return Err(bad("not an Omni segment file"));
    }

    let doc_count = r.read_varint()? as usize;
    let mut docs = Vec::with_capacity(doc_count);
    for id in 0..doc_count {
        let url = r.read_str()?;
        let title = r.read_str()?;
        let len_title = r.read_varint()? as u32;
        let len_body = r.read_varint()? as u32;
        let content_hash = r.read_varint()?;
        let published = r.read_varint()? as i64;
        let emb_len = r.read_varint()? as u32;
        let n_passages = r.read_varint()? as u32;
        // Cold fields start here; record the offset, then skip them.
        let cold_offset = r.pos;
        let text_len = r.read_varint()? as usize;
        r.pos += text_len; // skip text
        r.pos += emb_len as usize * n_passages as usize * 4; // skip passage f32s
        if r.pos > bytes.len() {
            return Err(bad("segment doc record runs past end of file"));
        }
        docs.push((
            Document {
                id,
                url,
                title,
                text: String::new(),
                len_title,
                len_body,
                rank: 0.0,
                content_hash,
                deleted: false,
                embedding: Vec::new(),
                emb_len,
                n_passages,
                published,
            },
            cold_offset,
        ));
    }

    let term_count = r.read_varint()? as usize;
    let mut term_index = HashMap::with_capacity(term_count);
    for _ in 0..term_count {
        let term = r.read_str()?;
        let blob_len = r.read_varint()? as usize;
        let offset = r.pos;
        if offset + blob_len > bytes.len() {
            return Err(bad("segment term blob runs past end of file"));
        }
        // df = the blob's first varint (posting count) read without decoding.
        let df = Cursor {
            data: &bytes[offset..offset + blob_len],
            pos: 0,
        }
        .read_varint()
        .unwrap_or(0) as usize;
        r.pos += blob_len; // skip the postings blob
        term_index.insert(
            term,
            TermMeta {
                offset,
                len: blob_len,
                df,
            },
        );
    }
    Ok((docs, term_index))
}

// ---- sidecars (mutable) ----------------------------------------------------

/// Write `.del` (bit-packed tombstones) and `.rank` (f64 per doc) — only when
/// they carry information, so an all-live, rank-free segment has no sidecars.
fn write_sidecars(dir: &Path, id: u64, seg: &Segment) -> io::Result<()> {
    let del_path = dir.join(seg_name(id, "del"));
    if seg.docs.iter().any(|d| d.deleted) {
        let mut bits = vec![0u8; seg.docs.len().div_ceil(8)];
        for (i, d) in seg.docs.iter().enumerate() {
            if d.deleted {
                bits[i / 8] |= 1 << (i % 8);
            }
        }
        atomic_write(&del_path, &bits)?;
    } else if del_path.exists() {
        std::fs::remove_file(&del_path)?;
    }

    let rank_path = dir.join(seg_name(id, "rank"));
    if seg.docs.iter().any(|d| d.rank != 0.0) {
        let mut buf = Vec::with_capacity(seg.docs.len() * 8);
        for d in &seg.docs {
            buf.extend_from_slice(&d.rank.to_le_bytes());
        }
        atomic_write(&rank_path, &buf)?;
    } else if rank_path.exists() {
        std::fs::remove_file(&rank_path)?;
    }
    Ok(())
}

/// Apply `.del` then `.rank` to a freshly-decoded segment.
fn apply_sidecars(dir: &Path, id: u64, seg: &mut Segment) -> io::Result<()> {
    if let Ok(bits) = std::fs::read(dir.join(seg_name(id, "del"))) {
        for i in 0..seg.total_docs() {
            if bits.get(i / 8).is_some_and(|b| b & (1 << (i % 8)) != 0) {
                seg.delete_doc(i);
            }
        }
    }
    if let Ok(buf) = std::fs::read(dir.join(seg_name(id, "rank"))) {
        for i in 0..seg.total_docs() {
            let off = i * 8;
            if off + 8 <= buf.len() {
                let r = f64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
                seg.set_rank(i, r);
            }
        }
    }
    Ok(())
}

// ---- manifest --------------------------------------------------------------

fn encode_manifest(emb: &EmbedderConfig, ids: &[u64]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(MANIFEST_MAGIC);
    buf.push(emb.kind);
    write_varint(&mut buf, emb.dim as u64);
    write_str(&mut buf, &emb.url);
    write_str(&mut buf, &emb.model);
    write_varint(&mut buf, ids.len() as u64);
    for &id in ids {
        buf.extend_from_slice(&id.to_le_bytes());
    }
    buf
}

fn decode_manifest(bytes: &[u8]) -> io::Result<(EmbedderConfig, Vec<u64>)> {
    let mut r = Cursor {
        data: bytes,
        pos: 0,
    };
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MANIFEST_MAGIC {
        return Err(bad("not an Omni index directory (bad/old manifest)"));
    }
    let kind = r.read_u8()?;
    let dim = r.read_varint()? as usize;
    let url = r.read_str()?;
    let model = r.read_str()?;
    let n = r.read_varint()? as usize;
    let mut ids = Vec::with_capacity(n);
    for _ in 0..n {
        let mut b = [0u8; 8];
        r.read_exact(&mut b)?;
        ids.push(u64::from_le_bytes(b));
    }
    Ok((
        EmbedderConfig {
            kind,
            dim,
            url,
            model,
        },
        ids,
    ))
}

// ---- file helpers ----------------------------------------------------------

fn seg_name(id: u64, ext: &str) -> String {
    format!("seg-{id:016x}.{ext}")
}

/// Remove `seg-*.{seg,del,rank}` files whose id isn't in the live set.
fn remove_orphans(dir: &Path, live: &[u64]) -> io::Result<()> {
    let live: std::collections::HashSet<u64> = live.iter().copied().collect();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(rest) = name.strip_prefix("seg-") else {
            continue;
        };
        let Some(hex) = rest.split('.').next() else {
            continue;
        };
        if let Ok(id) = u64::from_str_radix(hex, 16) {
            if !live.contains(&id) {
                std::fs::remove_file(&path)?;
            }
        }
    }
    Ok(())
}

/// Write to a temp file then rename, so a reader never sees a partial file.
fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

fn fnv64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

// ---- varint + primitives ---------------------------------------------------

/// LEB128 unsigned varint: 7 data bits per byte, MSB set means "continue".
fn write_varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if v == 0 {
            break;
        }
    }
}

fn write_str(buf: &mut Vec<u8>, s: &str) {
    write_varint(buf, s.len() as u64);
    buf.extend_from_slice(s.as_bytes());
}

/// A tiny byte cursor — avoids pulling in a dependency just to track an offset.
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn read_exact(&mut self, out: &mut [u8]) -> io::Result<()> {
        let end = self.pos + out.len();
        if end > self.data.len() {
            return Err(bad("unexpected end of file"));
        }
        out.copy_from_slice(&self.data[self.pos..end]);
        self.pos = end;
        Ok(())
    }

    fn read_u8(&mut self) -> io::Result<u8> {
        let mut b = [0u8; 1];
        self.read_exact(&mut b)?;
        Ok(b[0])
    }

    fn read_f32(&mut self) -> io::Result<f32> {
        let mut b = [0u8; 4];
        self.read_exact(&mut b)?;
        Ok(f32::from_le_bytes(b))
    }

    fn read_varint(&mut self) -> io::Result<u64> {
        let mut result = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = self.read_u8()?;
            result |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift >= 64 {
                return Err(bad("varint too long"));
            }
        }
        Ok(result)
    }

    fn read_str(&mut self) -> io::Result<String> {
        let len = self.read_varint()? as usize;
        let end = self.pos + len;
        if end > self.data.len() {
            return Err(bad("string runs past end of file"));
        }
        let s = String::from_utf8_lossy(&self.data[self.pos..end]).into_owned();
        self.pos = end;
        Ok(s)
    }
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query;

    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn varint_round_trips() {
        for v in [0u64, 1, 127, 128, 300, 16384, u32::MAX as u64, u64::MAX] {
            let mut buf = Vec::new();
            write_varint(&mut buf, v);
            let mut c = Cursor { data: &buf, pos: 0 };
            assert_eq!(c.read_varint().unwrap(), v);
        }
    }

    #[test]
    fn index_round_trips_through_disk() {
        let mut idx = Index::new();
        idx.add_document("a".into(), "Rust".into(), "rust systems language rust");
        idx.add_document("b".into(), "Go".into(), "go concurrent language");
        idx.set_ranks(&[0.7, 0.3]);

        let dir = tmp("omni-test-roundtrip.idx");
        save(&idx, &dir).unwrap();
        let loaded = load(&dir).unwrap();

        assert_eq!(loaded.doc_count(), 2);
        assert!((loaded.segments()[0].docs[0].rank - 0.7).abs() < 1e-9);
        let before = query::search(&idx, "rust language", 10);
        let after = query::search(&loaded, "rust language", 10);
        assert_eq!(before.len(), after.len());
        assert_eq!(after[0].title, "Rust");
        assert!((before[0].score - after[0].score).abs() < 1e-9);
        // mmap load is equivalent.
        let mapped = load_mmap(&dir).unwrap();
        assert_eq!(query::search(&mapped, "rust language", 10)[0].title, "Rust");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn embeddings_and_tombstones_round_trip() {
        use crate::embed::{Embedder, EmbedderConfig};

        let mut idx = Index::new();
        idx.add_document("a".into(), "Rust".into(), "rust ownership borrowing");
        idx.add_document("b".into(), "Go".into(), "go goroutines channels");
        idx.add_document("c".into(), "Old".into(), "stale page to be removed");
        idx.delete_by_url("c");
        let cfg = EmbedderConfig::parse("hash:64", None).unwrap();
        idx.set_embedder(cfg.clone());
        idx.embed_missing(&Embedder::from_config(&cfg).unwrap());

        let dir = tmp("omni-test-dir-v6.idx");
        save(&idx, &dir).unwrap();
        let loaded = load(&dir).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(loaded.doc_count(), 2, "tombstone preserved");
        assert!(loaded.embedder().enabled());
        assert_eq!(loaded.embedder().dim, 64);
        assert_eq!(loaded.segments()[0].docs[0].emb_len, 64);
        assert_eq!(
            loaded.segments()[0].embedding(0).len(),
            64,
            "embedding decodes lazily"
        );
        assert!(loaded.segments()[0].docs[2].deleted);
        let hits = query::search(&loaded, "ownership", 10);
        assert!(hits.iter().all(|h| h.title != "Old"));
        assert_eq!(hits[0].title, "Rust");
    }

    #[test]
    fn flush_appends_segment_file_without_rewriting_old() {
        // Build one segment, save. Add a second segment, save again. The first
        // segment's file must be untouched (same mtime) — only the new file and
        // the manifest change.
        let dir = tmp("omni-test-append.idx");
        let mut idx = Index::new();
        idx.add_document("a".into(), "A".into(), "alpha beta");
        save(&idx, &dir).unwrap();

        let first: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".seg"))
            .map(|e| (e.path(), e.metadata().unwrap().modified().unwrap()))
            .collect();
        assert_eq!(first.len(), 1);
        let (first_path, first_mtime) = first[0].clone();

        idx.begin_segment();
        idx.add_document("b".into(), "B".into(), "gamma delta");
        save(&idx, &dir).unwrap();

        // The original .seg still exists, unchanged; a new .seg appeared.
        let segs: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".seg"))
            .map(|e| e.path())
            .collect();
        assert_eq!(segs.len(), 2, "second flush appended a new segment file");
        assert!(first_path.exists(), "original segment file kept");
        assert_eq!(
            std::fs::metadata(&first_path).unwrap().modified().unwrap(),
            first_mtime,
            "original segment file was not rewritten"
        );

        let loaded = load(&dir).unwrap();
        assert_eq!(loaded.doc_count(), 2);
        assert_eq!(loaded.segment_count(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn compaction_removes_orphan_files() {
        let dir = tmp("omni-test-orphans.idx");
        let mut idx = Index::new();
        idx.add_document("a".into(), "A".into(), "alpha");
        idx.begin_segment();
        idx.add_document("b".into(), "B".into(), "beta");
        save(&idx, &dir).unwrap();
        assert_eq!(seg_files(&dir), 2);

        idx.compact();
        save(&idx, &dir).unwrap();
        assert_eq!(
            seg_files(&dir),
            1,
            "orphan segment files removed after compaction"
        );

        let loaded = load(&dir).unwrap();
        assert_eq!(loaded.doc_count(), 2);
        assert_eq!(loaded.segment_count(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn seg_files(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".seg"))
            .count()
    }

    #[test]
    fn lazy_load_holds_bytes_and_decodes_on_demand() {
        let mut idx = Index::new();
        idx.add_document("a".into(), "Rust".into(), "rust ownership borrowing rust");
        idx.add_document("b".into(), "Go".into(), "go channels");
        let dir = tmp("omni-test-lazy.idx");
        save(&idx, &dir).unwrap();
        let loaded = load(&dir).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        let seg = &loaded.segments()[0];
        // Loaded segment is lazy: it holds the raw file bytes, not a decoded map.
        assert!(seg.raw_bytes().is_some());
        // Document frequency comes from the term dictionary (no posting decode).
        assert_eq!(seg.term_df("rust"), 1, "only doc 0 has 'rust'");
        assert_eq!(seg.term_df("go"), 1);
        // Decoding a term on demand yields the correct posting.
        let p = seg.postings("rust").unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].doc_id, 0);
        assert_eq!(p[0].tf_title, 1, "title 'Rust'");
        assert_eq!(p[0].tf_body, 2, "body has rust twice");
    }
}
