#!/usr/bin/env python3
"""Graded-relevance retrieval eval for Omni's hybrid ranking.

The earlier known-item eval scored a single exact target per query, so it
*under*-counted as the corpus grew (a relevant-but-not-exact page outranking the
exact one read as a miss). This version uses **graded relevance**: each query has
several relevant URL patterns with grades (3 = canonical, 2 = closely related,
1 = tangential), and the headline metric is **nDCG@10** — a relevant family member
ranking high now credits the score instead of penalizing it.

The ideal DCG is computed from the *pooled* relevant docs actually found (a deep
k=50 fetch), so patterns whose docs aren't in the corpus don't deflate the score.

Usage: python3 scripts/eval.py [host]   (default localhost:8085)
"""
import sys, json, math, urllib.parse, urllib.request

HOST = sys.argv[1] if len(sys.argv) > 1 else "localhost:8085"

# A pattern ending in '$' matches the URL's end (the canonical page); otherwise
# it's a substring (a family). A doc's grade = the max grade over matching patterns.
QUERIES = [
    ("javascript promises and async await", {
        "Global_Objects/Promise$": 3, "Guide/Using_promises": 3,
        "Operators/await": 2, "Statements/async_function": 2,
        "Operators/async_function": 2, "Global_Objects/Promise/": 1,
    }),
    ("javascript key value dictionary collection", {
        "Global_Objects/Map$": 3, "Global_Objects/WeakMap": 2,
        "Global_Objects/Object$": 1, "Global_Objects/Map/": 1,
    }),
    ("javascript regular expression pattern matching", {
        "Global_Objects/RegExp$": 3, "Guide/Regular_expressions": 3,
        "Global_Objects/RegExp/": 2,
    }),
    ("javascript set of unique values", {
        "Global_Objects/Set$": 3, "Global_Objects/WeakSet": 2, "Global_Objects/Set/": 1,
    }),
    ("javascript fetch api http requests", {
        "Web/API/Fetch_API": 3, "Web/API/Window/fetch": 2,
        "Web/API/Request": 2, "Web/API/Response": 2,
    }),
    ("rust growable array vector type", {
        "std/vec/struct.Vec.html": 3, "std/vec/index.html": 2,
        "std/collections/index.html": 1,
    }),
    ("rust hash map dictionary", {
        "std/collections/struct.HashMap.html": 3, "std/collections/index.html": 2,
        "std/collections/struct.BTreeMap.html": 1,
    }),
    ("rust atomic reference counted shared pointer", {
        "std/sync/struct.Arc.html": 3, "std/sync/index.html": 2,
        "std/sync/struct.Mutex.html": 2, "std/rc/struct.Rc.html": 1,
    }),
    ("rust ownership borrowing and lifetimes", {
        "rust-by-example/scope": 3, "reference/lifetime": 2,
        "std/borrow": 1, "reference/references": 1,
    }),
    ("go language idiomatic best practices style guide", {
        "doc/effective_go": 3, "ref/spec": 2,
    }),
    ("python json encoding and decoding", {
        "library/json.html": 3, "library/pickle.html": 1,
    }),
    ("python asyncio event loop concurrency", {
        "library/asyncio": 3, "library/concurrent": 1, "library/threading": 1,
    }),
    ("abductive reasoning inference to the best explanation", {
        "entries/abduction": 3, "entries/scientific-discovery": 1, "entries/induction": 1,
    }),
    ("moral status and the ethics of abortion", {
        "entries/abortion": 3, "entries/ethics-": 1,
    }),
    ("what is aesthetic experience in art", {
        "entries/aesthetic-experience": 3, "entries/aesthetic-concept": 2,
        "entries/aesthetic-judgment": 2, "entries/beauty": 1,
    }),
    ("theory of knowledge and justified belief", {
        "entries/justep": 3, "entries/knowledge-analysis": 3, "entries/reliabilism": 2,
        "entries/epistemology": 2, "entries/foundationalist": 2,
    }),
    ("supervised machine learning from labeled data", {
        "wiki/Machine_learning": 3, "wiki/Artificial_intelligence": 2,
        "wiki/Statistic": 1,
    }),
    # --- broadened: C++ / Go stdlib / more Python+Rust / Wikipedia / SEP ---
    ("c++ dynamic array vector container", {
        "cpp/container/vector": 3, "cpp/container/array": 2, "cpp/container$": 1,
    }),
    ("c++ unique pointer smart pointer memory", {
        "cpp/memory/unique_ptr": 3, "cpp/memory/shared_ptr": 2, "cpp/memory": 1,
    }),
    ("c++ string class", {
        "cpp/string/basic_string": 3, "cpp/string": 1,
    }),
    ("go http web server package", {
        "pkg/net/http": 3, "pkg/net/url": 1,
    }),
    ("go json encoding and decoding package", {
        "pkg/encoding/json": 3, "pkg/encoding": 1,
    }),
    ("python regular expressions module", {
        "library/re.html": 3, "howto/regex": 2, "library/re": 2,
    }),
    ("python pep 8 style guide conventions", {
        "pep-0008": 3, "pep-0020": 1, "pep-0007": 1,
    }),
    ("rust error handling result and option types", {
        "std/result": 3, "std/option": 2, "book/ch09": 2,
    }),
    ("rust traits and generic programming", {
        "reference/items/traits": 3, "rust-by-example/trait": 3, "book/ch10": 2,
    }),
    ("theory of relativity einstein spacetime", {
        "wiki/Theory_of_relativity": 3, "wiki/General_relativity": 3,
        "wiki/Special_relativity": 2, "wiki/Spacetime": 1, "wiki/Albert_Einstein": 1,
    }),
    ("dna structure and genetics", {
        "wiki/DNA": 3, "wiki/Genetics": 3, "wiki/Gene": 2, "wiki/Chromosome": 1,
    }),
    ("calculus derivatives and integrals", {
        "wiki/Calculus": 3, "wiki/Derivative": 2, "wiki/Integral": 2,
        "wiki/Limit_(mathematics)": 1,
    }),
    ("free will and determinism", {
        "entries/freewill": 3, "entries/free-will": 3, "entries/determinism-causal": 2,
        "entries/compatibilism": 2, "entries/incompatibilism": 2,
    }),
    ("philosophy of mind and consciousness", {
        "entries/consciousness": 3, "entries/qualia": 2, "entries/mind": 1,
    }),
    ("utilitarianism and consequentialist ethics", {
        "entries/consequentialism": 3, "entries/utilitarianism-history": 3,
        "entries/hedonism": 1, "entries/ethics-deontological": 1,
    }),
]

MODES = [
    ("lexical-only", "lex=1"),
    ("hybrid sw=1.0", "sw=1.0"),
    ("hybrid sw=2.0", "sw=2.0"),
    ("hybrid sw=4.0", "sw=4.0"),
    # Opt-in LLM reranker — off by default; measured ~neutral vs the tuned hybrid
    # on this corpus (small models hurt). See core/src/rerank.rs.
    ("hybrid + rerank(e4b)", "sw=2.0&rerank=1&rr_model=gemma4:e4b-it-qat"),
]

POOL_MODE = "sw=4.0"  # mode used to build the judged pool for the ideal DCG

def fetch(query, frag, k):
    q = urllib.parse.quote_plus(query)
    url = f"http://{HOST}/search?q={q}&fmt=json&{frag}&k={k}"
    with urllib.request.urlopen(url, timeout=30) as r:
        return [h.get("url", "") for h in json.load(r)]

def grade(url, rel):
    g = 0
    for pat, gr in rel.items():
        hit = url.endswith(pat[:-1]) if pat.endswith("$") else (pat in url)
        if hit:
            g = max(g, gr)
    return g

def dcg(grades):
    return sum((2 ** g - 1) / math.log2(rank + 1) for rank, g in enumerate(grades, 1))

def ndcg_at(urls, rel, k=10):
    # Ideal from the pooled relevant docs (deep fetch), so unreachable patterns
    # don't deflate the score.
    pool = [grade(u, rel) for u in fetch_pool.get(id(rel), [])]
    idcg = dcg(sorted(pool, reverse=True)[:k])
    got = dcg([grade(u, rel) for u in urls[:k]])
    return (got / idcg) if idcg > 0 else 0.0

fetch_pool = {}

def main():
    print(f"Omni graded-relevance eval — {len(QUERIES)} queries @ {HOST}\n")
    # Build the judged pool once per query (deep fetch at the pool mode).
    for q, rel in QUERIES:
        fetch_pool[id(rel)] = fetch(q, POOL_MODE, 50)

    print(f"{'mode':<16} {'nDCG@10':>8} {'success@10':>11}")
    print("-" * 38)
    best = None
    for label, frag in MODES:
        nd, succ = 0.0, 0
        for q, rel in QUERIES:
            urls = fetch(q, frag, 10)
            nd += ndcg_at(urls, rel)
            if any(grade(u, rel) >= 2 for u in urls[:10]):
                succ += 1
        nd /= len(QUERIES)
        succ /= len(QUERIES)
        print(f"{label:<16} {nd:>8.3f} {succ:>11.2f}")
        if best is None or nd > best[1]:
            best = (label, nd)
    print("-" * 38)
    print(f"best: {best[0]} (nDCG@10 {best[1]:.3f})")
    print("\nsuccess@10 = fraction of queries with a grade>=2 doc in the top 10")

if __name__ == "__main__":
    main()
