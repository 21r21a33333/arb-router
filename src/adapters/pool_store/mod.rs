//! Snapshot store and the per-chain sync worker.
//!
//! [`ArcSwapPoolStore`] holds one lock-free, atomically-swappable snapshot per
//! chain — readers `load` the current one without blocking. [`SyncWorker`]
//! produces those snapshots: each tick it pins the latest block, discovers and
//! refreshes every exchange's pools at that block, and publishes the result.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use time::OffsetDateTime;

use crate::core::deps::chain_reader::{ChainReadError, ChainReader};
use crate::core::deps::exchange::{Exchange, ExchangeError};
use crate::core::deps::pool::Pool;
use crate::core::deps::pool_store::{PoolEntry, PoolMeta, PoolSnapshot, PoolStore};
use crate::primitives::asset::{AssetId, ChainId};
use crate::primitives::chain::BlockId;

/// An empty snapshot, served for chains that have not synced yet.
fn empty_snapshot() -> PoolSnapshot {
    PoolSnapshot::from_entries(0, OffsetDateTime::UNIX_EPOCH, Vec::new())
}

/// Per-chain, lock-free snapshot store backed by `arc-swap`.
pub struct ArcSwapPoolStore {
    snapshots: HashMap<ChainId, ArcSwap<PoolSnapshot>>,
}

impl ArcSwapPoolStore {
    /// A store pre-registered for `chains`, each starting empty.
    pub fn new(chains: &[ChainId]) -> Self {
        let snapshots = chains
            .iter()
            .map(|c| (c.clone(), ArcSwap::from_pointee(empty_snapshot())))
            .collect();
        Self { snapshots }
    }

    /// Atomically publish a new snapshot for `chain` (no-op for unregistered chains).
    pub fn store(&self, chain: &ChainId, snapshot: Arc<PoolSnapshot>) {
        if let Some(slot) = self.snapshots.get(chain) {
            slot.store(snapshot);
        }
    }
}

impl PoolStore for ArcSwapPoolStore {
    fn snapshot(&self, chain: &ChainId) -> Arc<PoolSnapshot> {
        match self.snapshots.get(chain) {
            Some(slot) => slot.load_full(),
            None => Arc::new(empty_snapshot()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("chain read failed: {0}")]
    Read(#[from] ChainReadError),
    #[error("exchange failed: {0}")]
    Exchange(#[from] ExchangeError),
}

/// Drives one chain's sync loop: discover + refresh + publish, per tick.
pub struct SyncWorker {
    pub chain: ChainId,
    pub store: Arc<ArcSwapPoolStore>,
    pub exchanges: Vec<Arc<dyn Exchange>>,
    pub reader: Arc<dyn ChainReader>,
    pub tracked_tokens: Vec<AssetId>,
    pub interval: Duration,
}

impl SyncWorker {
    /// Run one sync tick: pin the block, refresh every supporting exchange's
    /// pools at that block, and publish the assembled snapshot. Returns the block
    /// synced.
    pub async fn refresh_once(&self) -> Result<u64, SyncError> {
        let at = self.reader.latest_block(&self.chain).await?;
        let now = OffsetDateTime::now_utc();

        // Refresh every supporting exchange concurrently — each is RPC-bound, so
        // running them in parallel collapses the tick to the slowest exchange
        // rather than their sum. A single exchange failing is logged and skipped,
        // not fatal to the whole snapshot.
        let refreshes = self
            .exchanges
            .iter()
            .filter(|exchange| exchange.supports(&self.chain))
            .map(|exchange| self.refresh_exchange(exchange.as_ref(), at));
        let results = futures_util::future::join_all(refreshes).await;

        let mut entries = Vec::new();
        for result in results {
            match result {
                Ok(pools) => entries.extend(pools.into_iter().map(|pool| PoolEntry {
                    pool: Arc::from(pool),
                    meta: PoolMeta {
                        synced_block: at,
                        synced_at: now,
                    },
                })),
                Err(err) => tracing::warn!(
                    chain = self.chain.as_str(),
                    error = %err,
                    "exchange refresh failed; skipping"
                ),
            }
        }

        self.store.store(
            &self.chain,
            Arc::new(PoolSnapshot::from_entries(at, now, entries)),
        );
        Ok(at)
    }

    /// Discover then refresh one exchange's pools at block `at`.
    async fn refresh_exchange(
        &self,
        exchange: &dyn Exchange,
        at: u64,
    ) -> Result<Vec<Box<dyn Pool>>, ExchangeError> {
        let keys = exchange.discover(&self.chain, &self.tracked_tokens).await?;
        exchange.refresh(&keys, BlockId::Number(at)).await
    }

    /// Sync repeatedly, sleeping `interval` between ticks. A failed tick is
    /// logged and retried next interval — the loop never dies.
    pub async fn run(self) {
        loop {
            match self.refresh_once().await {
                Ok(block) => {
                    tracing::info!(chain = self.chain.as_str(), block, "sync tick complete")
                }
                Err(err) => tracing::warn!(
                    chain = self.chain.as_str(),
                    error = %err,
                    "sync tick failed"
                ),
            }
            tokio::time::sleep(self.interval).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::deps::chain_reader::ChainReader;
    use crate::core::deps::exchange::Exchange;
    use crate::core::deps::pool::Pool;
    use crate::primitives::asset::{Amount, Pair};
    use crate::primitives::pool::{ExchangeId, PoolId, PoolKey};
    use async_trait::async_trait;

    fn asset(s: &str) -> AssetId {
        AssetId::new(s).unwrap()
    }

    struct StubPool {
        id: PoolId,
        assets: Vec<AssetId>,
    }

    impl Pool for StubPool {
        fn id(&self) -> PoolId {
            self.id.clone()
        }
        fn assets(&self) -> &[AssetId] {
            &self.assets
        }
        fn quote(&self, _pair: &Pair, amount_in: Amount) -> Option<Amount> {
            Some(amount_in)
        }
    }

    /// Discovers one USDC/WETH pool and refreshes it into a `StubPool`.
    struct FakeExchange {
        id: ExchangeId,
        chain: ChainId,
    }

    #[async_trait]
    impl Exchange for FakeExchange {
        fn id(&self) -> ExchangeId {
            self.id.clone()
        }
        fn supports(&self, chain: &ChainId) -> bool {
            chain == &self.chain
        }
        async fn discover(
            &self,
            _chain: &ChainId,
            _tokens: &[AssetId],
        ) -> Result<Vec<PoolKey>, ExchangeError> {
            Ok(vec![PoolKey {
                exchange: self.id.clone(),
                chain: self.chain.clone(),
                address: "0xpool".into(),
                assets: vec![asset("ethereum:usdc"), asset("ethereum:weth")],
                fee_bps: Some(3_000),
            }])
        }
        async fn refresh(
            &self,
            keys: &[PoolKey],
            _at: BlockId,
        ) -> Result<Vec<Box<dyn Pool>>, ExchangeError> {
            Ok(keys
                .iter()
                .map(|k| {
                    Box::new(StubPool {
                        id: PoolId::new(&k.address),
                        assets: k.assets.clone(),
                    }) as Box<dyn Pool>
                })
                .collect())
        }
    }

    struct FakeReader {
        block: u64,
    }

    #[async_trait]
    impl ChainReader for FakeReader {
        async fn latest_block(&self, _chain: &ChainId) -> Result<u64, ChainReadError> {
            Ok(self.block)
        }
    }

    #[tokio::test]
    async fn one_tick_publishes_a_block_pinned_snapshot() {
        let chain = ChainId::new("ethereum");
        let store = Arc::new(ArcSwapPoolStore::new(std::slice::from_ref(&chain)));
        let worker = SyncWorker {
            chain: chain.clone(),
            store: store.clone(),
            exchanges: vec![Arc::new(FakeExchange {
                id: ExchangeId::new("univ3"),
                chain: chain.clone(),
            })],
            reader: Arc::new(FakeReader { block: 12_345 }),
            tracked_tokens: vec![asset("ethereum:usdc"), asset("ethereum:weth")],
            interval: Duration::from_secs(1),
        };

        // Store starts empty.
        assert_eq!(
            store
                .snapshot(&chain)
                .pools_from(&asset("ethereum:usdc"))
                .len(),
            0
        );

        let block = worker.refresh_once().await.unwrap();
        assert_eq!(block, 12_345);

        // After one tick the pool is served and the snapshot is block-pinned.
        let snap = store.snapshot(&chain);
        assert_eq!(snap.block(), 12_345);
        assert_eq!(snap.pools_from(&asset("ethereum:usdc")).len(), 1);
        assert!(snap.get(&PoolId::new("0xpool")).is_some());
    }
}
