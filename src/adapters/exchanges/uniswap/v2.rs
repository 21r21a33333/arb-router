//! Uniswap V2 constant-product pool quoter.
//!
//! Formula: `out = (in * (10000 - fee_bps) * reserve_out)
//!               / (reserve_in * 10000 + in * (10000 - fee_bps))`

use alloy_primitives::U256;

use crate::core::deps::pool::Pool;
use crate::primitives::asset::{Amount, AssetId, Pair};
use crate::primitives::pool::PoolId;

// ─── struct ───────────────────────────────────────────────────────────────────

/// Uniswap V2 (and V2-fork) constant-product pool.
///
/// Reserves and fee arithmetic operate on `U256` base-unit integers, matching
/// the on-chain `UniswapV2Library.getAmountOut` implementation exactly.
#[derive(Debug, Clone)]
pub struct UniswapV2Pool {
    id: PoolId,
    /// token0 asset id (the lower-address token in the pair)
    pub token0: AssetId,
    /// token1 asset id
    pub token1: AssetId,
    /// On-chain reserve for token0 (base units)
    pub reserve0: U256,
    /// On-chain reserve for token1 (base units)
    pub reserve1: U256,
    /// Fee in basis points (e.g. 30 = 0.3%)
    pub fee_bps: u32,
    /// Decimal places for token0 (informational, not used in integer math)
    pub decimals0: u8,
    /// Decimal places for token1 (informational, not used in integer math)
    pub decimals1: u8,
    /// Cached slice used by `Pool::assets`
    assets: [AssetId; 2],
}

impl UniswapV2Pool {
    /// Construct a pool from reserve data.
    ///
    /// `fee_bps` – fee in basis points (30 for the standard 0.3% pool).
    /// `decimals0`/`decimals1` – token decimal places (informational only).
    #[allow(clippy::too_many_arguments)]
    pub fn from_reserves(
        id: &str,
        token0: &str,
        token1: &str,
        reserve0: U256,
        reserve1: U256,
        fee_bps: u32,
        decimals0: u8,
        decimals1: u8,
    ) -> Result<Self, crate::primitives::asset::AssetIdError> {
        let t0 = AssetId::new(token0)?;
        let t1 = AssetId::new(token1)?;
        Ok(Self {
            id: PoolId::new(id),
            assets: [t0.clone(), t1.clone()],
            token0: t0,
            token1: t1,
            reserve0,
            reserve1,
            fee_bps,
            decimals0,
            decimals1,
        })
    }

    // ─── integer math ───────────────────────────

    /// Compute the output amount for an exact-input swap.
    ///
    /// `zero_for_one`: if `true`, swapping token0 → token1; otherwise token1 → token0.
    ///
    /// Returns `None` when:
    /// - `amount_in` is zero
    /// - either reserve is zero
    /// - an arithmetic overflow occurs (extremely large inputs)
    fn simulate_swap(&self, zero_for_one: bool, amount_in: U256) -> Option<U256> {
        match amount_in.is_zero() {
            true => None,
            false => {
                let (reserve_in, reserve_out) = match zero_for_one {
                    true => (self.reserve0, self.reserve1),
                    false => (self.reserve1, self.reserve0),
                };

                match (reserve_in.is_zero(), reserve_out.is_zero()) {
                    (true, _) | (_, true) => None,
                    (false, false) => {
                        let fee_factor = U256::from(10_000u32.checked_sub(self.fee_bps)?);
                        let amount_in_with_fee = amount_in.checked_mul(fee_factor)?;
                        let numerator = amount_in_with_fee.checked_mul(reserve_out)?;
                        let denominator = reserve_in
                            .checked_mul(U256::from(10_000u32))?
                            .checked_add(amount_in_with_fee)?;

                        match denominator.is_zero() {
                            true => None,
                            false => Some(numerator / denominator),
                        }
                    }
                }
            }
        }
    }
}

// ─── Pool trait impl ──────────────────────────────────────────────────────────

impl Pool for UniswapV2Pool {
    fn id(&self) -> PoolId {
        self.id.clone()
    }

    fn assets(&self) -> &[AssetId] {
        &self.assets
    }

    fn quote(&self, pair: &Pair, amount_in: Amount) -> Option<Amount> {
        super::quote_two_asset(
            &self.token0,
            &self.token1,
            pair,
            amount_in,
            |zero_for_one, amt| self.simulate_swap(zero_for_one, amt),
        )
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;

    // ── helpers to build a canonical USDC/WETH test pool ──────────────────────

    /// Pool reserves:
    ///   reserve0 (USDC) = 1_000_000_000_000  (1 M USDC, 6 dec)
    ///   reserve1 (WETH) = 500_000_000_000_000_000_000  (500 WETH, 18 dec)
    ///   fee_bps = 30 (0.3 %)
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

    // ── internal U256 golden-vector tests ────────────────

    /// Swap 1000 USDC (1_000_000_000 base units) → WETH.
    ///
    /// Exact expected value computed offline via the V2 formula, pinned as an exact
    /// integer so any future formula drift is caught (a sanity range is also asserted).
    #[test]
    fn simulate_swap_token0_to_token1_exact() {
        let pool = usdc_weth_pool();
        let amount_in = U256::from(1_000_000_000u64); // 1000 USDC
        let out = pool.simulate_swap(true, amount_in).unwrap();
        // Exact: (1_000_000_000 * 9970 * 500e18) / (1e12 * 10000 + 1_000_000_000 * 9970)
        assert_eq!(out, U256::from(498_003_490_519_951_608u64));
        // Sanity: within the expected range
        assert!(out > U256::from(490_000_000_000_000_000u64));
        assert!(out < U256::from(500_000_000_000_000_000u64));
    }

    /// Swap 1 WETH (1_000_000_000_000_000_000 base units) → USDC.
    #[test]
    fn simulate_swap_token1_to_token0_exact() {
        let pool = usdc_weth_pool();
        let amount_in = U256::from(1_000_000_000_000_000_000u64); // 1 WETH
        let out = pool.simulate_swap(false, amount_in).unwrap();
        // Exact: (1e18 * 9970 * 1e12) / (500e18 * 10000 + 1e18 * 9970)
        assert_eq!(out, U256::from(1_990_031_876u64));
        // Sanity: within the expected range
        assert!(out > U256::from(1_990_000_000u64));
        assert!(out < U256::from(2_000_000_000u64));
    }

    /// Zero amount must return None.
    #[test]
    fn simulate_swap_zero_amount_returns_none() {
        let pool = usdc_weth_pool();
        assert!(pool.simulate_swap(true, U256::ZERO).is_none());
    }

    /// Zero reserves must return None.
    #[test]
    fn simulate_swap_zero_reserves_returns_none() {
        let pool = UniswapV2Pool::from_reserves(
            "ethereum:univ2:0xdead",
            "ethereum:usdc",
            "ethereum:weth",
            U256::ZERO,
            U256::ZERO,
            30,
            6,
            18,
        )
        .unwrap();
        assert!(pool.simulate_swap(true, U256::from(100u64)).is_none());
    }

    // ── Pool::quote wrapper test ───────────────────────────────────────────────

    /// `quote` must produce the same output as the internal U256 golden vector,
    /// exposed as an `Amount`.
    #[test]
    fn quote_usdc_to_weth_matches_golden_vector() {
        let pool = usdc_weth_pool();
        let pair = Pair {
            source: AssetId::new("ethereum:usdc").unwrap(),
            destination: AssetId::new("ethereum:weth").unwrap(),
        };
        // 1000 USDC in base units
        let amount_in = Amount(Decimal::from(1_000_000_000u64));
        let out = pool.quote(&pair, amount_in).unwrap();
        // Must match the exact golden vector: 498_003_490_519_951_608 base-unit WETH
        assert_eq!(out, Amount(Decimal::from(498_003_490_519_951_608u64)));
    }

    /// `quote` must return None for an unsupported pair.
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

    /// `quote` in the reverse direction (WETH → USDC) must also work.
    #[test]
    fn quote_weth_to_usdc_matches_golden_vector() {
        let pool = usdc_weth_pool();
        let pair = Pair {
            source: AssetId::new("ethereum:weth").unwrap(),
            destination: AssetId::new("ethereum:usdc").unwrap(),
        };
        // 1 WETH in base units
        let amount_in = Amount(Decimal::from(1_000_000_000_000_000_000u64));
        let out = pool.quote(&pair, amount_in).unwrap();
        assert_eq!(out, Amount(Decimal::from(1_990_031_876u64)));
    }

    // (base-unit Amount↔U256 conversion is tested in `exchanges::tests`)
}
