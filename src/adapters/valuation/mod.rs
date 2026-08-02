//! USD valuation from live market feeds.
//!
//! Background feeds — a Binance bookTicker WebSocket and a CoinGecko poller —
//! write prices into a shared [`PriceStore`]. The [`Valuation`] port reads the
//! store and never touches the network, so the scan hot path never blocks. When
//! both sources are fresh, Binance (live, liquid) wins over CoinGecko (polled);
//! prices past the staleness window are treated as missing.

pub mod binance;
pub mod coingecko;

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::core::deps::valuation::{Valuation, ValuationError};
use crate::primitives::asset::{AssetId, Usd};

/// A USD price with the time it was observed.
#[derive(Clone, Copy)]
struct PricePoint {
    usd: Usd,
    at: OffsetDateTime,
}

/// Shared USD prices written by the feeds and read by [`CachedValuation`].
pub struct PriceStore {
    binance: RwLock<HashMap<AssetId, PricePoint>>,
    coingecko: RwLock<HashMap<AssetId, PricePoint>>,
    max_staleness: time::Duration,
}

impl PriceStore {
    pub fn new(max_staleness: Duration) -> Self {
        Self {
            binance: RwLock::new(HashMap::new()),
            coingecko: RwLock::new(HashMap::new()),
            max_staleness: time::Duration::try_from(max_staleness).unwrap_or(time::Duration::MAX),
        }
    }

    /// Record a live Binance price.
    pub fn set_binance(&self, asset: AssetId, usd: Usd) {
        set(&self.binance, asset, usd);
    }

    /// Record a polled CoinGecko price.
    pub fn set_coingecko(&self, asset: AssetId, usd: Usd) {
        set(&self.coingecko, asset, usd);
    }

    /// The freshest non-stale price for `asset`, preferring Binance.
    pub fn price(&self, asset: &AssetId, now: OffsetDateTime) -> Option<Usd> {
        self.fresh(&self.binance, asset, now)
            .or_else(|| self.fresh(&self.coingecko, asset, now))
    }

    fn fresh(
        &self,
        source: &RwLock<HashMap<AssetId, PricePoint>>,
        asset: &AssetId,
        now: OffsetDateTime,
    ) -> Option<Usd> {
        let point = *source.read().unwrap().get(asset)?;
        match now - point.at <= self.max_staleness {
            true => Some(point.usd),
            false => None,
        }
    }
}

fn set(source: &RwLock<HashMap<AssetId, PricePoint>>, asset: AssetId, usd: Usd) {
    source.write().unwrap().insert(
        asset,
        PricePoint {
            usd,
            at: OffsetDateTime::now_utc(),
        },
    );
}

/// The [`Valuation`] port over a [`PriceStore`] — a pure, non-blocking read.
pub struct CachedValuation {
    store: Arc<PriceStore>,
}

impl CachedValuation {
    pub fn new(store: Arc<PriceStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Valuation for CachedValuation {
    async fn price(&self, asset: &AssetId) -> Result<Usd, ValuationError> {
        self.store
            .price(asset, OffsetDateTime::now_utc())
            .ok_or_else(|| ValuationError::NotFound(asset.as_str().to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;

    fn asset(s: &str) -> AssetId {
        AssetId::new(s).unwrap()
    }

    #[test]
    fn prefers_binance_when_both_fresh() {
        let store = PriceStore::new(Duration::from_secs(60));
        let weth = asset("ethereum:weth");
        store.set_coingecko(weth.clone(), Usd(Decimal::from(2000)));
        store.set_binance(weth.clone(), Usd(Decimal::from(2001)));
        assert_eq!(
            store.price(&weth, OffsetDateTime::now_utc()),
            Some(Usd(Decimal::from(2001)))
        );
    }

    #[test]
    fn falls_back_to_coingecko_when_binance_absent() {
        let store = PriceStore::new(Duration::from_secs(60));
        let dai = asset("ethereum:dai");
        store.set_coingecko(dai.clone(), Usd(Decimal::from(1)));
        assert_eq!(
            store.price(&dai, OffsetDateTime::now_utc()),
            Some(Usd(Decimal::from(1)))
        );
    }

    #[test]
    fn rejects_stale_prices() {
        let store = PriceStore::new(Duration::from_secs(30));
        let weth = asset("ethereum:weth");
        // A Binance point observed two minutes ago is past the 30s window.
        store.binance.write().unwrap().insert(
            weth.clone(),
            PricePoint {
                usd: Usd(Decimal::from(2000)),
                at: OffsetDateTime::now_utc() - time::Duration::seconds(120),
            },
        );
        assert_eq!(store.price(&weth, OffsetDateTime::now_utc()), None);
    }
}
