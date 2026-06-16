#!/usr/bin/env python3
"""Cross-index A/B for Omni retrieval — comparable across different servers.

`eval.py` normalizes nDCG against a *per-index self-pool* (its own deep fetch),
so its nDCG is NOT comparable between two different indexes: a stronger index
builds a deeper pool and thereby a larger ideal DCG, which can make an identical
lexical run score *lower*. To compare indexes (e.g. passage-level vs whole-doc)
this tool reports metrics that are valid across servers:

  * DCG@10      — raw, un-normalized; grading is pattern-based and index-free.
  * nDCG@10*    — normalized against a SHARED pool (the union of every host's
                  deep fetch), so the ideal is identical for all hosts.
  * success@10  — fraction of queries with a grade>=2 doc in the top 10.

Usage: python3 scripts/compare.py host1=label1 host2=label2 ...
       (a bare host gets itself as the label)
"""
import sys, math
from eval import QUERIES, grade, dcg, fetch as _fetch

MODE = "sw=2.0"   # the shipped default; the apples-to-apples ranking mode
POOL_DEPTH = 50


def fetch(host, query, frag, k):
    # eval.fetch reads the module-global HOST; set it per call instead.
    import eval as e
    e.HOST = host
    return _fetch(query, frag, k)


def main():
    if len(sys.argv) < 2:
        print("usage: compare.py host1=label1 host2=label2 ...")
        sys.exit(1)
    hosts = []
    for a in sys.argv[1:]:
        host, _, label = a.partition("=")
        hosts.append((host, label or host))

    # Shared pool per query: union of every host's deep fetch, so the ideal DCG
    # is identical for all hosts being compared.
    shared_idcg = []
    for q, rel in QUERIES:
        pooled = set()
        for host, _ in hosts:
            pooled.update(fetch(host, q, MODE, POOL_DEPTH))
        grades = sorted((grade(u, rel) for u in pooled), reverse=True)[:10]
        shared_idcg.append(dcg(grades))

    print(f"comparison @ mode '{MODE}', shared pool over {len(hosts)} host(s), "
          f"{len(QUERIES)} queries\n")
    print(f"{'index':<22} {'DCG@10':>8} {'nDCG@10*':>9} {'success@10':>11}")
    print("-" * 53)
    for host, label in hosts:
        sum_dcg = sum_ndcg = 0.0
        succ = 0
        for i, (q, rel) in enumerate(QUERIES):
            urls = fetch(host, q, MODE, 10)
            d = dcg([grade(u, rel) for u in urls[:10]])
            sum_dcg += d
            if shared_idcg[i] > 0:
                sum_ndcg += d / shared_idcg[i]
            if any(grade(u, rel) >= 2 for u in urls[:10]):
                succ += 1
        n = len(QUERIES)
        print(f"{label:<22} {sum_dcg/n:>8.3f} {sum_ndcg/n:>9.3f} {succ/n:>11.2f}")
    print("-" * 53)
    print("* nDCG@10 normalized against the SHARED pool (comparable across hosts)")


if __name__ == "__main__":
    main()
