//! Embeddings for semantic / hybrid retrieval.
//!
//! Lexical BM25F matches words; **semantic** retrieval matches *meaning*, so a
//! query can find documents that share no query terms. That needs a vector
//! embedding of text plus nearest-neighbour search. This module provides a
//! pluggable `Embedder`:
//!
//!   * **Hash** — a self-contained, offline, deterministic feature-hashing
//!     embedder (bag of stemmed terms projected into a fixed space). It's not
//!     *learned* semantics — cosine ≈ weighted term overlap — but it exercises
//!     the whole hybrid pipeline with zero dependencies and is great for tests.
//!   * **Http** — a hand-rolled client for an Ollama-style embeddings endpoint
//!     (`POST {model, prompt}` → `{embedding:[…]}`). This gives *real* learned
//!     semantics when a local model is running. Plain HTTP only (std has no TLS).
//!
//! Retrieval fuses the lexical and semantic rankings with Reciprocal Rank Fusion
//! (`query.rs`). Vectors are L2-normalized, so cosine similarity is a dot product.

use crate::analyze;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

pub const KIND_NONE: u8 = 0;
pub const KIND_HASH: u8 = 1;
pub const KIND_HTTP: u8 = 2;

/// Persisted embedder configuration (stored in the index so serving rebuilds the
/// matching embedder without CLI flags).
#[derive(Clone)]
pub struct EmbedderConfig {
    pub kind: u8,
    pub dim: usize,
    pub url: String,
    pub model: String,
}

impl EmbedderConfig {
    pub fn none() -> Self {
        EmbedderConfig {
            kind: KIND_NONE,
            dim: 0,
            url: String::new(),
            model: String::new(),
        }
    }
    pub fn enabled(&self) -> bool {
        self.kind != KIND_NONE
    }

    /// Origin (`scheme://host[:port]`) of the Ollama-style host, for sibling
    /// endpoints like `/api/chat` (the reranker). Derived from the embedder URL
    /// when it's HTTP; falls back to localhost:11434.
    pub fn host_base(&self) -> String {
        if let Some(rest) = self.url.strip_prefix("http://") {
            let host = rest.split('/').next().unwrap_or("");
            if !host.is_empty() {
                return format!("http://{host}");
            }
        }
        "http://localhost:11434".to_string()
    }

    /// Human-readable embedder kind (for stats / the dashboard).
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            KIND_HASH => "hash",
            KIND_HTTP => "http",
            _ => "none",
        }
    }

    /// Parse a `--embed` spec: `off` | `hash[:DIM]` | `ollama` | a full
    /// `http://…` URL. `model_override` sets the model for the HTTP kinds.
    pub fn parse(spec: &str, model_override: Option<&str>) -> Result<Self, String> {
        let model = model_override.unwrap_or("nomic-embed-text").to_string();
        if spec == "off" {
            Ok(Self::none())
        } else if spec == "hash" {
            Ok(EmbedderConfig {
                kind: KIND_HASH,
                dim: 256,
                ..Self::none()
            })
        } else if let Some(d) = spec.strip_prefix("hash:") {
            let dim = d.parse().map_err(|_| format!("bad hash dim: {d}"))?;
            Ok(EmbedderConfig {
                kind: KIND_HASH,
                dim,
                ..Self::none()
            })
        } else if spec == "ollama" {
            Ok(EmbedderConfig {
                kind: KIND_HTTP,
                dim: 0, // discovered from the first embedding
                url: "http://localhost:11434/api/embeddings".into(),
                model,
            })
        } else if spec.starts_with("http://") {
            Ok(EmbedderConfig {
                kind: KIND_HTTP,
                dim: 0,
                url: spec.to_string(),
                model,
            })
        } else {
            Err(format!(
                "unknown --embed spec: {spec:?} (use off|hash|hash:DIM|ollama|http://…)"
            ))
        }
    }
}

/// A ready-to-use embedder built from a config.
pub enum Embedder {
    Hash { dim: usize },
    Http { url: String, model: String },
}

impl Embedder {
    pub fn from_config(cfg: &EmbedderConfig) -> Option<Embedder> {
        match cfg.kind {
            KIND_HASH => Some(Embedder::Hash {
                dim: cfg.dim.max(1),
            }),
            KIND_HTTP => Some(Embedder::Http {
                url: cfg.url.clone(),
                model: cfg.model.clone(),
            }),
            _ => None,
        }
    }

    /// Embed text into an L2-normalized vector. Returns an empty vector on
    /// failure (e.g. the HTTP embedder can't reach the model) so callers can
    /// gracefully fall back to lexical-only retrieval.
    pub fn embed(&self, text: &str) -> Vec<f32> {
        match self {
            Embedder::Hash { dim } => embed_hash(text, *dim),
            Embedder::Http { url, model } => http_embed(url, model, text)
                .map(|mut v| {
                    normalize(&mut v);
                    v
                })
                .unwrap_or_default(),
        }
    }

    /// Embed a **document** for indexing (applies the model's document task
    /// prefix where one is needed).
    pub fn embed_doc(&self, text: &str) -> Vec<f32> {
        self.embed(&self.with_prefix(text, false))
    }

    /// Embed a **query** for retrieval (applies the model's query task prefix).
    /// Asymmetric prefixes matter for instruction-tuned embedders: a query and the
    /// documents it should match are encoded differently.
    pub fn embed_query(&self, text: &str) -> Vec<f32> {
        self.embed(&self.with_prefix(text, true))
    }

    /// Prepend the model-specific task prefix. `nomic-embed-text` was trained with
    /// `search_query:` / `search_document:` instructions and needs them for good
    /// retrieval; other models (and the hash embedder) get the text unchanged.
    fn with_prefix(&self, text: &str, is_query: bool) -> String {
        let prefix = match self {
            Embedder::Http { model, .. } if model.to_ascii_lowercase().contains("nomic") => {
                if is_query {
                    "search_query: "
                } else {
                    "search_document: "
                }
            }
            _ => "",
        };
        format!("{prefix}{text}")
    }
}

/// Cosine similarity of two L2-normalized vectors (a dot product). 0 if either
/// is empty or lengths differ.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

// ---- hashing embedder ------------------------------------------------------

fn embed_hash(text: &str, dim: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; dim];
    for term in analyze::analyze_terms(text) {
        let h = fnv1a(term.as_bytes());
        let bucket = (h % dim as u64) as usize;
        let sign = if (h >> 63) & 1 == 0 { 1.0 } else { -1.0 };
        v[bucket] += sign;
    }
    normalize(&mut v);
    v
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

// ---- HTTP (Ollama-style) embedder ------------------------------------------

/// POST to an Ollama-compatible `/api/embeddings` endpoint and parse the vector.
/// Plain HTTP (std has no TLS). Uses **HTTP/1.0** so the response is close-
/// delimited (Ollama otherwise replies `Transfer-Encoding: chunked`, which would
/// inject chunk-size lines mid-array); a chunked body is still decoded as a
/// fallback. Timeouts keep a slow/unreachable model from stalling a whole build —
/// any error returns `None`, so the caller falls back to lexical-only.
fn http_embed(url: &str, model: &str, text: &str) -> Option<Vec<f32>> {
    let body = format!(
        "{{\"model\":{},\"prompt\":{}}}",
        json_str(model),
        json_str(text)
    );
    let resp = http_post_json(url, &body, 60)?;
    parse_embedding(&resp)
}

/// POST a JSON `body` to an Ollama-style HTTP endpoint and return the response
/// **body** (chunked transfers decoded). Plain HTTP/1.0 (std has no TLS); a short
/// connect timeout fails fast if the model is unreachable, and `read_secs` is
/// generous because the first call may load the model. Shared by the embedder and
/// the reranker; `None` on any error.
pub(crate) fn http_post_json(url: &str, body: &str, read_secs: u64) -> Option<String> {
    let rest = url.strip_prefix("http://")?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().unwrap_or(80)),
        None => (hostport, 80),
    };
    let req = format!(
        "POST {path} HTTP/1.0\r\nHost: {host}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let addr = (host, port).to_socket_addrs().ok()?.next()?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(read_secs)))
        .ok()?;
    stream.write_all(req.as_bytes()).ok()?;

    let mut resp = Vec::new();
    stream.read_to_end(&mut resp).ok()?;
    let resp = String::from_utf8_lossy(&resp);
    let hdr_end = resp.find("\r\n\r\n")? + 4;
    let (head, body) = (&resp[..hdr_end], &resp[hdr_end..]);
    Some(
        if head
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
        {
            dechunk(body)
        } else {
            body.to_string()
        },
    )
}

/// Decode an HTTP/1.1 chunked transfer body into its payload. Each chunk is a hex
/// length line, then that many bytes, then CRLF; a zero-length chunk ends it.
fn dechunk(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(nl) = rest.find("\r\n") {
        let size_tok = rest[..nl].trim().split(';').next().unwrap_or("");
        let size = usize::from_str_radix(size_tok.trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        let start = nl + 2;
        let end = (start + size).min(rest.len());
        out.push_str(&rest[start..end]);
        // Skip the data and its trailing CRLF.
        rest = rest.get(end + 2..).unwrap_or("");
    }
    out
}

/// Pull the float array out of `{"embedding":[...]}` (tolerant of surrounding JSON).
fn parse_embedding(json: &str) -> Option<Vec<f32>> {
    let key = json.find("\"embedding\"")?;
    let lb = json[key..].find('[')? + key;
    let rb = json[lb..].find(']')? + lb;
    let mut v = Vec::new();
    for part in json[lb + 1..rb].split(',') {
        let t = part.trim();
        if t.is_empty() {
            continue;
        }
        v.push(t.parse::<f32>().ok()?);
    }
    (!v.is_empty()).then_some(v)
}

/// Minimal JSON string literal encoder for the request body.
pub(crate) fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_embeddings_are_normalized_and_deterministic() {
        let e = Embedder::Hash { dim: 64 };
        let a = e.embed("the rust programming language");
        let b = e.embed("the rust programming language");
        assert_eq!(a, b, "deterministic");
        let norm: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "L2-normalized");
    }

    #[test]
    fn similar_text_scores_higher_than_unrelated() {
        let e = Embedder::Hash { dim: 512 };
        let q = e.embed("rust ownership and borrowing");
        let near = e.embed("rust ownership borrowing rules");
        let far = e.embed("italian pasta recipes with tomato");
        assert!(cosine(&q, &near) > cosine(&q, &far));
    }

    #[test]
    fn parse_embedding_reads_array() {
        let v = parse_embedding("{\"embedding\":[0.5, -0.25, 1e-2],\"model\":\"x\"}").unwrap();
        assert_eq!(v, vec![0.5, -0.25, 0.01]);
    }

    #[test]
    fn dechunk_reassembles_split_body() {
        // A chunked body that splits the JSON across chunks (the case that breaks
        // a naive read): "{"embedding":[1.0," + "2.0]}".
        let chunked = "12\r\n{\"embedding\":[1.0,\r\n5\r\n2.0]}\r\n0\r\n\r\n";
        let body = dechunk(chunked);
        assert_eq!(body, "{\"embedding\":[1.0,2.0]}");
        assert_eq!(parse_embedding(&body).unwrap(), vec![1.0, 2.0]);
    }

    #[test]
    fn config_parse() {
        assert!(!EmbedderConfig::parse("off", None).unwrap().enabled());
        assert_eq!(EmbedderConfig::parse("hash", None).unwrap().dim, 256);
        assert_eq!(EmbedderConfig::parse("hash:128", None).unwrap().dim, 128);
        assert_eq!(
            EmbedderConfig::parse("ollama", None).unwrap().kind,
            KIND_HTTP
        );
        assert!(EmbedderConfig::parse("nonsense", None).is_err());
    }
}
