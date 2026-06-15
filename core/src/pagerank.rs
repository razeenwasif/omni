//! PageRank — a query-independent measure of document authority.
//!
//! Intuition: a page is important if important pages link to it. PageRank models
//! a "random surfer" who, with probability `damping`, follows a random outlink,
//! and otherwise (`1 - damping`) teleports to a random page. The stationary
//! distribution of where the surfer spends time *is* the PageRank vector.
//!
//! We iterate the update to convergence:
//!   rank'(i) = (1-d)/N  +  d * ( Σ_{j→i} rank(j)/outdeg(j)  +  dangling/N )
//! where "dangling" is the mass on pages with no outlinks (spread uniformly so
//! rank isn't lost). The graph is built from the crawler's stored `links:`.

const DAMPING: f64 = 0.85;
const ITERATIONS: usize = 40;

/// Compute PageRank over `num_docs` documents given each doc's out-edges
/// (target doc ids). Returns a rank per doc id; the vector sums to ~1.
pub fn compute(num_docs: usize, out_edges: &[Vec<usize>]) -> Vec<f64> {
    if num_docs == 0 {
        return Vec::new();
    }
    let n = num_docs as f64;
    let mut rank = vec![1.0 / n; num_docs];

    for _ in 0..ITERATIONS {
        // Base teleport mass for every node.
        let mut next = vec![(1.0 - DAMPING) / n; num_docs];

        // Mass stuck on dangling nodes (no outlinks) is redistributed uniformly.
        let dangling: f64 = (0..num_docs)
            .filter(|&j| out_edges[j].is_empty())
            .map(|j| rank[j])
            .sum();
        let dangling_share = DAMPING * dangling / n;
        for slot in next.iter_mut() {
            *slot += dangling_share;
        }

        // Push each node's rank along its outlinks.
        for j in 0..num_docs {
            let deg = out_edges[j].len();
            if deg == 0 {
                continue;
            }
            let share = DAMPING * rank[j] / deg as f64;
            for &target in &out_edges[j] {
                next[target] += share;
            }
        }

        rank = next;
    }
    rank
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_node_outranks_others() {
        // 0→2, 1→2, 2→0 : node 2 is linked by both others → highest rank.
        let edges = vec![vec![2], vec![2], vec![0]];
        let r = compute(3, &edges);
        assert!(r[2] > r[0]);
        assert!(r[2] > r[1]);
        // Probability vector sums to ~1.
        let sum: f64 = r.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6);
    }

    #[test]
    fn empty_graph_is_empty() {
        assert!(compute(0, &[]).is_empty());
    }
}
