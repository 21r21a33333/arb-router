//! Aerodrome (Solidly) **stable** pool — the `x³y + y³x` invariant.
//!
//! A thin adapter over `amm_core`'s `AerodromeStablePool`: it holds the amm-core
//! quoter and delegates. The Newton-solved Solidly math (`_k` / `_get_y` / …,
//! wei-exact vs `Pool.sol`) lives once, in `amm-core`.

use alloy_primitives::U256;
use amm_core::primitives::pool::PoolId as CorePoolId;
use amm_core::protocols::aerodrome::stable::AerodromeStablePool as CorePool;

use crate::adapters::exchanges::uniswap::v3::{core_pair, core_quote};
use crate::adapters::exchanges::{amount_to_u256, u256_to_amount};
use crate::core::deps::pool::Pool;
use crate::primitives::asset::{Amount, AssetId, AssetIdError, Pair};
use crate::primitives::pool::PoolId;

/// An Aerodrome stable (Solidly `x³y + y³x`) pool.
#[derive(Debug, Clone)]
pub struct AerodromeStablePool {
    id: PoolId,
    assets: [AssetId; 2],
    inner: CorePool,
}

impl AerodromeStablePool {
    /// Build a stable pool from reserves, token decimal counts (e.g. 6, 18), and
    /// the swap fee (out of 10_000).
    #[allow(clippy::too_many_arguments)]
    pub fn from_reserves(
        id: &str,
        token0: &str,
        token1: &str,
        reserve0: U256,
        reserve1: U256,
        decimals0: u8,
        decimals1: u8,
        fee_bps: u32,
    ) -> Result<Self, AssetIdError> {
        let t0 = AssetId::new(token0)?;
        let t1 = AssetId::new(token1)?;
        let inner = CorePool::new(
            CorePoolId::new(id),
            core_pair(&t0, &t1),
            [reserve0, reserve1],
            [decimals0, decimals1],
            fee_bps,
        );
        Ok(Self {
            id: PoolId::new(id),
            assets: [t0, t1],
            inner,
        })
    }

    /// Test helper: exact-input swap output in a direction, via amm-core.
    #[cfg(test)]
    fn get_amount_out(&self, amount_in: U256, zero_for_one: bool) -> Option<U256> {
        let (from, to) = match zero_for_one {
            true => (&self.assets[0], &self.assets[1]),
            false => (&self.assets[1], &self.assets[0]),
        };
        core_quote(&self.inner, from, to, amount_in)
    }
}

impl Pool for AerodromeStablePool {
    fn id(&self) -> PoolId {
        self.id.clone()
    }

    fn assets(&self) -> &[AssetId] {
        &self.assets
    }

    fn quote(&self, pair: &Pair, amount_in: Amount) -> Option<Amount> {
        let raw = amount_to_u256(amount_in)?;
        u256_to_amount(core_quote(
            &self.inner,
            &pair.source,
            &pair.destination,
            raw,
        )?)
    }
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;

    fn dai() -> AssetId {
        AssetId::new("base:dai").unwrap()
    }
    fn usdc() -> AssetId {
        AssetId::new("base:usdc").unwrap()
    }

    /// A value-balanced USDC(6)/DAI(18) stable pool: 1M of each, 0.05% fee.
    fn usdc_dai_pool() -> AerodromeStablePool {
        AerodromeStablePool::from_reserves(
            "base:aero-stable:0xtest",
            "base:usdc",
            "base:dai",
            U256::from(1_000_000_u128 * 1_000_000),
            U256::from(1_000_000_u128 * 1_000_000_000_000_000_000),
            6,
            18,
            5,
        )
        .unwrap()
    }

    fn symmetric_pool(fee_bps: u32) -> AerodromeStablePool {
        let r = U256::from(1_000_000_u128 * 1_000_000_000_000_000_000);
        AerodromeStablePool::from_reserves(
            "base:aero-stable:0xsym",
            "base:dai",
            "base:usdc",
            r,
            r,
            18,
            18,
            fee_bps,
        )
        .unwrap()
    }

    /// On a value-balanced stable pool, 1 unit in returns just under 1 unit out
    /// (fee + tiny slippage) — the stable curve, delegated to amm-core.
    #[test]
    fn balanced_pool_swaps_near_parity() {
        let pool = usdc_dai_pool();
        let out = pool
            .get_amount_out(U256::from(1_000_000u64), true)
            .expect("must quote");
        assert!(
            out < U256::from(1_000_000_000_000_000_000u128),
            "out {out} must be < 1 DAI"
        );
        assert!(
            out > U256::from(990_000_000_000_000_000u128),
            "out {out} must be > 0.99 DAI"
        );
    }

    /// A symmetric pool must quote identically in both directions for equal input.
    #[test]
    fn symmetric_pool_is_direction_symmetric() {
        let pool = symmetric_pool(5);
        let amount = U256::from(1_000_000_000_000_000_000u128);
        assert_eq!(
            pool.get_amount_out(amount, true).unwrap(),
            pool.get_amount_out(amount, false).unwrap()
        );
    }

    /// Regression: a fixed 1000-USDC swap yields ~1000 DAI on a deep balanced pool.
    #[test]
    fn get_amount_out_is_stable_for_fixed_input() {
        let pool = usdc_dai_pool();
        let out = pool
            .get_amount_out(U256::from(1_000_000_000u64), true)
            .unwrap();
        assert!(out > U256::from(998_000_000_000_000_000_000u128));
        assert!(out < U256::from(1_000_000_000_000_000_000_000u128));
    }

    #[test]
    fn zero_input_and_empty_reserves_return_none() {
        let pool = usdc_dai_pool();
        assert!(pool.get_amount_out(U256::ZERO, true).is_none());
        let empty = AerodromeStablePool::from_reserves(
            "base:aero-stable:0xdead",
            "base:usdc",
            "base:dai",
            U256::ZERO,
            U256::ZERO,
            6,
            18,
            5,
        )
        .unwrap();
        assert!(
            empty
                .get_amount_out(U256::from(1_000_000u64), true)
                .is_none()
        );
    }

    #[test]
    fn quote_resolves_direction_and_rejects_unknown_pair() {
        let pool = usdc_dai_pool();
        let out = pool
            .quote(
                &Pair {
                    source: usdc(),
                    destination: dai(),
                },
                Amount(Decimal::from(1_000_000u64)),
            )
            .unwrap();
        assert!(out.0 > Decimal::ZERO);
        assert!(
            pool.quote(
                &Pair {
                    source: AssetId::new("base:weth").unwrap(),
                    destination: dai(),
                },
                Amount(Decimal::from(1_000_000u64)),
            )
            .is_none()
        );
    }
}
