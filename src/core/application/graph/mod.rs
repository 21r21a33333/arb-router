//! The asset graph induced by a snapshot: assets are nodes, and each pool
//! contributes a directed edge between every ordered pair of the assets it
//! holds (an N-asset pool yields N-1 edges from any given asset).
//!
//! Bounded path enumeration over this graph lives in [`finder`].

pub mod finder;

use crate::core::deps::pool_store::PoolSnapshot;
use crate::primitives::asset::AssetId;
use crate::primitives::pool::PoolId;

/// One directed pool edge `from -> to`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    pub pool: PoolId,
    pub from: AssetId,
    pub to: AssetId,
}

/// A read-only view over a snapshot's connectivity.
pub struct Graph<'a>(&'a PoolSnapshot);

impl<'a> Graph<'a> {
    pub fn new(snapshot: &'a PoolSnapshot) -> Self {
        Self(snapshot)
    }

    /// Every edge leaving `asset`: one per (pool, other-asset) pairing.
    pub fn edges_from(&self, asset: &AssetId) -> impl Iterator<Item = Edge> + 'a {
        let from = asset.clone();
        self.0.pools_from(asset).iter().flat_map(move |entry| {
            let pool = entry.pool.id();
            entry
                .pool
                .assets()
                .iter()
                .filter(|other| **other != from)
                .map(|other| Edge {
                    pool: pool.clone(),
                    from: from.clone(),
                    to: other.clone(),
                })
                .collect::<Vec<_>>()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{FakePool, fake_snapshot};
    use rust_decimal::Decimal;

    #[test]
    fn two_asset_pool_yields_one_edge_each_direction() {
        let snap = fake_snapshot(vec![FakePool::new(
            "p1",
            &["ethereum:usdc", "ethereum:weth"],
            Decimal::ONE,
        )]);
        let graph = Graph::new(&snap);

        let from_usdc: Vec<Edge> = graph
            .edges_from(&AssetId::new("ethereum:usdc").unwrap())
            .collect();
        assert_eq!(from_usdc.len(), 1);
        assert_eq!(from_usdc[0].to, AssetId::new("ethereum:weth").unwrap());
        assert_eq!(from_usdc[0].pool, PoolId::new("p1"));

        let from_weth: Vec<Edge> = graph
            .edges_from(&AssetId::new("ethereum:weth").unwrap())
            .collect();
        assert_eq!(from_weth.len(), 1);
        assert_eq!(from_weth[0].to, AssetId::new("ethereum:usdc").unwrap());
    }

    #[test]
    fn n_asset_pool_yields_n_minus_one_edges() {
        let snap = fake_snapshot(vec![FakePool::new(
            "3pool",
            &["ethereum:dai", "ethereum:usdc", "ethereum:usdt"],
            Decimal::ONE,
        )]);
        let graph = Graph::new(&snap);
        let edges: Vec<Edge> = graph
            .edges_from(&AssetId::new("ethereum:dai").unwrap())
            .collect();
        assert_eq!(edges.len(), 2);
    }
}
