//! Uniswap V4 concentrated-liquidity pool quoter.
//!
//! V4 uses identical tick-crossing math to V3 — the swap simulation is
//! delegated to `simulate_v3_swap` in the `v3` module.  The key structural
//! difference from V3 is that pools are identified by a 32-byte `pool_id`
//! (hash of the pool key) rather than a per-pool contract address, and they
//! live inside a singleton `PoolManager` contract.
//!

use std::collections::HashMap;

use alloy_primitives::U256;

use super::v3::{TickInfo, simulate_v3_swap};
use crate::core::deps::pool::Pool;
use crate::primitives::asset::{Amount, AssetId, Pair};
use crate::primitives::pool::PoolId;

// ─── struct ───────────────────────────────────────────────────────────────────

/// Uniswap V4 concentrated-liquidity pool.
///
/// Uses identical tick math to V3.  The pool lives inside a singleton
/// `PoolManager` and is identified by its 32-byte `pool_id`.
#[derive(Debug, Clone)]
pub struct UniswapV4Pool {
    id: PoolId,
    /// Unique 32-byte pool identifier (hash of the pool key).
    pub pool_id: [u8; 32],
    /// Token0 asset id (lower-sorted in the pair).
    pub token0: AssetId,
    /// Token1 asset id.
    pub token1: AssetId,
    /// Fee in hundredths of a basis point (e.g. 3000 = 0.30%).
    pub fee: u32,
    /// Tick spacing for this fee tier (e.g. 60 for the 0.30% tier).
    pub tick_spacing: i32,
    /// Current active liquidity in the tick range containing the price.
    pub liquidity: u128,
    /// Current sqrt(price) encoded as Q64.96 fixed-point.
    pub sqrt_price_x96: U256,
    /// Current tick corresponding to `sqrt_price_x96`.
    pub tick: i32,
    /// Initialized tick data keyed by tick index.
    pub ticks: HashMap<i32, TickInfo>,
    /// Tick bitmap words keyed by word position (i16).
    pub tick_bitmap: HashMap<i16, U256>,
    /// Decimal places for token0 (informational; not used in integer math).
    pub decimals0: u8,
    /// Decimal places for token1 (informational; not used in integer math).
    pub decimals1: u8,
    /// Cached slice used by `Pool::assets`.
    assets: [AssetId; 2],
}

impl UniswapV4Pool {
    /// Construct a new V4 pool from on-chain slot0 + liquidity + tick state.
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
        tick_bitmap: HashMap<i16, U256>,
        fee: u32,
        tick_spacing: i32,
        decimals0: u8,
        decimals1: u8,
    ) -> Self {
        let assets = [token0.clone(), token1.clone()];
        Self {
            id: PoolId::new(id),
            pool_id,
            token0,
            token1,
            sqrt_price_x96,
            liquidity,
            tick,
            ticks,
            tick_bitmap,
            fee,
            tick_spacing,
            decimals0,
            decimals1,
            assets,
        }
    }

    // ─── integer swap math (delegated to V3) ─────────────

    /// Compute the output amount for an exact-input swap using Q64.96 tick-crossing math.
    ///
    /// V4 math is identical to V3 — this delegates to `simulate_v3_swap`.
    ///
    /// `zero_for_one`: `true` means token0 → token1, `false` means token1 → token0.
    fn simulate_swap(&self, zero_for_one: bool, amount_in: U256) -> Option<U256> {
        simulate_v3_swap(
            self.sqrt_price_x96,
            self.tick,
            self.liquidity,
            &self.ticks,
            &self.tick_bitmap,
            self.fee,
            self.tick_spacing,
            zero_for_one,
            amount_in,
        )
    }
}

// ─── Pool trait impl ──────────────────────────────────────────────────────────

impl Pool for UniswapV4Pool {
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

    fn usdc() -> AssetId {
        AssetId::new("ethereum:usdc").unwrap()
    }
    fn weth() -> AssetId {
        AssetId::new("ethereum:weth").unwrap()
    }

    use super::super::set_tick_bitmap_bit;

    /// Build a full-range USDC/WETH V4 pool at tick 0 (1:1 price).
    ///
    /// Golden-vector pool state:
    ///   - pool_id = [0xAA; 32]
    ///   - token0 = USDC, token1 = WETH
    ///   - tick_spacing = 60, fee = 3000 (0.30%)
    ///   - sqrt_price_x96 = 2^96 = 79228162514264337593543950336 (tick 0 → 1:1)
    ///   - liquidity = 1_000_000_000_000_000_000
    ///   - Full-range position: lower = -887220, upper = 887220
    fn make_test_pool() -> UniswapV4Pool {
        let mut ticks = HashMap::new();
        let mut tick_bitmap = HashMap::new();

        let lower = -887220i32;
        let upper = 887220i32;
        let liq: i128 = 1_000_000_000_000_000_000;
        let tick_spacing = 60i32;

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

        set_tick_bitmap_bit(&mut tick_bitmap, lower, tick_spacing);
        set_tick_bitmap_bit(&mut tick_bitmap, upper, tick_spacing);

        UniswapV4Pool::new(
            "ethereum:univ4:0xtest",
            [0xAA; 32],
            usdc(),
            weth(),
            // tick 0 → sqrtPriceX96 = 2^96
            U256::from(79228162514264337593543950336u128),
            liq as u128,
            0,
            ticks,
            tick_bitmap,
            3000,
            tick_spacing,
            6,
            18,
        )
    }

    // ── internal U256 golden-vector tests ────────────

    /// Golden vector: swap token0→token1 (USDC→WETH).
    /// At 1:1 price with 0.3% fee, output must be > 0 and < input.
    #[test]
    fn simulate_swap_zero_for_one_produces_output() {
        let pool = make_test_pool();
        let amount_in = U256::from(1_000_000u64);
        let out = pool.simulate_swap(true, amount_in).unwrap();
        assert!(out > U256::ZERO, "output must be non-zero");
        // At 1:1 price the raw output must be less than input (fee taken)
        assert!(
            out < amount_in,
            "output {out} should be less than input {amount_in} due to fee"
        );
        // Must still be within 1% of input (sanity)
        assert!(
            out > amount_in * U256::from(990u64) / U256::from(1000u64),
            "output {out} should be > 99% of input {amount_in}"
        );
    }

    /// Golden vector: swap token1→token0 (WETH→USDC).
    #[test]
    fn simulate_swap_one_for_zero_produces_output() {
        let pool = make_test_pool();
        let amount_in = U256::from(1_000_000u64);
        let out = pool.simulate_swap(false, amount_in).unwrap();
        assert!(out > U256::ZERO);
        assert!(out < amount_in);
        assert!(out > amount_in * U256::from(990u64) / U256::from(1000u64));
    }

    /// Golden vector: at 1:1 price, 0.3% fee, larger amount.
    #[test]
    fn simulate_swap_larger_amount_is_directionally_consistent() {
        let pool = make_test_pool();
        let amount_in = U256::from(1_000_000_000_000u64); // 1e12 base units
        let out = pool.simulate_swap(true, amount_in).unwrap();
        assert!(
            out < amount_in,
            "output should be less than input due to fees"
        );
        assert!(
            out > amount_in * U256::from(996u64) / U256::from(1000u64),
            "output should be close to input minus 0.3% fee"
        );
    }

    /// Zero amount_in must return None.
    #[test]
    fn simulate_swap_zero_amount_returns_none() {
        let pool = make_test_pool();
        assert!(pool.simulate_swap(true, U256::ZERO).is_none());
    }

    /// Unknown token (mismatched direction) — only guards direction at Pool::quote level.
    /// Pool with no tick data must return None.
    #[test]
    fn simulate_swap_empty_ticks_returns_none() {
        let mut pool = make_test_pool();
        pool.ticks.clear();
        assert!(pool.simulate_swap(true, U256::from(1_000_000u64)).is_none());
    }

    /// Pool with no bitmap data must return None.
    #[test]
    fn simulate_swap_empty_bitmap_returns_none() {
        let mut pool = make_test_pool();
        pool.tick_bitmap.clear();
        assert!(pool.simulate_swap(true, U256::from(1_000_000u64)).is_none());
    }

    // ── Pool::quote wrapper tests ─────────────────────────────────────────────

    /// `quote` USDC→WETH must produce a non-zero Amount.
    #[test]
    fn quote_usdc_to_weth_produces_output() {
        let pool = make_test_pool();
        let pair = Pair {
            source: usdc(),
            destination: weth(),
        };
        let amount_in = Amount(Decimal::from(1_000_000u64));
        let out = pool.quote(&pair, amount_in).unwrap();
        assert!(out.0 > Decimal::ZERO);
    }

    /// `quote` must round-trip through simulate_swap identically.
    #[test]
    fn quote_matches_simulate_swap() {
        let pool = make_test_pool();
        let pair = Pair {
            source: usdc(),
            destination: weth(),
        };
        let amount_in = Amount(Decimal::from(1_000_000u64));
        let out = pool.quote(&pair, amount_in).unwrap();

        let u256_in = U256::from(1_000_000u64);
        let u256_out = pool.simulate_swap(true, u256_in).unwrap();
        let expected = crate::adapters::exchanges::u256_to_amount(u256_out).unwrap();
        assert_eq!(out, expected, "quote must round-trip through simulate_swap");
    }

    /// `quote` WETH→USDC must also work.
    #[test]
    fn quote_weth_to_usdc_produces_output() {
        let pool = make_test_pool();
        let pair = Pair {
            source: weth(),
            destination: usdc(),
        };
        let amount_in = Amount(Decimal::from(1_000_000u64));
        let out = pool.quote(&pair, amount_in).unwrap();
        assert!(out.0 > Decimal::ZERO);
    }

    /// `quote` with an unsupported pair must return None.
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

    /// `quote` with zero amount must return None.
    #[test]
    fn quote_zero_amount_returns_none() {
        let pool = make_test_pool();
        let pair = Pair {
            source: usdc(),
            destination: weth(),
        };
        assert!(pool.quote(&pair, Amount(Decimal::ZERO)).is_none());
    }

    /// `quote` with a negative Amount must return None.
    #[test]
    fn quote_negative_amount_returns_none() {
        let pool = make_test_pool();
        let pair = Pair {
            source: usdc(),
            destination: weth(),
        };
        assert!(pool.quote(&pair, Amount(Decimal::from(-1i64))).is_none());
    }

    // ── Pool trait surface tests ──────────────────────────────────────────────

    #[test]
    fn pool_id_roundtrips() {
        let pool = make_test_pool();
        assert_eq!(pool.id().as_str(), "ethereum:univ4:0xtest");
    }

    #[test]
    fn pool_assets_contains_both_tokens() {
        let pool = make_test_pool();
        let assets = pool.assets();
        assert_eq!(assets.len(), 2);
        assert_eq!(assets[0], usdc());
        assert_eq!(assets[1], weth());
    }

    #[test]
    fn pool_id_bytes_stored_correctly() {
        let pool = make_test_pool();
        assert_eq!(pool.pool_id, [0xAA; 32]);
    }

    #[test]
    fn pool_is_object_safe() {
        let pool = make_test_pool();
        let _: Box<dyn Pool> = Box::new(pool);
    }
}
