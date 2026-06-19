//! Local, in-memory product telemetry for search quality feedback.
//!
//! This deliberately records only aggregate search/click signals inside the
//! running Omni process. It is enough to spot zero-result queries, popular
//! searches, clicked URLs, and latency without introducing persistent tracking.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

const MAX_TRACKED_QUERIES: usize = 512;
const MAX_TRACKED_URLS: usize = 512;
const MAX_LATENCIES: usize = 512;

pub struct Telemetry {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    clock: u64,
    searches: u64,
    clicks: u64,
    zero_results: u64,
    total_search_ms: u64,
    latencies: VecDeque<u64>,
    queries: HashMap<String, QueryStat>,
    urls: HashMap<String, UrlStat>,
}

#[derive(Clone)]
struct QueryStat {
    count: u64,
    zero_results: u64,
    total_results: u64,
    total_ms: u64,
    last_seen: u64,
}

#[derive(Clone)]
struct UrlStat {
    clicks: u64,
    last_query: String,
    last_seen: u64,
}

pub struct Snapshot {
    pub searches: u64,
    pub clicks: u64,
    pub zero_results: u64,
    pub avg_search_ms: f64,
    pub p95_search_ms: u64,
    pub top_queries: Vec<QueryMetric>,
    pub top_clicks: Vec<ClickMetric>,
}

pub struct QueryMetric {
    pub query: String,
    pub count: u64,
    pub zero_results: u64,
    pub avg_results: f64,
    pub avg_ms: f64,
}

pub struct ClickMetric {
    pub url: String,
    pub clicks: u64,
    pub last_query: String,
}

impl Telemetry {
    pub fn new() -> Self {
        Telemetry {
            state: Mutex::new(State::default()),
        }
    }

    pub fn record_search(&self, query: &str, results: usize, elapsed: Duration) {
        let Some(query) = normalize(query) else {
            return;
        };
        let ms = elapsed.as_millis().min(u128::from(u64::MAX)) as u64;
        let mut s = self.state.lock().unwrap();
        s.clock += 1;
        s.searches += 1;
        s.total_search_ms = s.total_search_ms.saturating_add(ms);
        if results == 0 {
            s.zero_results += 1;
        }
        s.latencies.push_back(ms);
        if s.latencies.len() > MAX_LATENCIES {
            s.latencies.pop_front();
        }

        let now = s.clock;
        let q = s.queries.entry(query).or_insert(QueryStat {
            count: 0,
            zero_results: 0,
            total_results: 0,
            total_ms: 0,
            last_seen: 0,
        });
        q.count += 1;
        q.total_results = q.total_results.saturating_add(results as u64);
        q.total_ms = q.total_ms.saturating_add(ms);
        q.last_seen = now;
        if results == 0 {
            q.zero_results += 1;
        }
        prune_queries(&mut s);
    }

    pub fn record_click(&self, query: &str, url: &str) {
        let Some(url) = normalize_url(url) else {
            return;
        };
        let query = normalize(query).unwrap_or_default();
        let mut s = self.state.lock().unwrap();
        s.clock += 1;
        s.clicks += 1;
        let now = s.clock;
        let u = s.urls.entry(url).or_insert(UrlStat {
            clicks: 0,
            last_query: String::new(),
            last_seen: 0,
        });
        u.clicks += 1;
        u.last_query = query;
        u.last_seen = now;
        prune_urls(&mut s);
    }

    pub fn snapshot(&self) -> Snapshot {
        let s = self.state.lock().unwrap();
        let mut latencies: Vec<u64> = s.latencies.iter().copied().collect();
        latencies.sort_unstable();
        let p95 = if latencies.is_empty() {
            0
        } else {
            let idx = ((latencies.len() - 1) * 95) / 100;
            latencies[idx]
        };

        let mut queries: Vec<(&String, &QueryStat)> = s.queries.iter().collect();
        queries.sort_by(|a, b| {
            b.1.count
                .cmp(&a.1.count)
                .then_with(|| b.1.last_seen.cmp(&a.1.last_seen))
                .then_with(|| a.0.cmp(b.0))
        });
        let top_queries = queries
            .into_iter()
            .take(8)
            .map(|(q, stat)| QueryMetric {
                query: q.clone(),
                count: stat.count,
                zero_results: stat.zero_results,
                avg_results: stat.total_results as f64 / stat.count.max(1) as f64,
                avg_ms: stat.total_ms as f64 / stat.count.max(1) as f64,
            })
            .collect();

        let mut urls: Vec<(&String, &UrlStat)> = s.urls.iter().collect();
        urls.sort_by(|a, b| {
            b.1.clicks
                .cmp(&a.1.clicks)
                .then_with(|| b.1.last_seen.cmp(&a.1.last_seen))
                .then_with(|| a.0.cmp(b.0))
        });
        let top_clicks = urls
            .into_iter()
            .take(8)
            .map(|(url, stat)| ClickMetric {
                url: url.clone(),
                clicks: stat.clicks,
                last_query: stat.last_query.clone(),
            })
            .collect();

        Snapshot {
            searches: s.searches,
            clicks: s.clicks,
            zero_results: s.zero_results,
            avg_search_ms: s.total_search_ms as f64 / s.searches.max(1) as f64,
            p95_search_ms: p95,
            top_queries,
            top_clicks,
        }
    }
}

fn normalize(q: &str) -> Option<String> {
    let q = q.split_whitespace().collect::<Vec<_>>().join(" ");
    if q.is_empty() {
        None
    } else {
        Some(q.chars().take(160).collect::<String>().to_lowercase())
    }
}

fn normalize_url(url: &str) -> Option<String> {
    let url = url.trim();
    if (url.starts_with("http://") || url.starts_with("https://"))
        && !url.contains('\r')
        && !url.contains('\n')
    {
        Some(url.chars().take(2048).collect())
    } else {
        None
    }
}

fn prune_queries(s: &mut State) {
    if s.queries.len() <= MAX_TRACKED_QUERIES {
        return;
    }
    if let Some(remove) = s
        .queries
        .iter()
        .min_by_key(|(_, q)| (q.count, q.last_seen))
        .map(|(q, _)| q.clone())
    {
        s.queries.remove(&remove);
    }
}

fn prune_urls(s: &mut State) {
    if s.urls.len() <= MAX_TRACKED_URLS {
        return;
    }
    if let Some(remove) = s
        .urls
        .iter()
        .min_by_key(|(_, u)| (u.clicks, u.last_seen))
        .map(|(u, _)| u.clone())
    {
        s.urls.remove(&remove);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_searches_and_zero_results() {
        let t = Telemetry::new();
        t.record_search("Rust Ownership", 10, Duration::from_millis(12));
        t.record_search("rust   ownership", 0, Duration::from_millis(30));
        let s = t.snapshot();
        assert_eq!(s.searches, 2);
        assert_eq!(s.zero_results, 1);
        assert_eq!(s.top_queries[0].query, "rust ownership");
        assert_eq!(s.top_queries[0].count, 2);
        assert_eq!(s.top_queries[0].zero_results, 1);
    }

    #[test]
    fn records_clicks_by_url() {
        let t = Telemetry::new();
        t.record_click("rust ownership", "https://example.com/rust");
        t.record_click("borrow checker", "https://example.com/rust");
        let s = t.snapshot();
        assert_eq!(s.clicks, 2);
        assert_eq!(s.top_clicks[0].url, "https://example.com/rust");
        assert_eq!(s.top_clicks[0].clicks, 2);
        assert_eq!(s.top_clicks[0].last_query, "borrow checker");
    }

    #[test]
    fn rejects_unsafe_click_urls() {
        let t = Telemetry::new();
        t.record_click("x", "javascript:alert(1)");
        t.record_click("x", "https://example.com/\r\nLocation: https://evil.test");
        let s = t.snapshot();
        assert_eq!(s.clicks, 0);
        assert!(s.top_clicks.is_empty());
    }
}
