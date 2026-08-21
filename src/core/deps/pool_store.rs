//! Port for reading a block-consistent snapshot of pool state, plus the
//! immutable [`PoolSnapshot`] value the port hands out.

use std::collections::HashMap;
use std::sync::Arc;

use time::OffsetDateTime;

use crate::core::deps::pool::Pool;
use crate::primitives::asset::{AssetId, ChainId};
use crate::primitives::pool::PoolId;

/// Provenance of a pool's cached state: which block it was read at and when.
#[derive(Clone, Copy, Debug)]
pub struct PoolMeta {
    pub synced_block: u64,
    pub synced_at: OffsetDateTime,
}

/// A quotable pool paired with the freshness of its state.
#[derive(Clone)]
pub struct PoolEntry {
    pub pool: Arc<dyn Pool>,
    pub meta: PoolMeta,
}

/// An immutable, block-consistent view of all pools on one chain.
///
/// Two lookup indexes are precomputed so the hot path (graph traversal and hop
/// resolution) never scans: `by_asset` groups entries by each asset they touch,
/// and `by_id` resolves a `PoolId` directly. Entries are cheap to duplicate
/// across indexes because [`PoolEntry`] holds an `Arc<dyn Pool>`.
pub struct PoolSnapshot {
    block: u64,
    taken_at: OffsetDateTime,
    by_asset: HashMap<AssetId, Vec<PoolEntry>>,
    by_id: HashMap<PoolId, PoolEntry>,
}

impl PoolSnapshot {
    /// Build a snapshot and its indexes from the pools live at `block`.
    pub fn from_entries(block: u64, taken_at: OffsetDateTime, entries: Vec<PoolEntry>) -> Self {
        let mut by_asset: HashMap<AssetId, Vec<PoolEntry>> = HashMap::new();
        let mut by_id: HashMap<PoolId, PoolEntry> = HashMap::new();
        for entry in entries {
            for asset in entry.pool.assets() {
                by_asset
                    .entry(asset.clone())
                    .or_default()
                    .push(entry.clone());
            }
            by_id.insert(entry.pool.id(), entry);
        }
        Self {
            block,
            taken_at,
            by_asset,
            by_id,
        }
    }

    /// Pools that quote `asset` on at least one side.
    pub fn pools_from(&self, asset: &AssetId) -> &[PoolEntry] {
        self.by_asset.get(asset).map_or(&[], Vec::as_slice)
    }

    /// The entry for a specific pool id, if present.
    pub fn get(&self, id: &PoolId) -> Option<&PoolEntry> {
        self.by_id.get(id)
    }

    /// Every asset touched by at least one pool.
    pub fn assets(&self) -> impl Iterator<Item = &AssetId> {
        self.by_asset.keys()
    }

    /// Block height this snapshot was taken at.
    pub fn block(&self) -> u64 {
        self.block
    }

    /// Wall-clock time this snapshot was assembled.
    pub fn taken_at(&self) -> OffsetDateTime {
        self.taken_at
    }

    /// Number of distinct pools in the snapshot.
    pub fn pool_count(&self) -> usize {
        self.by_id.len()
    }
}

pub trait PoolStore: Send + Sync {
    /// The latest consistent snapshot for `chain`.
    fn snapshot(&self, chain: &ChainId) -> Arc<PoolSnapshot>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::asset::{Amount, Pair};

    struct TwoAssetPool {
        id: PoolId,
        assets: Vec<AssetId>,
    }

    impl Pool for TwoAssetPool {
        fn id(&self) -> PoolId {
            self.id.clone()
        }
        fn assets(&self) -> &[AssetId] {
            &self.assets
        }
        fn quote(&self, _pair: &Pair, amount_in: Amount) -> Option<Amount> {
            Some(amount_in)
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    fn entry(id: &str, a: &str, b: &str) -> PoolEntry {
        PoolEntry {
            pool: Arc::new(TwoAssetPool {
                id: PoolId::new(id),
                assets: vec![AssetId::new(a).unwrap(), AssetId::new(b).unwrap()],
            }),
            meta: PoolMeta {
                synced_block: 100,
                synced_at: OffsetDateTime::UNIX_EPOCH,
            },
        }
    }

    #[test]
    fn indexes_by_asset_and_id() {
        let snap = PoolSnapshot::from_entries(
            100,
            OffsetDateTime::UNIX_EPOCH,
            vec![
                entry("p1", "ethereum:usdc", "ethereum:weth"),
                entry("p2", "ethereum:usdc", "ethereum:dai"),
            ],
        );

        // USDC touches both pools; WETH only the first.
        assert_eq!(
            snap.pools_from(&AssetId::new("ethereum:usdc").unwrap())
                .len(),
            2
        );
        assert_eq!(
            snap.pools_from(&AssetId::new("ethereum:weth").unwrap())
                .len(),
            1
        );
        // Unknown asset yields an empty slice, not a panic.
        assert!(
            snap.pools_from(&AssetId::new("ethereum:xyz").unwrap())
                .is_empty()
        );
        // Direct id resolution.
        assert!(snap.get(&PoolId::new("p1")).is_some());
        assert!(snap.get(&PoolId::new("nope")).is_none());
        assert_eq!(snap.block(), 100);
    }
}
