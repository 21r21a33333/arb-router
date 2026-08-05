//! In-memory port implementations for exercising the application engine with no
//! real I/O. Test-only.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rust_decimal::Decimal;
use time::OffsetDateTime;

use crate::core::deps::notifier::{Notifier, NotifyError};
use crate::core::deps::pool::Pool;
use crate::core::deps::pool_store::{PoolEntry, PoolMeta, PoolSnapshot, PoolStore};
use crate::core::deps::valuation::{Valuation, ValuationError};
use crate::primitives::asset::{Amount, AssetId, ChainId, Pair, Usd};
use crate::primitives::opportunity::Opportunity;
use crate::primitives::pool::PoolId;

/// Default public RPC for the live block-height test (override with `ETH_RPC_URL`).
pub const ETH_RPC_DEFAULT: &str = "https://ethereum-rpc.publicnode.com";

/// A pool that returns `amount_in * rate` for any supported pair, in either
/// direction. Enough to drive graph, path, profit, and ranking logic.
pub struct FakePool {
    id: PoolId,
    assets: Vec<AssetId>,
    rate: Decimal,
}

impl FakePool {
    /// A `dyn Pool` handle over the given assets and fixed rate. Returns the
    /// boxed trait object callers actually need rather than a bare `Self`.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(id: &str, assets: &[&str], rate: Decimal) -> Arc<dyn Pool> {
        Arc::new(Self {
            id: PoolId::new(id),
            assets: assets.iter().map(|a| AssetId::new(a).unwrap()).collect(),
            rate,
        })
    }

    fn holds(&self, asset: &AssetId) -> bool {
        self.assets.iter().any(|a| a == asset)
    }
}

impl Pool for FakePool {
    fn id(&self) -> PoolId {
        self.id.clone()
    }

    fn assets(&self) -> &[AssetId] {
        &self.assets
    }

    fn quote(&self, pair: &Pair, amount_in: Amount) -> Option<Amount> {
        match self.holds(&pair.source)
            && self.holds(&pair.destination)
            && pair.source != pair.destination
        {
            true => Some(Amount(amount_in.0 * self.rate)),
            false => None,
        }
    }
}

/// Wrap pools in a snapshot stamped fresh at the current time.
pub fn fake_snapshot(pools: Vec<Arc<dyn Pool>>) -> Arc<PoolSnapshot> {
    let meta = PoolMeta {
        synced_block: 1,
        synced_at: OffsetDateTime::now_utc(),
    };
    let entries = pools
        .into_iter()
        .map(|pool| PoolEntry { pool, meta })
        .collect();
    Arc::new(PoolSnapshot::from_entries(
        1,
        OffsetDateTime::now_utc(),
        entries,
    ))
}

/// A store that always serves the same pre-built snapshot.
pub struct StaticPoolStore(pub Arc<PoolSnapshot>);

impl PoolStore for StaticPoolStore {
    fn snapshot(&self, _chain: &ChainId) -> Arc<PoolSnapshot> {
        self.0.clone()
    }
}

/// A fixed price table.
pub struct FakeValuation(HashMap<AssetId, Usd>);

impl FakeValuation {
    pub fn new(prices: &[(&str, Usd)]) -> Self {
        Self(
            prices
                .iter()
                .map(|(a, p)| (AssetId::new(a).unwrap(), *p))
                .collect(),
        )
    }
}

#[async_trait]
impl Valuation for FakeValuation {
    async fn price(&self, asset: &AssetId) -> Result<Usd, ValuationError> {
        self.0
            .get(asset)
            .copied()
            .ok_or_else(|| ValuationError::NotFound(asset.as_str().to_string()))
    }
}

/// A notifier that keeps every opportunity it is handed for later assertion.
#[derive(Clone, Default)]
pub struct RecordingNotifier(Arc<Mutex<Vec<Opportunity>>>);

impl RecordingNotifier {
    pub fn recorded(&self) -> Vec<Opportunity> {
        self.0.lock().unwrap().clone()
    }
}

#[async_trait]
impl Notifier for RecordingNotifier {
    async fn notify(&self, _chain: &ChainId, opps: &[Opportunity]) -> Result<(), NotifyError> {
        self.0.lock().unwrap().extend(opps.iter().cloned());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_valuation_returns_configured_price() {
        let v = FakeValuation::new(&[("ethereum:usdc", Usd(Decimal::ONE))]);
        let price = v
            .price(&AssetId::new("ethereum:usdc").unwrap())
            .await
            .unwrap();
        assert_eq!(price, Usd(Decimal::ONE));
        assert!(
            v.price(&AssetId::new("ethereum:weth").unwrap())
                .await
                .is_err()
        );
    }
}
