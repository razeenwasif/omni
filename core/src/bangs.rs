//! "Bang" shortcuts and curated **essential sites** — the *necessity* category.
//!
//! Omni's index is academic/reference content. The sites you *act through* rather
//! than search within — YouTube, GitHub, Overleaf, … — are modelled two ways from
//! one table (`SITES`), so there's a single source of truth:
//!
//!   * **Bangs** (`resolve`): a query whose first token is `!<key>` redirects to
//!     that site's own search (DuckDuckGo-style). `!gh rust async` →
//!     GitHub repo search; a bare `!yt` → YouTube's home. The `!` prefix keeps
//!     ordinary searches (`github actions`) hitting the index.
//!   * **Launch cards** (`cards`): each site is also indexed as a small searchable
//!     document, so a plain search for `overleaf` surfaces a card you can click.
//!
//! Bangs are resolved at the Omni layer, so they work regardless of Flux's own
//! omnibox keyword shortcuts (which stay as a convenience); both can coexist.

/// One essential site: its bang keys, display name, home URL, a search-URL
/// template (`{q}` is replaced with the url-encoded query), and a blurb used for
/// the indexed launch card.
pub struct Site {
    pub keys: &'static [&'static str],
    pub name: &'static str,
    pub home: &'static str,
    pub search: &'static str,
    pub blurb: &'static str,
}

/// The curated set. Keys must be lowercase. Order is the display order on the
/// dashboard.
pub const SITES: &[Site] = &[
    Site {
        keys: &["yt", "youtube"],
        name: "YouTube",
        home: "https://www.youtube.com",
        search: "https://www.youtube.com/results?search_query={q}",
        blurb: "video lectures, talks, and tutorials",
    },
    Site {
        keys: &["gh", "github"],
        name: "GitHub",
        home: "https://github.com",
        search: "https://github.com/search?q={q}&type=repositories",
        blurb: "source code, repositories, and projects",
    },
    Site {
        keys: &["ol", "overleaf"],
        name: "Overleaf",
        home: "https://www.overleaf.com",
        search: "https://www.overleaf.com/latex/templates?q={q}",
        blurb: "collaborative LaTeX papers and templates",
    },
    Site {
        keys: &["so", "stackoverflow"],
        name: "Stack Overflow",
        home: "https://stackoverflow.com",
        search: "https://stackoverflow.com/search?q={q}",
        blurb: "programming questions and answers",
    },
    Site {
        keys: &["w", "wiki", "wikipedia"],
        name: "Wikipedia",
        home: "https://en.wikipedia.org",
        search: "https://en.wikipedia.org/w/index.php?search={q}",
        blurb: "the free encyclopedia",
    },
    Site {
        keys: &["ax", "arxiv"],
        name: "arXiv",
        home: "https://arxiv.org",
        search: "https://arxiv.org/search/?query={q}&searchtype=all",
        blurb: "open-access e-prints in physics, math, and CS",
    },
    Site {
        keys: &["sc", "scholar"],
        name: "Google Scholar",
        home: "https://scholar.google.com",
        search: "https://scholar.google.com/scholar?q={q}",
        blurb: "scholarly papers and citations",
    },
    Site {
        keys: &["wa", "wolfram"],
        name: "Wolfram Alpha",
        home: "https://www.wolframalpha.com",
        search: "https://www.wolframalpha.com/input?i={q}",
        blurb: "computational answers and math",
    },
    Site {
        keys: &["mdn"],
        name: "MDN Web Docs",
        home: "https://developer.mozilla.org",
        search: "https://developer.mozilla.org/en-US/search?q={q}",
        blurb: "web platform and JavaScript reference",
    },
    // ---- general sites people visit a lot --------------------------------------
    // Professional / communities
    Site {
        keys: &["li", "linkedin"],
        name: "LinkedIn",
        home: "https://www.linkedin.com",
        search: "https://www.linkedin.com/search/results/all/?keywords={q}",
        blurb: "professional network, jobs, and people",
    },
    Site {
        keys: &["md", "medium"],
        name: "Medium",
        home: "https://medium.com",
        search: "https://medium.com/search?q={q}",
        blurb: "articles, essays, and technical blogs",
    },
    Site {
        keys: &["kg", "kaggle"],
        name: "Kaggle",
        home: "https://www.kaggle.com",
        search: "https://www.kaggle.com/search?q={q}",
        blurb: "datasets, notebooks, and ML competitions",
    },
    Site {
        keys: &["r", "reddit"],
        name: "Reddit",
        home: "https://www.reddit.com",
        search: "https://www.reddit.com/search/?q={q}",
        blurb: "communities and discussion threads",
    },
    Site {
        keys: &["hn", "hackernews"],
        name: "Hacker News",
        home: "https://news.ycombinator.com",
        search: "https://hn.algolia.com/?q={q}",
        blurb: "startup and technology news",
    },
    Site {
        keys: &["dev", "devto"],
        name: "DEV Community",
        home: "https://dev.to",
        search: "https://dev.to/search?q={q}",
        blurb: "developer articles and discussion",
    },
    // AI / ML
    Site {
        keys: &["hf", "huggingface"],
        name: "Hugging Face",
        home: "https://huggingface.co",
        search: "https://huggingface.co/search/full-text?q={q}",
        blurb: "models, datasets, and ML spaces",
    },
    Site {
        keys: &["gpt", "chatgpt"],
        name: "ChatGPT",
        home: "https://chatgpt.com",
        search: "https://chatgpt.com/?q={q}",
        blurb: "OpenAI's chat assistant",
    },
    Site {
        keys: &["cl", "claude"],
        name: "Claude",
        home: "https://claude.ai",
        search: "https://claude.ai/new?q={q}",
        blurb: "Anthropic's AI assistant",
    },
    // Package registries
    Site {
        keys: &["npm"],
        name: "npm",
        home: "https://www.npmjs.com",
        search: "https://www.npmjs.com/search?q={q}",
        blurb: "JavaScript package registry",
    },
    Site {
        keys: &["cr", "crates"],
        name: "crates.io",
        home: "https://crates.io",
        search: "https://crates.io/search?q={q}",
        blurb: "the Rust package registry",
    },
    Site {
        keys: &["pypi"],
        name: "PyPI",
        home: "https://pypi.org",
        search: "https://pypi.org/search/?q={q}",
        blurb: "the Python package index",
    },
    // Search engines
    Site {
        keys: &["g", "google"],
        name: "Google",
        home: "https://www.google.com",
        search: "https://www.google.com/search?q={q}",
        blurb: "the web search engine",
    },
    Site {
        keys: &["ddg"],
        name: "DuckDuckGo",
        home: "https://duckduckgo.com",
        search: "https://duckduckgo.com/?q={q}",
        blurb: "privacy-first web search",
    },
    // Google apps
    Site {
        keys: &["gm", "gmail"],
        name: "Gmail",
        home: "https://mail.google.com",
        search: "https://mail.google.com/mail/u/0/#search/{q}",
        blurb: "your email",
    },
    Site {
        keys: &["maps", "gmaps"],
        name: "Google Maps",
        home: "https://maps.google.com",
        search: "https://www.google.com/maps/search/{q}",
        blurb: "maps, directions, and places",
    },
    Site {
        keys: &["drive", "gdrive"],
        name: "Google Drive",
        home: "https://drive.google.com",
        search: "https://drive.google.com/drive/search?q={q}",
        blurb: "your files and documents",
    },
    // Social / media / shopping
    Site {
        keys: &["x", "twitter"],
        name: "X (Twitter)",
        home: "https://x.com",
        search: "https://x.com/search?q={q}",
        blurb: "posts and real-time updates",
    },
    Site {
        keys: &["az", "amazon"],
        name: "Amazon",
        home: "https://www.amazon.com",
        search: "https://www.amazon.com/s?k={q}",
        blurb: "online shopping",
    },
    Site {
        keys: &["nf", "netflix"],
        name: "Netflix",
        home: "https://www.netflix.com",
        search: "https://www.netflix.com/search?q={q}",
        blurb: "films and series",
    },
    Site {
        keys: &["sp", "spotify"],
        name: "Spotify",
        home: "https://open.spotify.com",
        search: "https://open.spotify.com/search/{q}",
        blurb: "music and podcasts",
    },
    Site {
        keys: &["imdb"],
        name: "IMDb",
        home: "https://www.imdb.com",
        search: "https://www.imdb.com/find/?q={q}",
        blurb: "films, TV, and cast",
    },
    Site {
        keys: &["tw", "twitch"],
        name: "Twitch",
        home: "https://www.twitch.tv",
        search: "https://www.twitch.tv/search?term={q}",
        blurb: "live streams and gaming",
    },
    // Productivity
    Site {
        keys: &["nt", "notion"],
        name: "Notion",
        home: "https://www.notion.so",
        search: "https://www.notion.so/search?q={q}",
        blurb: "notes, docs, and wikis",
    },
    Site {
        keys: &["fig", "figma"],
        name: "Figma",
        home: "https://www.figma.com",
        search: "https://www.figma.com/community/search?resource_type=mixed&q={q}",
        blurb: "collaborative design",
    },
];

/// If `query`'s first `!<key>` token names a known site, return the URL to
/// redirect to: the site's search filled with the remaining terms, or its home if
/// there are none. `None` ⇒ not a bang, run a normal index search.
pub fn resolve(query: &str) -> Option<String> {
    let mut site: Option<&'static Site> = None;
    let mut terms: Vec<&str> = Vec::new();
    for tok in query.split_whitespace() {
        if site.is_none() {
            if let Some(key) = tok.strip_prefix('!') {
                if let Some(s) = lookup(key) {
                    site = Some(s);
                    continue;
                }
            }
        }
        terms.push(tok);
    }
    let site = site?;
    let terms = terms.join(" ");
    Some(if terms.is_empty() {
        site.home.to_string()
    } else {
        site.search.replace("{q}", &encode(&terms))
    })
}

fn lookup(key: &str) -> Option<&'static Site> {
    let k = key.to_ascii_lowercase();
    SITES.iter().find(|s| s.keys.contains(&k.as_str()))
}

/// Curated launch cards as `(url, title, body)` for indexing. The body folds in
/// the blurb and the bang keys, so a search for "youtube" or "yt" finds it.
pub fn cards() -> Vec<(String, String, String)> {
    SITES
        .iter()
        .map(|s| {
            let keys = s.keys.join(" ");
            (
                s.home.to_string(),
                s.name.to_string(),
                format!(
                    "{name} — {blurb}. Quick-launch shortcut: !{key} ({keys}).",
                    name = s.name,
                    blurb = s.blurb,
                    key = s.keys[0],
                ),
            )
        })
        .collect()
}

/// Percent-encode a query string component (RFC 3986 unreserved kept literal).
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_bang_with_terms() {
        let u = resolve("!gh rust async").unwrap();
        assert_eq!(
            u,
            "https://github.com/search?q=rust%20async&type=repositories"
        );
    }

    #[test]
    fn bare_bang_goes_home() {
        assert_eq!(resolve("!youtube").unwrap(), "https://www.youtube.com");
        assert_eq!(resolve("!yt").unwrap(), "https://www.youtube.com");
    }

    #[test]
    fn non_bang_is_normal_search() {
        assert!(resolve("github actions tutorial").is_none());
        assert!(resolve("rust ownership").is_none());
        // An unknown bang isn't hijacked — falls through to a normal search.
        assert!(resolve("!nope something").is_none());
    }

    #[test]
    fn encodes_special_characters() {
        let u = resolve("!so how to do x?").unwrap();
        assert!(u.contains("how%20to%20do%20x%3F"), "got {u}");
    }

    #[test]
    fn resolves_a_general_site_bang() {
        assert_eq!(
            resolve("!li staff engineer").unwrap(),
            "https://www.linkedin.com/search/results/all/?keywords=staff%20engineer"
        );
        assert_eq!(resolve("!kaggle").unwrap(), "https://www.kaggle.com");
    }

    #[test]
    fn all_bang_keys_are_unique() {
        // A duplicate key would make `lookup` resolve to whichever site comes first
        // — a silent footgun as the table grows.
        let mut seen = std::collections::HashSet::new();
        for s in SITES {
            for k in s.keys {
                assert!(
                    k.chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
                    "key {k:?} must be lowercase/digits"
                );
                assert!(seen.insert(*k), "duplicate bang key: {k:?}");
            }
        }
    }

    #[test]
    fn every_card_has_searchable_body() {
        let cards = cards();
        assert_eq!(cards.len(), SITES.len());
        for (url, title, body) in &cards {
            assert!(url.starts_with("https://"));
            assert!(!title.is_empty());
            assert!(body.contains('!'), "card body mentions its bang");
        }
    }
}
