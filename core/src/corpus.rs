//! Loading documents into the index.
//!
//! Phase 1 indexes local `.html` files from a directory (PLAN.md crawl scope:
//! "local-docs-only first"). HTML handling here is deliberately crude — a
//! regex-free tag stripper using only std — because a real parser (and the Go
//! crawler that feeds it) arrives in Phase 2. The goal now is end-to-end search.

use crate::index::Index;
use std::fs;
use std::path::Path;

/// Index every `.html`/`.htm` file directly inside `dir`.
/// Returns the number of documents added.
pub fn load_dir(index: &mut Index, dir: &Path) -> std::io::Result<usize> {
    let mut count = 0;
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let is_html = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("html") || e.eq_ignore_ascii_case("htm"))
            .unwrap_or(false);
        if !is_html {
            continue;
        }
        let raw = fs::read_to_string(&path)?;
        let title = extract_title(&raw).unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("untitled")
                .to_string()
        });
        let body = strip_html(&raw);
        // For a local corpus the "URL" is the file path; the crawler supplies
        // real URLs in Phase 2.
        let url = format!("file://{}", path.display());
        index.add_document(url, title, &body);
        count += 1;
    }
    Ok(count)
}

/// Pull the contents of the first `<title>...</title>`.
fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let start = lower.find("<title>")? + "<title>".len();
    let end = lower[start..].find("</title>")? + start;
    let title = html[start..end].trim().to_string();
    if title.is_empty() {
        None
    } else {
        Some(title)
    }
}

/// Remove `<script>`/`<style>`/`<title>` blocks and all tags, leaving visible
/// body text (the title is indexed separately, so we keep it out of the body).
/// Not robust against malformed HTML — that's the crawler/parser's job later.
fn strip_html(html: &str) -> String {
    let without_blocks = remove_block(
        &remove_block(&remove_block(html, "script"), "style"),
        "title",
    );

    let mut out = String::with_capacity(without_blocks.len());
    let mut in_tag = false;
    for c in without_blocks.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Drop everything from `<tag ...>` to `</tag>` (case-insensitive).
fn remove_block(html: &str, tag: &str) -> String {
    let lower = html.to_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::new();
    let mut cursor = 0;
    while let Some(rel) = lower[cursor..].find(&open) {
        let start = cursor + rel;
        out.push_str(&html[cursor..start]);
        match lower[start..].find(&close) {
            Some(rel_end) => cursor = start + rel_end + close.len(),
            None => {
                cursor = html.len();
                break;
            }
        }
    }
    out.push_str(&html[cursor..]);
    out
}
