//! Bounded path enumeration over the asset graph.
//!
//! A depth-first walk from a start asset emits a [`Path`] at *every* asset it
//! reaches, so both cycles (returning to the start) and cross-asset endpoints
//! are captured. The only traversal guard is no-pool-reuse: a given pool may
//! appear at most once per path. Revisiting an *asset* through a different pool
//! is allowed — that is how multi-pool cycles are found.

use crate::core::application::config;
use crate::core::application::graph::{Edge, Graph};
use crate::primitives::asset::{AssetId, Pair};
use crate::primitives::opportunity::{Hop, Path};
use crate::primitives::pool::PoolId;

/// Enumerate paths from `start` up to `max_hops` deep, under the per-node beam bound.
pub fn find_paths(graph: &Graph, start: &AssetId, max_hops: usize) -> Vec<Path> {
    let mut out = Vec::new();
    let mut used: Vec<PoolId> = Vec::new();
    let mut hops: Vec<Hop> = Vec::new();
    expand(
        graph, start, start, max_hops, &mut used, &mut hops, &mut out,
    );
    tracing::debug!(
        start = start.as_str(),
        paths = out.len(),
        "enumerated paths"
    );
    out
}

/// Recursively expand from `current`, emitting a path at each hop taken.
fn expand(
    graph: &Graph,
    start: &AssetId,
    current: &AssetId,
    max_hops: usize,
    used: &mut Vec<PoolId>,
    hops: &mut Vec<Hop>,
    out: &mut Vec<Path>,
) {
    if hops.len() >= max_hops {
        return;
    }

    let frontier: Vec<Edge> = graph
        .edges_from(current)
        .filter(|edge| !used.contains(&edge.pool))
        .take(config::BEAM_WIDTH)
        .collect();

    for edge in frontier {
        let next = edge.to.clone();
        used.push(edge.pool.clone());
        hops.push(Hop {
            pool: edge.pool,
            pair: Pair {
                source: edge.from,
                destination: edge.to,
            },
        });

        out.push(Path {
            start: start.clone(),
            hops: hops.clone(),
        });
        expand(graph, start, &next, max_hops, used, hops, out);

        hops.pop();
        used.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::application::graph::Graph;
    use crate::test_utils::{FakePool, fake_snapshot};
    use rust_decimal::Decimal;

    fn key(path: &Path) -> String {
        let mut s = path.start.as_str().to_string();
        for hop in &path.hops {
            s.push_str("->");
            s.push_str(hop.pair.destination.as_str());
            s.push('(');
            s.push_str(hop.pool.as_str());
            s.push(')');
        }
        s
    }

    #[test]
    fn enumerates_cycle_and_cross_asset_endpoints() {
        // Triangle A-B, B-C, A-C, each a distinct pool.
        let snap = fake_snapshot(vec![
            FakePool::new("pab", &["ethereum:a", "ethereum:b"], Decimal::ONE),
            FakePool::new("pbc", &["ethereum:b", "ethereum:c"], Decimal::ONE),
            FakePool::new("pac", &["ethereum:a", "ethereum:c"], Decimal::ONE),
        ]);
        let graph = Graph::new(&snap);
        let paths = find_paths(&graph, &AssetId::new("ethereum:a").unwrap(), 3);
        let keys: Vec<String> = paths.iter().map(key).collect();

        // The one-hop cross-asset endpoint A->C.
        assert!(keys.iter().any(|k| k == "ethereum:a->ethereum:c(pac)"));
        // The full triangle cycle back to A.
        assert!(
            keys.iter()
                .any(|k| k == "ethereum:a->ethereum:b(pab)->ethereum:c(pbc)->ethereum:a(pac)")
        );
    }

    #[test]
    fn no_pool_is_reused_within_a_path() {
        // Two distinct pools p1, p2 both connecting A and B.
        let snap = fake_snapshot(vec![
            FakePool::new("p1", &["ethereum:a", "ethereum:b"], Decimal::ONE),
            FakePool::new("p2", &["ethereum:a", "ethereum:b"], Decimal::ONE),
        ]);
        let graph = Graph::new(&snap);
        let paths = find_paths(&graph, &AssetId::new("ethereum:a").unwrap(), 2);
        let keys: Vec<String> = paths.iter().map(key).collect();

        // A round trip through two different pools is allowed.
        assert!(
            keys.iter()
                .any(|k| k == "ethereum:a->ethereum:b(p1)->ethereum:a(p2)")
        );
        // The same pool twice never appears.
        assert!(
            !keys
                .iter()
                .any(|k| k.contains("(p1)") && k.matches("(p1)").count() > 1)
        );
        assert!(
            !keys
                .iter()
                .any(|k| k.contains("(p2)") && k.matches("(p2)").count() > 1)
        );
    }

    #[test]
    fn max_hops_one_yields_only_single_hop_paths() {
        let snap = fake_snapshot(vec![
            FakePool::new("pab", &["ethereum:a", "ethereum:b"], Decimal::ONE),
            FakePool::new("pbc", &["ethereum:b", "ethereum:c"], Decimal::ONE),
        ]);
        let graph = Graph::new(&snap);
        let paths = find_paths(&graph, &AssetId::new("ethereum:a").unwrap(), 1);
        assert!(paths.iter().all(|p| p.hops.len() == 1));
    }

    /// Experiment harness: report how the enumerated-path count grows with depth
    /// on a fully-connected asset graph with a few pools per pair. Ignored by
    /// default; run with `cargo test -- --ignored --nocapture path_count_growth`.
    #[test]
    #[ignore = "experiment: measures path-count growth, run with --ignored --nocapture"]
    fn path_count_growth() {
        let assets = [
            "ethereum:usdc",
            "ethereum:weth",
            "ethereum:dai",
            "ethereum:usdt",
        ];
        let pools_per_pair = 2;

        let mut pools = Vec::new();
        let mut id = 0;
        for i in 0..assets.len() {
            for j in (i + 1)..assets.len() {
                for _ in 0..pools_per_pair {
                    pools.push(FakePool::new(
                        &format!("p{id}"),
                        &[assets[i], assets[j]],
                        Decimal::ONE,
                    ));
                    id += 1;
                }
            }
        }
        let snap = fake_snapshot(pools);
        let graph = Graph::new(&snap);
        let start = AssetId::new(assets[0]).unwrap();

        eprintln!(
            "graph: {} assets, {pools_per_pair} pools/pair, {id} pools total",
            assets.len()
        );
        for max_hops in 1..=8 {
            let n = find_paths(&graph, &start, max_hops).len();
            eprintln!("max_hops={max_hops:>2}  paths={n}");
        }
    }
}
