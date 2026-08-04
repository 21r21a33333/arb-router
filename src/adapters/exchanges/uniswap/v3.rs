//! Uniswap V3 concentrated-liquidity pool.
//!
//! A thin adapter over `amm_core`'s shared tick engine: it holds an
//! `amm_core::UniswapV3Pool` built from the synced tick set and delegates
//! quoting. The Q64.96 tick-crossing math lives once, in `amm-core`. Also serves
//! Aerodrome Slipstream (identical math).

use std::collections::HashMap;

use alloy_primitives::U256;
use amm_core::primitives::asset::AssetAmount as CoreAmount;
use amm_core::primitives::pool::PoolId as CorePoolId;
use amm_core::protocols::uniswap::v3::{
    TickData as CoreTickData, TickInfo as CoreTickInfo, UniswapV3Pool as CorePool,
};
use amm_core::traits::pool::Pool as CorePoolTrait;

use crate::adapters::exchanges::{amount_to_u256, core_asset, u256_to_amount};
use crate::core::deps::pool::Pool;
use crate::primitives::asset::{Amount, AssetId, Pair};
use crate::primitives::pool::PoolId;

/// Tick-level liquidity info for an initialized tick. Produced by the fetch
/// adapters and reconstructed into the amm-core tick set.
#[derive(Debug, Clone)]
pub struct TickInfo {
    pub liquidity_net: i128,
    pub initialized: bool,
}

/// Uniswap V3 concentrated-liquidity pool (also suitable for Aerodrome
/// Slipstream — identical math).
#[derive(Debug, Clone)]
pub struct UniswapV3Pool {
    id: PoolId,
    assets: [AssetId; 2],
    inner: CorePool,
}

impl UniswapV3Pool {
    /// Construct a pool from on-chain slot0 + liquidity + tick state. The bitmap
    /// and decimals are accepted for call-site compatibility; amm-core rebuilds
    /// the bitmap from the tick set, and the integer math ignores decimals.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: &str,
        token0: AssetId,
        token1: AssetId,
        sqrt_price_x96: U256,
        liquidity: u128,
        tick: i32,
        ticks: HashMap<i32, TickInfo>,
        _tick_bitmap: HashMap<i16, U256>,
        fee: u32,
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
            core_pair(&token0, &token1),
            sqrt_price_x96,
            liquidity,
            tick,
            fee,
            CoreTickData::from_ticks(tick_spacing, core_ticks),
        );
        Self {
            id: PoolId::new(id),
            assets: [token0, token1],
            inner,
        }
    }

    /// Test helper: exact-input swap output in a direction, via the shared engine.
    #[cfg(test)]
    fn simulate_swap(&self, zero_for_one: bool, amount_in: U256) -> Option<U256> {
        let (from, to) = match zero_for_one {
            true => (&self.assets[0], &self.assets[1]),
            false => (&self.assets[1], &self.assets[0]),
        };
        core_quote(&self.inner, from, to, amount_in)
    }
}

/// Build the amm-core asset pair from two arb-router asset ids.
pub(crate) fn core_pair(
    token0: &AssetId,
    token1: &AssetId,
) -> [amm_core::primitives::asset::AssetId; 2] {
    [
        core_asset(token0).expect("asset id is chain:token"),
        core_asset(token1).expect("asset id is chain:token"),
    ]
}

/// Quote `from -> to` through an amm-core pool, mapping a zero input/output to
/// `None` (arb-router's convention).
pub(crate) fn core_quote(
    inner: &impl CorePoolTrait,
    from: &AssetId,
    to: &AssetId,
    amount_in: U256,
) -> Option<U256> {
    if amount_in.is_zero() {
        return None;
    }
    let out = inner
        .quote(
            &CoreAmount::new(core_asset(from)?, amount_in),
            &core_asset(to)?,
        )
        .ok()?;
    match out.raw.is_zero() {
        true => None,
        false => Some(out.raw),
    }
}

impl Pool for UniswapV3Pool {
    fn id(&self) -> PoolId {
        self.id.clone()
    }

    fn assets(&self) -> &[AssetId] {
        &self.assets
    }

    fn quote(&self, pair: &Pair, amount_in: Amount) -> Option<Amount> {
        let raw = amount_to_u256(amount_in)?;
        let out = core_quote(&self.inner, &pair.source, &pair.destination, raw)?;
        u256_to_amount(out)
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

    /// Full-range USDC/WETH pool at tick 0 (1:1 price), 0.30% fee, 1e18 liquidity.
    fn make_test_pool() -> UniswapV3Pool {
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
        UniswapV3Pool::new(
            "ethereum:univ3:0xtest",
            usdc(),
            weth(),
            U256::from(79228162514264337593543950336u128), // tick 0
            liq as u128,
            0,
            ticks,
            tick_bitmap,
            3000,
            60,
            6,
            18,
        )
    }

    #[test]
    fn simulate_swap_zero_for_one_produces_output() {
        let pool = make_test_pool();
        let amount_in = U256::from(1_000_000_000u64);
        let out = pool.simulate_swap(true, amount_in).unwrap();
        assert!(out > U256::ZERO && out < amount_in);
        assert!(out > amount_in * U256::from(990u64) / U256::from(1000u64));
    }

    #[test]
    fn simulate_swap_one_for_zero_produces_output() {
        let pool = make_test_pool();
        let amount_in = U256::from(1_000_000_000u64);
        let out = pool.simulate_swap(false, amount_in).unwrap();
        assert!(out > U256::ZERO && out < amount_in);
        assert!(out > amount_in * U256::from(990u64) / U256::from(1000u64));
    }

    #[test]
    fn simulate_swap_at_parity_output_less_than_input() {
        let pool = make_test_pool();
        let amount_in = U256::from(1_000_000_000_000u64);
        let out = pool.simulate_swap(true, amount_in).unwrap();
        assert!(out < amount_in);
        assert!(out > amount_in * U256::from(996u64) / U256::from(1000u64));
    }

    #[test]
    fn simulate_swap_zero_amount_returns_none() {
        let pool = make_test_pool();
        assert!(pool.simulate_swap(true, U256::ZERO).is_none());
    }

    #[test]
    fn quote_usdc_to_weth_matches_simulate_swap() {
        let pool = make_test_pool();
        let pair = Pair {
            source: usdc(),
            destination: weth(),
        };
        let out = pool
            .quote(&pair, Amount(Decimal::from(1_000_000_000u64)))
            .unwrap();
        let expected = u256_to_amount(
            pool.simulate_swap(true, U256::from(1_000_000_000u64))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(out, expected);
    }

    #[test]
    fn quote_weth_to_usdc_produces_output() {
        let pool = make_test_pool();
        let pair = Pair {
            source: weth(),
            destination: usdc(),
        };
        let out = pool
            .quote(&pair, Amount(Decimal::from(1_000_000_000u64)))
            .unwrap();
        assert!(out.0 > Decimal::ZERO);
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
    fn quote_mismatched_destination_returns_none() {
        let pool = make_test_pool();
        let pair = Pair {
            source: usdc(),
            destination: usdc(),
        };
        assert!(
            pool.quote(&pair, Amount(Decimal::from(1_000_000u64)))
                .is_none()
        );
    }

    #[test]
    fn pool_id_roundtrips() {
        assert_eq!(make_test_pool().id().as_str(), "ethereum:univ3:0xtest");
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

    /// A thin full-range base (1e17) plus a concentrated position (9e17) in
    /// [-60, 60]; a swap large enough to cross tick -60 must quote at a strictly
    /// worse rate than a tiny non-crossing one (price impact through the engine).
    fn make_two_position_pool() -> UniswapV3Pool {
        let mut ticks = HashMap::new();
        let mut tick_bitmap = HashMap::new();
        let wide = 100_000_000_000_000_000i128;
        let conc = 900_000_000_000_000_000i128;
        for (t, net) in [
            (-887220i32, wide),
            (-60, conc),
            (60, -conc),
            (887220, -wide),
        ] {
            ticks.insert(
                t,
                TickInfo {
                    liquidity_net: net,
                    initialized: true,
                },
            );
            set_tick_bitmap_bit(&mut tick_bitmap, t, 60);
        }
        UniswapV3Pool::new(
            "ethereum:univ3:0x2pos",
            usdc(),
            weth(),
            U256::from(79228162514264337593543950336u128),
            (wide + conc) as u128,
            0,
            ticks,
            tick_bitmap,
            3000,
            60,
            6,
            18,
        )
    }

    #[test]
    fn simulate_swap_crosses_tick_with_worse_rate() {
        let pool = make_two_position_pool();
        let big_in = U256::from(5_000_000_000_000_000u64);
        let big_out = pool
            .simulate_swap(true, big_in)
            .expect("tick-crossing swap must quote");
        assert!(big_out > U256::ZERO && big_out < big_in);
        let tiny_in = U256::from(1_000_000_000u64);
        let tiny_out = pool
            .simulate_swap(true, tiny_in)
            .expect("tiny swap must quote");
        assert!(
            big_out * tiny_in < tiny_out * big_in,
            "crossing swap {big_out}/{big_in} must be worse than tiny {tiny_out}/{tiny_in}"
        );
    }
}
