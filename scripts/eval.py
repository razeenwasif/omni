#!/usr/bin/env python3
"""Known-item retrieval eval for Omni's hybrid ranking.

Each query has one expected target doc (by URL substring) that exists in the
corpus. For each retrieval mode we measure MRR@10 and recall@10 over the set,
sweeping the semantic-fusion weight to find the best lexical/semantic balance.

Usage: python3 scripts/eval.py [host]   (default localhost:8085)
The server must be running with embeddings (hybrid) for the sweep to be meaningful.
"""
import sys, json, urllib.parse, urllib.request

HOST = sys.argv[1] if len(sys.argv) > 1 else "localhost:8085"

# (natural-language query, expected URL substring) — all targets verified present.
QUERIES = [
    ("javascript promises and async await",                 "Global_Objects/Promise"),
    ("javascript key value dictionary collection",          "Global_Objects/Map"),
    ("javascript regular expression pattern matching",       "Global_Objects/RegExp"),
    ("javascript set of unique values",                      "Global_Objects/Set"),
    ("javascript unique symbol primitive type",             "Global_Objects/Symbol"),
    ("rust growable array vector type",                      "std/vec/struct.Vec.html"),
    ("rust hash map dictionary",                            "std/collections/struct.HashMap.html"),
    ("rust atomic reference counted shared pointer",         "std/sync/struct.Arc.html"),
    ("go language idiomatic best practices style guide",     "doc/effective_go"),
    ("abductive reasoning inference to the best explanation","entries/abduction"),
    ("moral status and the ethics of abortion",             "entries/abortion"),
    ("what is aesthetic experience in art",                 "entries/aesthetic-experience"),
]

# Retrieval modes: (label, query-string fragment).
MODES = [
    ("lexical-only",  "lex=1"),
    ("hybrid sw=0.5", "sw=0.5"),
    ("hybrid sw=1.0", "sw=1.0"),
    ("hybrid sw=1.5", "sw=1.5"),
    ("hybrid sw=2.0", "sw=2.0"),
    ("hybrid sw=3.0", "sw=3.0"),
    ("semantic-heavy sw=6.0", "sw=6.0"),
]

def fetch(query, frag):
    q = urllib.parse.quote_plus(query)
    url = f"http://{HOST}/search?q={q}&fmt=json&{frag}"
    with urllib.request.urlopen(url, timeout=30) as r:
        return json.load(r)

def rank_of(results, expected, k=10):
    for i, hit in enumerate(results[:k], 1):
        if expected in hit.get("url", ""):
            return i
    return 0  # not found in top-k

def main():
    print(f"Omni hybrid-ranking eval — {len(QUERIES)} known-item queries @ {HOST}\n")
    print(f"{'mode':<22} {'MRR@10':>8} {'recall@10':>10}")
    print("-" * 42)
    best = None
    for label, frag in MODES:
        rr_sum, hits = 0.0, 0
        for query, expected in QUERIES:
            try:
                results = fetch(query, frag)
            except Exception as e:
                print(f"  fetch error ({label}, {query!r}): {e}")
                results = []
            r = rank_of(results, expected)
            if r:
                rr_sum += 1.0 / r
                hits += 1
        mrr = rr_sum / len(QUERIES)
        recall = hits / len(QUERIES)
        print(f"{label:<22} {mrr:>8.3f} {recall:>10.2f}")
        if best is None or mrr > best[1]:
            best = (label, mrr)
    print("-" * 42)
    print(f"best: {best[0]} (MRR@10 {best[1]:.3f})")

if __name__ == "__main__":
    main()
