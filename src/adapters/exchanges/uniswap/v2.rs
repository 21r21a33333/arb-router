//! Uniswap V2 (and V2-fork) constant-product pool.
//!
//! A thin adapter over `amm_core`'s wei-exact `UniswapV2Pool`: it holds the
//! amm-core quoter and translates arb-router's `Pair`/`Amount` at the boundary.
//! The swap math lives once, in `amm-core`.

use alloy_primitives::U256;
use amm_core::primitives::asset::AssetAmount as CoreAmount;
use amm_core::primitives::pool::PoolId as CorePoolId;
use amm_core::protocols::uniswap::v2::UniswapV2Pool as CorePool;
use amm_core::traits::pool::Pool as CorePoolTrait;

use crate::adapters::exchanges::{amount_to_u256, core_asset, u256_to_amount};
use crate::core::deps::pool::Pool;
use crate::primitives::asset::{Amount, AssetId, Pair};
use crate::primitives::pool::PoolId;

/// Uniswap V2 (and V2-fork, e.g. Aerodrome volatile) constant-product pool.
#[derive(Debug, Clone)]
pub struct UniswapV2Pool {
    id: PoolId,
    assets: [AssetId; 2],
    inner: CorePool,
}

impl UniswapV2Pool {
    /// Construct a pool from reserve data.
    ///
    /// `fee_bps` – fee in basis points (30 for the standard 0.3% pool).
    /// `decimals0`/`decimals1` – token decimal places (not used by the integer
    /// swap math; retained for call-site compatibility).
    #[allow(clippy::too_many_arguments)]
    pub fn from_reserves(
        id: &str,
        token0: &str,
        token1: &str,
        reserve0: U256,
        reserve1: U256,
        fee_bps: u32,
        _decimals0: u8,
        _decimals1: u8,
    ) -> Result<Self, crate::primitives::asset::AssetIdError> {
        let t0 = AssetId::new(token0)?;
        let t1 = AssetId::new(token1)?;
        let c0 = core_asset(&t0).ok_or_else(|| bad(token0))?;
        let c1 = core_asset(&t1).ok_or_else(|| bad(token1))?;
        let inner = CorePool::new(CorePoolId::new(id), [c0, c1], [reserve0, reserve1], fee_bps);
        Ok(Self {
            id: PoolId::new(id),
            assets: [t0, t1],
            inner,
        })
    }
}

fn bad(token: &str) -> crate::primitives::asset::AssetIdError {
    crate::primitives::asset::AssetIdError::BadFormat(token.to_string())
}

impl Pool for UniswapV2Pool {
    fn id(&self) -> PoolId {
        self.id.clone()
    }

    fn assets(&self) -> &[AssetId] {
        &self.assets
    }

    fn quote(&self, pair: &Pair, amount_in: Amount) -> Option<Amount> {
        let from = core_asset(&pair.source)?;
        let to = core_asset(&pair.destination)?;
        let raw = amount_to_u256(amount_in)?;
        let out = self.inner.quote(&CoreAmount::new(from, raw), &to).ok()?;
        u256_to_amount(out.raw)
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;

    /// Pool reserves: 1 M USDC (6 dec) / 500 WETH (18 dec), 0.3% fee.
    fn usdc_weth_pool() -> UniswapV2Pool {
        UniswapV2Pool::from_reserves(
            "ethereum:univ2:0xtest",
            "ethereum:usdc",
            "ethereum:weth",
            U256::from(1_000_000_000_000u64),
            U256::from(500_000_000_000_000_000_000u128),
            30,
            6,
            18,
        )
        .unwrap()
    }

    /// `quote` reproduces the exact V2 golden vector — now a differential check
    /// that arb-router's boundary + amm-core's math match the pinned value.
    #[test]
    fn quote_usdc_to_weth_matches_golden_vector() {
        let pool = usdc_weth_pool();
        let pair = Pair {
            source: AssetId::new("ethereum:usdc").unwrap(),
            destination: AssetId::new("ethereum:weth").unwrap(),
        };
        let out = pool
            .quote(&pair, Amount(Decimal::from(1_000_000_000u64))) // 1000 USDC base units
            .unwrap();
        assert_eq!(out, Amount(Decimal::from(498_003_490_519_951_608u64)));
    }

    #[test]
    fn quote_weth_to_usdc_matches_golden_vector() {
        let pool = usdc_weth_pool();
        let pair = Pair {
            source: AssetId::new("ethereum:weth").unwrap(),
            destination: AssetId::new("ethereum:usdc").unwrap(),
        };
        let out = pool
            .quote(&pair, Amount(Decimal::from(1_000_000_000_000_000_000u64))) // 1 WETH
            .unwrap();
        assert_eq!(out, Amount(Decimal::from(1_990_031_876u64)));
    }

    #[test]
    fn quote_unknown_pair_returns_none() {
        let pool = usdc_weth_pool();
        let pair = Pair {
            source: AssetId::new("ethereum:dai").unwrap(),
            destination: AssetId::new("ethereum:weth").unwrap(),
        };
        assert!(
            pool.quote(&pair, Amount(Decimal::from(1_000_000u64)))
                .is_none()
        );
    }
}
