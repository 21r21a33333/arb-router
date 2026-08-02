//! Curve pool quoter backed by the `curve-math` engine, covering every StableSwap and
//! CryptoSwap variant. This wrapper adapts the chain-agnostic `AssetId` interface onto
//! `curve_math::Pool`'s index-based `get_amount_out`.

use curve_math::Pool as CurveMathPool;

use crate::core::deps::pool::Pool;
use crate::primitives::asset::{Amount, AssetId, Pair};
use crate::primitives::pool::PoolId;

/// A Curve pool of any variant.
///
/// `coins` is ordered to match the coin indices of `inner`, so a `(source, destination)`
/// pair resolves to the `(i, j)` indices `curve_math` expects.
#[derive(Clone)]
pub struct CurvePool {
    id: PoolId,
    coins: Vec<AssetId>,
    inner: CurveMathPool,
}

impl CurvePool {
    /// Wrap a built `curve_math::Pool` with its coin ordering.
    pub fn new(id: &str, coins: Vec<AssetId>, inner: CurveMathPool) -> Self {
        Self {
            id: PoolId::new(id),
            coins,
            inner,
        }
    }
}

impl Pool for CurvePool {
    fn id(&self) -> PoolId {
        self.id.clone()
    }

    fn assets(&self) -> &[AssetId] {
        &self.coins
    }

    fn quote(&self, pair: &Pair, amount_in: Amount) -> Option<Amount> {
        super::quote_n_asset(&self.coins, pair, amount_in, |i, j, dx| {
            self.inner.get_amount_out(i, j, dx)
        })
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    use rust_decimal::Decimal;

    fn asset(s: &str) -> AssetId {
        AssetId::new(s).unwrap()
    }

    /// A balanced 3-coin StableSwap (all coins normalised to 1e18 via `rates`),
    /// 1,000,000 units of each. Values are for wiring, not a specific live pool.
    fn stable_3pool() -> CurvePool {
        let e18 = U256::from(1_000_000_000_000_000_000u64);
        let bal = e18 * U256::from(1_000_000u64);
        let inner = CurveMathPool::StableSwapV1 {
            balances: vec![bal, bal, bal],
            rates: vec![e18, e18, e18],
            amp: U256::from(2_000u64),
            fee: U256::from(1_000_000u64), // 0.01% (1e10 denominator)
        };
        CurvePool::new(
            "ethereum:curve:0x3pool",
            vec![
                asset("ethereum:dai"),
                asset("ethereum:usdc"),
                asset("ethereum:usdt"),
            ],
            inner,
        )
    }

    #[test]
    fn quote_near_parity_between_coins() {
        let pool = stable_3pool();
        let pair = Pair {
            source: asset("ethereum:dai"),
            destination: asset("ethereum:usdc"),
        };
        let dx = Amount(Decimal::from(1_000_000_000_000_000_000u64)); // 1 coin (1e18)
        let out = pool
            .quote(&pair, dx)
            .expect("balanced stableswap must quote");
        // Near parity minus a tiny fee: (0.99e18, 1e18).
        assert!(out.0 > Decimal::from(990_000_000_000_000_000u64));
        assert!(out.0 < Decimal::from(1_000_000_000_000_000_000u64));
    }

    #[test]
    fn assets_lists_all_three_coins() {
        assert_eq!(stable_3pool().assets().len(), 3);
    }

    #[test]
    fn quote_unknown_asset_returns_none() {
        let pool = stable_3pool();
        let pair = Pair {
            source: asset("ethereum:weth"),
            destination: asset("ethereum:usdc"),
        };
        assert!(
            pool.quote(&pair, Amount(Decimal::from(1_000_000u64)))
                .is_none()
        );
    }

    #[test]
    fn quote_same_asset_returns_none() {
        let pool = stable_3pool();
        let pair = Pair {
            source: asset("ethereum:dai"),
            destination: asset("ethereum:dai"),
        };
        assert!(
            pool.quote(&pair, Amount(Decimal::from(1_000_000u64)))
                .is_none()
        );
    }

    #[test]
    fn quote_zero_amount_returns_none() {
        let pool = stable_3pool();
        let pair = Pair {
            source: asset("ethereum:dai"),
            destination: asset("ethereum:usdc"),
        };
        assert!(pool.quote(&pair, Amount(Decimal::ZERO)).is_none());
    }
}
