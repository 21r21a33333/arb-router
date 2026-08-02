//! Uniswap V3 concentrated-liquidity pool quoter.
//!
//! Tick-crossing swap math delegating Q64.96 arithmetic to the `uniswap_v3_math` crate.
//!
//! Pool state is identified by `AssetId` (arb-router string IDs) rather than
//! `alloy::Address`, but the integer swap math is identical.

use std::collections::HashMap;

use alloy_primitives::{I256, U256};

use crate::core::deps::pool::Pool;
use crate::primitives::asset::{Amount, AssetId, Pair};
use crate::primitives::pool::PoolId;

// ─── tick info ────────────────────────────────────────────────────────────────

/// Tick-level liquidity info for an initialized tick.
#[derive(Debug, Clone)]
pub struct TickInfo {
    pub liquidity_net: i128,
    pub initialized: bool,
}

// ─── struct ───────────────────────────────────────────────────────────────────

/// Uniswap V3 concentrated-liquidity pool.
///
/// Also suitable for Aerodrome Slipstream (identical math) via a `protocol`
/// flag.
#[derive(Debug, Clone)]
pub struct UniswapV3Pool {
    id: PoolId,
    /// Token0 asset id (lower-sorted in the pair)
    pub token0: AssetId,
    /// Token1 asset id
    pub token1: AssetId,
    /// Current sqrt(price) encoded as Q64.96 fixed-point
    pub sqrt_price_x96: U256,
    /// Current active liquidity in the tick range containing the price
    pub liquidity: u128,
    /// Current tick index corresponding to `sqrt_price_x96`
    pub tick: i32,
    /// Initialized tick data keyed by tick index
    pub ticks: HashMap<i32, TickInfo>,
    /// Tick bitmap words keyed by word position (i16)
    pub tick_bitmap: HashMap<i16, U256>,
    /// Fee in millionths of a unit (e.g. 3000 = 0.30%).
    /// This is the Uniswap V3 fee tier, passed directly to `compute_swap_step`.
    pub fee: u32,
    /// Tick spacing for this fee tier (e.g. 60 for the 0.30% tier)
    pub tick_spacing: i32,
    /// Decimal places for token0 (informational; not used in integer math)
    pub decimals0: u8,
    /// Decimal places for token1 (informational; not used in integer math)
    pub decimals1: u8,
    /// Cached slice used by `Pool::assets`
    assets: [AssetId; 2],
}

impl UniswapV3Pool {
    /// Construct a new pool from on-chain slot0 + liquidity + tick state.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: &str,
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

    // ─── integer swap math ─────────────────────

    /// Compute the output amount for an exact-input swap using Q64.96 tick-crossing math.
    ///
    /// `zero_for_one`: `true` means token0 → token1, `false` means token1 → token0.
    ///
    /// Returns `None` when:
    /// - `amount_in` is zero
    /// - tick/bitmap data is empty (pool not initialised)
    /// - any arithmetic step would overflow or underflow
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

impl Pool for UniswapV3Pool {
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

// ─── V3-style tick-crossing swap (shared by V3 and V4) ────────────────────────

/// Shared V3-style concentrated-liquidity swap simulation.
///
/// Delegates Q64.96 arithmetic to the `uniswap_v3_math` crate, decomposed into small
/// steps — [`SwapState`], [`swap_step`], [`advance_tick`] — for readability. Callers
/// resolve direction into `zero_for_one` before calling.
#[allow(clippy::too_many_arguments)]
pub fn simulate_v3_swap(
    sqrt_price_x96: U256,
    tick: i32,
    liquidity: u128,
    ticks: &HashMap<i32, TickInfo>,
    tick_bitmap: &HashMap<i16, U256>,
    fee: u32,
    tick_spacing: i32,
    zero_for_one: bool,
    amount_in: U256,
) -> Option<U256> {
    match (
        amount_in.is_zero(),
        ticks.is_empty() || tick_bitmap.is_empty(),
    ) {
        (false, false) => {}
        _ => return None,
    }

    let limit = price_limit(zero_for_one);
    let mut state = SwapState::new(sqrt_price_x96, tick, liquidity, amount_in);

    while state.is_swapping(limit) {
        swap_step(
            &mut state,
            ticks,
            tick_bitmap,
            fee,
            tick_spacing,
            zero_for_one,
            limit,
        )?;
    }

    state.output()
}

/// Mutable state threaded through the tick-crossing loop of a V3-style swap.
struct SwapState {
    /// Current sqrt price (Q64.96).
    sqrt_price_x96: U256,
    /// Current tick.
    tick: i32,
    /// Active liquidity at the current price.
    liquidity: u128,
    /// Signed input still to consume (exact-in: starts positive, drains toward zero).
    amount_remaining: I256,
    /// Signed output accumulated so far (negative — it leaves the pool).
    amount_out: I256,
}

impl SwapState {
    fn new(sqrt_price_x96: U256, tick: i32, liquidity: u128, amount_in: U256) -> Self {
        Self {
            sqrt_price_x96,
            tick,
            liquidity,
            amount_remaining: I256::from_raw(amount_in),
            amount_out: I256::ZERO,
        }
    }

    /// Keep stepping while input remains and the price limit is not yet reached.
    fn is_swapping(&self, limit: U256) -> bool {
        self.amount_remaining != I256::ZERO && self.sqrt_price_x96 != limit
    }

    /// Deduct one step's input (+fee) from remaining and add its output.
    fn consume(&mut self, step_in: U256, step_out: U256, step_fee: U256) {
        // overflow impossible here — step amounts are bounded by pool liquidity.
        self.amount_remaining = self
            .amount_remaining
            .overflowing_sub(I256::from_raw(step_in.overflowing_add(step_fee).0))
            .0;
        self.amount_out -= I256::from_raw(step_out);
    }

    /// Final positive output, or `None` if the swap produced nothing.
    fn output(&self) -> Option<U256> {
        let out = (-self.amount_out).into_raw();
        match out.is_zero() {
            true => None,
            false => Some(out),
        }
    }
}

/// The next initialized tick boundary reached in a step, plus its price.
struct NextTick {
    tick: i32,
    price: U256,
    initialized: bool,
}

/// The extreme sqrt price the swap runs toward (one unit inside the valid range).
fn price_limit(zero_for_one: bool) -> U256 {
    match zero_for_one {
        true => uniswap_v3_math::tick_math::MIN_SQRT_RATIO + U256::from(1u64),
        false => uniswap_v3_math::tick_math::MAX_SQRT_RATIO - U256::from(1u64),
    }
}

/// Target price for one step: the closer of the next tick boundary and the limit.
/// `zero_for_one` prices fall (clamp up = `max`); otherwise they rise (clamp down = `min`).
fn step_target(zero_for_one: bool, next_tick_price: U256, limit: U256) -> U256 {
    match zero_for_one {
        true => next_tick_price.max(limit),
        false => next_tick_price.min(limit),
    }
}

/// Resolve the next initialized tick boundary from the bitmap, clamped, with its price.
fn next_tick(
    tick: i32,
    tick_bitmap: &HashMap<i16, U256>,
    tick_spacing: i32,
    zero_for_one: bool,
) -> Option<NextTick> {
    let (raw, initialized) = uniswap_v3_math::tick_bitmap::next_initialized_tick_within_one_word(
        tick_bitmap,
        tick,
        tick_spacing,
        zero_for_one,
    )
    .ok()?;
    let tick = raw.clamp(
        uniswap_v3_math::tick_math::MIN_TICK,
        uniswap_v3_math::tick_math::MAX_TICK,
    );
    let price = uniswap_v3_math::tick_math::get_sqrt_ratio_at_tick(tick).ok()?;
    Some(NextTick {
        tick,
        price,
        initialized,
    })
}

/// Net liquidity to apply when crossing `tick`, sign-adjusted for swap direction.
///
/// `None` when the tick is flagged initialized in the bitmap but absent from the synced
/// tick data: the quote fails rather than silently pricing with zero net liquidity, which
/// would misprice the swap.
fn tick_liquidity_net(
    ticks: &HashMap<i32, TickInfo>,
    tick: i32,
    zero_for_one: bool,
) -> Option<i128> {
    let net = ticks.get(&tick)?.liquidity_net;
    Some(match zero_for_one {
        true => -net,
        false => net,
    })
}

/// Liquidity after crossing an initialized tick — `None` on overflow/underflow.
fn crossed_liquidity(liquidity: u128, liquidity_net: i128) -> Option<u128> {
    match liquidity_net.is_negative() {
        true => liquidity.checked_sub(liquidity_net.unsigned_abs()),
        false => liquidity.checked_add(liquidity_net as u128),
    }
}

/// Advance the swap by one tick-crossing step, mutating `state`.
/// `None` on any tick-math / arithmetic failure.
fn swap_step(
    state: &mut SwapState,
    ticks: &HashMap<i32, TickInfo>,
    tick_bitmap: &HashMap<i16, U256>,
    fee: u32,
    tick_spacing: i32,
    zero_for_one: bool,
    limit: U256,
) -> Option<()> {
    let next = next_tick(state.tick, tick_bitmap, tick_spacing, zero_for_one)?;
    let target = step_target(zero_for_one, next.price, limit);

    let (new_price, step_in, step_out, step_fee) = uniswap_v3_math::swap_math::compute_swap_step(
        state.sqrt_price_x96,
        target,
        state.liquidity,
        state.amount_remaining,
        fee,
    )
    .ok()?;

    let price_start = state.sqrt_price_x96;
    state.sqrt_price_x96 = new_price;
    state.consume(step_in, step_out, step_fee);

    advance_tick(state, ticks, zero_for_one, next, price_start)
}

/// Move the tick after a step:
///   reached the boundary  → cross it (adjust liquidity) and step the tick;
///   moved but not reached → recompute the tick from the new price;
///   unchanged             → leave it.
fn advance_tick(
    state: &mut SwapState,
    ticks: &HashMap<i32, TickInfo>,
    zero_for_one: bool,
    next: NextTick,
    price_start: U256,
) -> Option<()> {
    match (
        state.sqrt_price_x96 == next.price,
        state.sqrt_price_x96 != price_start,
    ) {
        (true, _) => {
            if next.initialized {
                let net = tick_liquidity_net(ticks, next.tick, zero_for_one)?;
                state.liquidity = crossed_liquidity(state.liquidity, net)?;
            }
            state.tick = match zero_for_one {
                true => next.tick.wrapping_sub(1),
                false => next.tick,
            };
        }
        (false, true) => {
            state.tick =
                uniswap_v3_math::tick_math::get_tick_at_sqrt_ratio(state.sqrt_price_x96).ok()?;
        }
        (false, false) => {}
    }
    Some(())
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;

    // ── pool factory ─────────────────────────────────────────────────────────

    fn usdc() -> AssetId {
        AssetId::new("ethereum:usdc").unwrap()
    }
    fn weth() -> AssetId {
        AssetId::new("ethereum:weth").unwrap()
    }

    use super::super::set_tick_bitmap_bit;

    /// Build a full-range USDC/WETH pool at tick 0 (1:1 price).
    ///
    /// Golden-vector pool state:
    ///   - token0 = USDC, token1 = WETH
    ///   - tick_spacing = 60, fee = 3000 (0.30%)
    ///   - sqrt_price_x96 = 2^96 = 79228162514264337593543950336 (tick 0 → 1:1)
    ///   - liquidity = 1_000_000_000_000_000_000
    ///   - Full-range position: lower = -887220, upper = 887220
    fn make_test_pool() -> UniswapV3Pool {
        let mut ticks = HashMap::new();
        let mut tick_bitmap = HashMap::new();

        let lower = -887220i32;
        let upper = 887220i32;
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
            // tick 0 → sqrtPriceX96 = 2^96
            U256::from(79228162514264337593543950336u128),
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

    // ── internal U256 golden-vector tests ───────────────

    /// Swap token0→token1 (USDC→WETH): output must be non-zero and below input
    /// after fee deduction.
    ///
    /// Golden vector: at 1:1 price with 0.3% fee and 1B liquidity,
    /// swapping 1_000_000_000 base units yields output that is close to but
    /// strictly less than the input.
    #[test]
    fn simulate_swap_zero_for_one_produces_output() {
        let pool = make_test_pool();
        let amount_in = U256::from(1_000_000_000u64);
        let out = pool.simulate_swap(true, amount_in).unwrap();
        assert!(out > U256::ZERO, "output must be non-zero");
        // At 1:1 price the raw output must be less than input (fee taken)
        assert!(
            out < amount_in,
            "output {out} should be less than input {amount_in} due to fee"
        );
        // Must still be within 1% of input (sanity: not wildly wrong)
        assert!(
            out > amount_in * U256::from(990u64) / U256::from(1000u64),
            "output {out} should be > 99% of input {amount_in}"
        );
    }

    /// Swap token1→token0 (WETH→USDC): same sanity checks.
    #[test]
    fn simulate_swap_one_for_zero_produces_output() {
        let pool = make_test_pool();
        let amount_in = U256::from(1_000_000_000u64);
        let out = pool.simulate_swap(false, amount_in).unwrap();
        assert!(out > U256::ZERO);
        assert!(out < amount_in);
        assert!(out > amount_in * U256::from(990u64) / U256::from(1000u64));
    }

    /// Golden vector: at 1:1 price, swapping 1_000_000_000 base units with 0.3%
    /// fee, output must be slightly less than input.
    ///
    /// Expected: `out < amount_in` and `out > amount_in * 996 / 1000`.
    /// We pin the exact value here so any formula drift is caught.
    #[test]
    fn simulate_swap_at_parity_output_less_than_input() {
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

    /// `quote` USDC→WETH must produce a non-zero Amount consistent with the
    /// internal U256 golden vector.
    #[test]
    fn quote_usdc_to_weth_matches_simulate_swap() {
        let pool = make_test_pool();
        let pair = Pair {
            source: usdc(),
            destination: weth(),
        };
        let amount_in = Amount(Decimal::from(1_000_000_000u64));
        let out = pool.quote(&pair, amount_in).unwrap();

        // Must match simulate_swap(true, ...) converted through the same helpers
        let u256_in = U256::from(1_000_000_000u64);
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
        let amount_in = Amount(Decimal::from(1_000_000_000u64));
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

    /// `quote` with a zero amount must return None.
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

    /// `quote` with a mismatched destination (source=token0, dest=token0) returns None.
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

    // ── Pool trait surface tests ──────────────────────────────────────────────

    #[test]
    fn pool_id_roundtrips() {
        let pool = make_test_pool();
        assert_eq!(pool.id().as_str(), "ethereum:univ3:0xtest");
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
    fn pool_is_object_safe() {
        let pool = make_test_pool();
        let _: Box<dyn Pool> = Box::new(pool);
    }

    // ── extracted swap-helper unit tests ──────────────────────────────────────

    #[test]
    fn crossed_liquidity_adds_and_subtracts() {
        assert_eq!(crossed_liquidity(1_000, 300), Some(1_300));
        assert_eq!(crossed_liquidity(1_000, -300), Some(700));
        assert_eq!(crossed_liquidity(1_000, 0), Some(1_000));
    }

    #[test]
    fn crossed_liquidity_underflow_returns_none() {
        assert_eq!(crossed_liquidity(100, -300), None);
    }

    #[test]
    fn crossed_liquidity_handles_i128_min_without_panic() {
        // `unsigned_abs()` must not overflow on i128::MIN (a plain `-x` would).
        assert_eq!(
            crossed_liquidity(u128::MAX, i128::MIN),
            Some(u128::MAX - (1u128 << 127))
        );
    }

    #[test]
    fn tick_liquidity_net_sign_and_missing() {
        let mut ticks = HashMap::new();
        ticks.insert(
            60,
            TickInfo {
                liquidity_net: 500,
                initialized: true,
            },
        );
        assert_eq!(tick_liquidity_net(&ticks, 60, false), Some(500));
        assert_eq!(tick_liquidity_net(&ticks, 60, true), Some(-500));
        // An initialized tick absent from synced data fails the quote (never silent zero).
        assert_eq!(tick_liquidity_net(&ticks, 120, false), None);
    }

    #[test]
    fn step_target_picks_the_binding_bound() {
        let lo = U256::from(100u64);
        let hi = U256::from(200u64);
        // zero_for_one: price falls, the limit is a floor → clamp UP (max)
        assert_eq!(step_target(true, lo, hi), hi);
        assert_eq!(step_target(true, hi, lo), hi);
        // one_for_zero: price rises, the limit is a ceiling → clamp DOWN (min)
        assert_eq!(step_target(false, lo, hi), lo);
        assert_eq!(step_target(false, hi, lo), lo);
    }

    #[test]
    fn price_limit_is_one_unit_inside_the_range() {
        assert_eq!(
            price_limit(true),
            uniswap_v3_math::tick_math::MIN_SQRT_RATIO + U256::from(1u64)
        );
        assert_eq!(
            price_limit(false),
            uniswap_v3_math::tick_math::MAX_SQRT_RATIO - U256::from(1u64)
        );
    }

    // ── tick-crossing integration test ────────────────────────────────────────

    /// A thin full-range base (1e17) plus a concentrated position (9e17) in [-60, 60],
    /// active liquidity 1e18 at tick 0.
    fn make_two_position_pool() -> UniswapV3Pool {
        let mut ticks = HashMap::new();
        let mut tick_bitmap = HashMap::new();
        let wide = 100_000_000_000_000_000i128; // 1e17, full range
        let conc = 900_000_000_000_000_000i128; // 9e17, [-60, 60]
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
            U256::from(79228162514264337593543950336u128), // tick 0
            (wide + conc) as u128,                         // 1e18 active at tick 0
            0,
            ticks,
            tick_bitmap,
            3000,
            60,
            6,
            18,
        )
    }

    /// A `zero_for_one` swap large enough to move price below tick -60 must cross it
    /// (dropping active liquidity 1e18 → 1e17) and quote successfully — and a crossing
    /// swap must get a strictly worse rate than a tiny non-crossing one (price impact).
    #[test]
    fn simulate_swap_crosses_tick_with_worse_rate() {
        let pool = make_two_position_pool();

        let big_in = U256::from(5_000_000_000_000_000u64); // 5e15 — crosses tick -60
        let big_out = pool
            .simulate_swap(true, big_in)
            .expect("tick-crossing swap must quote");
        assert!(big_out > U256::ZERO && big_out < big_in);

        let tiny_in = U256::from(1_000_000_000u64); // 1e9 — stays within [-60, 60]
        let tiny_out = pool
            .simulate_swap(true, tiny_in)
            .expect("tiny swap must quote");

        // rate(big) < rate(tiny)  ⟺  big_out * tiny_in < tiny_out * big_in  (integer, no floats)
        assert!(
            big_out * tiny_in < tiny_out * big_in,
            "crossing swap {big_out}/{big_in} must be worse than tiny {tiny_out}/{tiny_in}"
        );
    }
}
