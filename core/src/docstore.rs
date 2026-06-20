//! Reading the doc store produced by the Go crawler (PLAN.md §7), and applying
//! it to the index — both for a full build (`load_dir`) and an incremental
//! refresh (`update`).
//!
//! Each `*.doc` file is one crawled page in an RFC822-style format: header lines
//! (`key: value`), a blank line, then the raw body text. We parse the headers we
//! care about (`url`, `title`, `links`, `image`) and index the body. The `links:`
//! header is the link graph that PageRank runs over; repeated `image:` headers
//! feed the Images vertical.
//!
//! The cross-language boundary stays dependency-free: no JSON parser, no serde —
//! just line splitting, readable in both Go and Rust.

use crate::index::{self, Index};
use crate::pagerank;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

/// A parsed doc-store record.
pub struct Record {
    pub url: String,
    pub title: String,
    pub links: Vec<String>,
    pub images: Vec<crate::index::Image>,
    pub text: String,
    /// Publish time as unix seconds (0 = unknown), from the `published:` header.
    pub published: i64,
}

/// Result of an incremental update, for reporting.
#[derive(Default)]
pub struct UpdateStats {
    pub added: usize,
    pub changed: usize,
    pub deleted: usize,
    pub unchanged: usize,
}

/// Full build: index every `*.doc` in `dir`, then compute PageRank over the link
/// graph. Returns the number of records read.
pub fn load_dir(index: &mut Index, dir: &Path) -> std::io::Result<usize> {
    let records = read_records(dir)?;
    for rec in &records {
        index.add_document_with_images(
            rec.url.clone(),
            title_or_url(rec),
            &rec.text,
            rec.images.clone(),
        );
        if rec.published != 0 {
            index.set_published(&rec.url, rec.published);
        }
    }
    recompute_pagerank(index, &records);
    Ok(records.len())
}

/// Incremental update: diff the current doc store against the loaded index.
///   * url not indexed            → add
///   * url indexed, content changed → tombstone the old doc, add the new one
///   * url indexed, content same   → leave it
///   * indexed url no longer in store → tombstone
/// Then recompute PageRank over the live link graph. Tombstones accumulate until
/// a full rebuild (compaction) reclaims their space.
pub fn update(index: &mut Index, dir: &Path) -> std::io::Result<UpdateStats> {
    let records = read_records(dir)?;
    let mut stats = UpdateStats::default();
    let mut seen: HashSet<String> = HashSet::with_capacity(records.len());

    // New/changed docs land in a *fresh* segment — existing segments aren't
    // rewritten, only tombstoned where a url changed or vanished.
    index.begin_segment();

    for rec in &records {
        seen.insert(rec.url.clone());
        let title = title_or_url(rec);
        let new_hash = index::content_hash(&title, &index_text(&rec.text, &rec.images));
        match index.addr_for_url(&rec.url) {
            Some((s, l)) if index.segments()[s].docs[l].content_hash == new_hash => {
                stats.unchanged += 1
            }
            Some(_) => {
                index.delete_by_url(&rec.url);
                index.add_document_with_images(
                    rec.url.clone(),
                    title,
                    &rec.text,
                    rec.images.clone(),
                );
                stats.changed += 1;
            }
            None => {
                index.add_document_with_images(
                    rec.url.clone(),
                    title,
                    &rec.text,
                    rec.images.clone(),
                );
                stats.added += 1;
            }
        }
        if rec.published != 0 {
            index.set_published(&rec.url, rec.published);
        }
    }

    // Pages dropped from the store → tombstone. Collect urls first so we don't
    // mutate the index while reading its url map.
    let gone: Vec<String> = index
        .live_entries()
        .into_iter()
        .filter(|(url, _)| !seen.contains(url))
        .map(|(url, _)| url)
        .collect();
    for url in gone {
        index.delete_by_url(&url);
        stats.deleted += 1;
    }

    recompute_pagerank(index, &records);
    Ok(stats)
}

/// Build the link graph from the current store records and assign PageRank over
/// the full (segment-major) global doc space. Edges are kept only between live
/// documents; tombstoned/absent targets are dropped.
fn recompute_pagerank(index: &mut Index, records: &[Record]) {
    let n = index.total_docs();
    let mut out_edges: Vec<Vec<usize>> = vec![Vec::new(); n];
    for rec in records {
        if let Some(from) = index.url_to_global(&rec.url) {
            for link in &rec.links {
                if let Some(to) = index.url_to_global(link) {
                    out_edges[from].push(to);
                }
            }
        }
    }
    let ranks = pagerank::compute(n, &out_edges);
    index.set_ranks(&ranks);
}

/// Read and parse every `*.doc` file in `dir`.
fn read_records(dir: &Path) -> std::io::Result<Vec<Record>> {
    let mut records = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let is_doc = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("doc"))
            .unwrap_or(false);
        if !is_doc {
            continue;
        }
        let raw = fs::read_to_string(&path)?;
        if let Some(rec) = parse(&raw) {
            records.push(rec);
        }
    }
    Ok(records)
}

fn title_or_url(rec: &Record) -> String {
    if rec.title.is_empty() {
        rec.url.clone()
    } else {
        rec.title.clone()
    }
}

/// Parse one record: headers until the first blank line, then the body.
pub fn parse(raw: &str) -> Option<Record> {
    let (header_block, body) = match raw.split_once("\n\n") {
        Some((h, b)) => (h, b),
        None => (raw, ""),
    };

    let mut url = String::new();
    let mut title = String::new();
    let mut links = Vec::new();
    let mut images = Vec::new();
    let mut published = 0i64;

    for line in header_block.lines() {
        let Some((key, val)) = line.split_once(':') else {
            continue;
        };
        let val = val.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "url" => url = val.to_string(),
            "title" => title = val.to_string(),
            "links" => links = val.split_whitespace().map(|s| s.to_string()).collect(),
            "image" => {
                if let Some(img) = parse_image_header(val) {
                    images.push(img);
                }
            }
            "published" => published = parse_published(val),
            _ => {}
        }
    }

    if url.is_empty() {
        return None;
    }
    Some(Record {
        url,
        title,
        links,
        images,
        text: body.to_string(),
        published,
    })
}

fn parse_image_header(val: &str) -> Option<crate::index::Image> {
    let (url, alt) = val.split_once('\t').unwrap_or((val, ""));
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    Some(crate::index::Image {
        url: url.to_string(),
        alt: alt.trim().to_string(),
    })
}

fn index_text(text: &str, images: &[crate::index::Image]) -> String {
    if images.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + images.len() * 64);
    out.push_str(text);
    for img in images {
        out.push(' ');
        out.push_str(&img.alt);
        out.push(' ');
        out.push_str(&img.url);
    }
    out
}

/// Parse a publish date (ISO-8601-ish: `YYYY-MM-DD`, optionally with a time) into
/// unix seconds at day granularity. Returns 0 if it can't read a plausible date —
/// so an absent/garbled date is simply "unknown", never a spurious timestamp.
pub fn parse_published(s: &str) -> i64 {
    let mut nums = s.split(|c: char| !c.is_ascii_digit());
    let y: i64 = match nums.next().and_then(|x| x.parse().ok()) {
        Some(y) if (1970..=3000).contains(&y) => y,
        _ => return 0,
    };
    let m: i64 = nums
        .next()
        .and_then(|x| x.parse().ok())
        .unwrap_or(1)
        .clamp(1, 12);
    let d: i64 = nums
        .next()
        .and_then(|x| x.parse().ok())
        .unwrap_or(1)
        .clamp(1, 31);
    days_from_civil(y, m, d) * 86_400
}

/// Days since the unix epoch for a civil (y, m, d) date — Howard Hinnant's
/// well-known branch-free algorithm (no chrono dependency).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_headers_and_body() {
        let raw = "url: https://example.com/\n\
                   title: Example\n\
                   status: 200\n\
                   image: https://example.com/diagram.png\tExample diagram\n\
                   links: https://example.com/a https://example.com/b\n\
                   \n\
                   Hello world body text.\n";
        let rec = parse(raw).expect("should parse");
        assert_eq!(rec.url, "https://example.com/");
        assert_eq!(rec.title, "Example");
        assert_eq!(rec.links.len(), 2);
        assert_eq!(rec.images.len(), 1);
        assert_eq!(rec.images[0].url, "https://example.com/diagram.png");
        assert_eq!(rec.images[0].alt, "Example diagram");
        assert!(rec.text.contains("Hello world"));
    }

    #[test]
    fn rejects_record_without_url() {
        let raw = "title: No URL\n\nbody";
        assert!(parse(raw).is_none());
    }

    #[test]
    fn parses_publish_dates() {
        // 1970-01-01 is epoch day 0.
        assert_eq!(parse_published("1970-01-01"), 0);
        assert_eq!(parse_published("1970-01-02"), 86_400);
        // ISO-8601 with time/zone — day granularity, ignores the time.
        assert_eq!(
            parse_published("2026-06-12T02:13:54.000Z"),
            parse_published("2026-06-12")
        );
        // Known reference: 2000-01-01 = 10957 days after epoch.
        assert_eq!(parse_published("2000-01-01"), 10957 * 86_400);
        // Garbage / out-of-range ⇒ unknown (0), never a spurious date.
        assert_eq!(parse_published("not a date"), 0);
        assert_eq!(parse_published(""), 0);
        assert_eq!(parse_published("1850-01-01"), 0);

        // End-to-end: the header is read into the record.
        let rec = parse("url: http://x\ntitle: T\npublished: 2024-03-15\n\nbody").unwrap();
        assert_eq!(rec.published, parse_published("2024-03-15"));
        assert!(rec.published > 0);
    }

    #[test]
    fn incremental_update_add_change_delete() {
        use crate::index::Index;
        use crate::query;

        let dir = std::env::temp_dir().join("omni-update-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, url: &str, title: &str, body: &str| {
            fs::write(
                dir.join(name),
                format!("url: {url}\ntitle: {title}\n\n{body}"),
            )
            .unwrap();
        };

        write("a.doc", "http://x/a", "Alpha", "the alpha topic about rust");
        write("b.doc", "http://x/b", "Beta", "the beta topic about golang");
        let mut idx = Index::new();
        load_dir(&mut idx, &dir).unwrap();
        assert_eq!(idx.doc_count(), 2);

        // Change a's content, add c, remove b.
        write(
            "a.doc",
            "http://x/a",
            "Alpha",
            "the alpha topic about rust and search engines",
        );
        write("c.doc", "http://x/c", "Gamma", "gamma about kubernetes");
        fs::remove_file(dir.join("b.doc")).unwrap();

        let s = update(&mut idx, &dir).unwrap();
        assert_eq!(s.added, 1, "c added");
        assert_eq!(s.changed, 1, "a changed");
        assert_eq!(s.deleted, 1, "b removed");
        assert_eq!(s.unchanged, 0);

        assert_eq!(idx.doc_count(), 2, "live = new-a + c");
        assert_eq!(
            idx.total_docs(),
            4,
            "old-a, b, new-a, c (tombstones retained)"
        );

        // Search reflects the changes.
        assert_eq!(
            query::search(&idx, "kubernetes", 10).len(),
            1,
            "added doc searchable"
        );
        assert!(
            query::search(&idx, "golang", 10).is_empty(),
            "deleted doc gone"
        );
        assert_eq!(
            query::search(&idx, "engines", 10).len(),
            1,
            "changed content searchable"
        );

        // A no-op update reports everything unchanged.
        let s2 = update(&mut idx, &dir).unwrap();
        assert_eq!(
            (s2.added, s2.changed, s2.deleted, s2.unchanged),
            (0, 0, 0, 2)
        );

        fs::remove_dir_all(&dir).ok();
    }
}
