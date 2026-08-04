//! Uniswap V4 concentrated-liquidity pool.
//!
//! Identical tick math to V3, delegated to `amm_core`'s V4 quoter. V4 pools live
//! in a singleton `PoolManager` and are identified by a 32-byte `pool_id`; fees
//! are per-direction (LP fee compounded with the V4 protocol fee).

use std::collections::HashMap;

use alloy_primitives::{B256, U256};
use amm_core::primitives::pool::PoolId as CorePoolId;
use amm_core::protocols::uniswap::v4::{
    Hooks, TickData as CoreTickData, TickInfo as CoreTickInfo, UniswapV4Pool as CorePool,
};

use super::v3::{TickInfo, core_pair, core_quote};
use crate::adapters::exchanges::{amount_to_u256, u256_to_amount};
use crate::core::deps::pool::Pool;
use crate::primitives::asset::{Amount, AssetId, Pair};
use crate::primitives::pool::PoolId;

/// Uniswap V4 concentrated-liquidity pool.
#[derive(Debug, Clone)]
pub struct UniswapV4Pool {
    id: PoolId,
    /// The 32-byte pool identifier (hash of the pool key).
    pub pool_id: [u8; 32],
    assets: [AssetId; 2],
    inner: CorePool,
}

impl UniswapV4Pool {
    /// Construct a V4 pool from on-chain slot0 + liquidity + tick state.
    /// `fee_zero_for_one`/`fee_one_for_zero` are the effective per-direction fees
    /// (pips), already compounded by the fetch adapter. The bitmap/decimals are
    /// accepted for call-site compatibility (amm-core rebuilds the bitmap).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: &str,
        pool_id: [u8; 32],
        token0: AssetId,
        token1: AssetId,
        sqrt_price_x96: U256,
        liquidity: u128,
        tick: i32,
        ticks: HashMap<i32, TickInfo>,
        _tick_bitmap: HashMap<i16, U256>,
        fee_zero_for_one: u32,
        fee_one_for_zero: u32,
        tick_spacing: i32,
        _decimals0: u8,
        _decimals1: u8,
    ) -> Self {
        let core_ticks = ticks.into_iter().map(|(t, i)| {
            (
                t,
                CoreTickInfo {
                    liquidity_net: i.liquidity_net,
                    initialized: i.initialized,
                },
            )
        });
        let inner = CorePool::new(
            CorePoolId::new(id),
            B256::from(pool_id),
            core_pair(&token0, &token1),
            sqrt_price_x96,
            liquidity,
            tick,
            fee_zero_for_one,
            fee_one_for_zero,
            CoreTickData::from_ticks(tick_spacing, core_ticks),
            Hooks::None,
        );
        Self {
            id: PoolId::new(id),
            pool_id,
            assets: [token0, token1],
            inner,
        }
    }

    #[cfg(test)]
    fn simulate_swap(&self, zero_for_one: bool, amount_in: U256) -> Option<U256> {
        let (from, to) = match zero_for_one {
            true => (&self.assets[0], &self.assets[1]),
            false => (&self.assets[1], &self.assets[0]),
        };
        core_quote(&self.inner, from, to, amount_in)
    }
}

impl Pool for UniswapV4Pool {
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

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;

    fn usdc() -> AssetId {
        AssetId::new("ethereum:usdc").unwrap()
    }
    fn weth() -> AssetId {
        AssetId::new("ethereum:weth").unwrap()
    }

    use super::super::set_tick_bitmap_bit;

    /// Full-range USDC/WETH V4 pool at tick 0 (1:1), 0.30% both directions.
    fn make_test_pool() -> UniswapV4Pool {
        let mut ticks = HashMap::new();
        let mut tick_bitmap = HashMap::new();
        let (lower, upper) = (-887220i32, 887220i32);
        let liq: i128 = 1_000_000_000_000_000_000;
        ticks.insert(
            lower,
            TickInfo {
                liquidity_net: liq,
                initialized: true,
            },
        );
        ticks.insert(
            upper,
            TickInfo {
                liquidity_net: -liq,
                initialized: true,
            },
        );
        set_tick_bitmap_bit(&mut tick_bitmap, lower, 60);
        set_tick_bitmap_bit(&mut tick_bitmap, upper, 60);
        UniswapV4Pool::new(
            "ethereum:univ4:0xtest",
            [0xAA; 32],
            usdc(),
            weth(),
            U256::from(79228162514264337593543950336u128),
            liq as u128,
            0,
            ticks,
            tick_bitmap,
            3000,
            3000,
            60,
            6,
            18,
        )
    }

    #[test]
    fn simulate_swap_zero_for_one_produces_output() {
        let pool = make_test_pool();
        let amount_in = U256::from(1_000_000u64);
        let out = pool.simulate_swap(true, amount_in).unwrap();
        assert!(out > U256::ZERO && out < amount_in);
        assert!(out > amount_in * U256::from(990u64) / U256::from(1000u64));
    }

    #[test]
    fn simulate_swap_one_for_zero_produces_output() {
        let pool = make_test_pool();
        let amount_in = U256::from(1_000_000u64);
        let out = pool.simulate_swap(false, amount_in).unwrap();
        assert!(out > U256::ZERO && out < amount_in);
        assert!(out > amount_in * U256::from(990u64) / U256::from(1000u64));
    }

    #[test]
    fn simulate_swap_zero_amount_returns_none() {
        let pool = make_test_pool();
        assert!(pool.simulate_swap(true, U256::ZERO).is_none());
    }

    #[test]
    fn quote_matches_simulate_swap() {
        let pool = make_test_pool();
        let pair = Pair {
            source: usdc(),
            destination: weth(),
        };
        let out = pool
            .quote(&pair, Amount(Decimal::from(1_000_000u64)))
            .unwrap();
        let expected =
            u256_to_amount(pool.simulate_swap(true, U256::from(1_000_000u64)).unwrap()).unwrap();
        assert_eq!(out, expected);
    }

    #[test]
    fn quote_weth_to_usdc_produces_output() {
        let pool = make_test_pool();
        let pair = Pair {
            source: weth(),
            destination: usdc(),
        };
        assert!(
            pool.quote(&pair, Amount(Decimal::from(1_000_000u64)))
                .unwrap()
                .0
                > Decimal::ZERO
        );
    }

    #[test]
    fn quote_unknown_pair_returns_none() {
        let pool = make_test_pool();
        let pair = Pair {
            source: AssetId::new("ethereum:dai").unwrap(),
            destination: weth(),
        };
        assert!(
            pool.quote(&pair, Amount(Decimal::from(1_000_000u64)))
                .is_none()
        );
    }

    #[test]
    fn quote_zero_amount_returns_none() {
        let pool = make_test_pool();
        let pair = Pair {
            source: usdc(),
            destination: weth(),
        };
        assert!(pool.quote(&pair, Amount(Decimal::ZERO)).is_none());
    }

    #[test]
    fn quote_negative_amount_returns_none() {
        let pool = make_test_pool();
        let pair = Pair {
            source: usdc(),
            destination: weth(),
        };
        assert!(pool.quote(&pair, Amount(Decimal::from(-1i64))).is_none());
    }

    #[test]
    fn pool_id_roundtrips() {
        assert_eq!(make_test_pool().id().as_str(), "ethereum:univ4:0xtest");
    }

    #[test]
    fn pool_id_bytes_stored_correctly() {
        assert_eq!(make_test_pool().pool_id, [0xAA; 32]);
    }

    #[test]
    fn pool_assets_contains_both_tokens() {
        let pool = make_test_pool();
        assert_eq!(pool.assets(), &[usdc(), weth()]);
    }

    #[test]
    fn pool_is_object_safe() {
        let _: Box<dyn Pool> = Box::new(make_test_pool());
    }
}
