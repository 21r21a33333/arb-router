//! Aerodrome **Slipstream** `Exchange`: concentrated-liquidity pools that are a
//! Uniswap V3 fork, so they reuse the V3 [`UniswapV3Pool`] swap math.
//!
//! Two things differ from Uniswap V3, which is why this can't reuse the V3
//! adapter directly:
//! - pools are keyed by **`tickSpacing`** (`getPool(a, b, int24 tickSpacing)`),
//!   not a fee tier; the spacing is configured, and the actual swap fee is read
//!   per pool via `fee()` (it can be set/updated by the gauge).
//! - the `slot0()` layout drops V3's `feeProtocol`, and `ticks()` inserts
//!   staked-liquidity / reward fields — so both need Slipstream's own ABI. Only
//!   `liquidityGross`/`liquidityNet` (still fields 0–1) feed the swap math.
//!
//! Refresh is the same two dependent rounds as V3: slot0 + fee + liquidity, then
//! a window of ticks around the active tick.

use std::collections::HashMap;

use alloy::primitives::aliases::I24;
use alloy::primitives::{Address, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use async_trait::async_trait;

use crate::adapters::exchanges::uniswap::ordered;
use crate::adapters::exchanges::uniswap::v3::{TickInfo, UniswapV3Pool};
use crate::adapters::exchanges::{asset_address, call, pool_address, read_err};
use crate::core::deps::chain_reader::ChainReader;
use crate::core::deps::exchange::{Exchange, ExchangeError};
use crate::core::deps::pool::Pool;
use crate::primitives::asset::{AssetId, ChainId};
use crate::primitives::chain::{BlockId, Call, CallResult};
use crate::primitives::pool::{ExchangeId, PoolKey};

sol! {
    interface ICLFactory {
        function getPool(address tokenA, address tokenB, int24 tickSpacing) external view returns (address pool);
    }

    interface ICLPool {
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            bool unlocked
        );
        function fee() external view returns (uint24);
        function liquidity() external view returns (uint128);
        function ticks(int24 tick) external view returns (
            uint128 liquidityGross,
            int128 liquidityNet,
            int128 stakedLiquidityNet,
            uint256 feeGrowthOutside0X128,
            uint256 feeGrowthOutside1X128,
            uint256 rewardGrowthOutsideX128,
            int56 tickCumulativeOutside,
            uint160 secondsPerLiquidityOutsideX128,
            uint32 secondsOutside,
            bool initialized
        );
    }
}

/// Tick-spacings read on each side of the active tick during refresh (as V3).
const TICK_WINDOW: i32 = 50;

// ─── exchange ─────────────────────────────────────────────────────────────────

/// Discovers and refreshes Aerodrome Slipstream pools on one chain.
pub struct SlipstreamExchange {
    id: ExchangeId,
    chain: ChainId,
    factory: Address,
    /// Tick spacings to scan (Slipstream's discovery dimension, e.g. 1/50/100/200).
    tick_spacings: Vec<i32>,
}

impl SlipstreamExchange {
    pub fn new(id: &str, chain: ChainId, factory: Address, tick_spacings: Vec<i32>) -> Self {
        Self {
            id: ExchangeId::new(id),
            chain,
            factory,
            tick_spacings,
        }
    }

    /// One `getPool` per token pair × tick spacing, tracked in lockstep.
    fn get_pool_calls(
        &self,
        tokens: &[AssetId],
    ) -> Result<(Vec<Call>, Vec<PairSpacing>), ExchangeError> {
        let mut calls = Vec::new();
        let mut candidates = Vec::new();
        for i in 0..tokens.len() {
            for j in (i + 1)..tokens.len() {
                let (token0, token1) = ordered(&tokens[i], &tokens[j])?;
                let (addr0, addr1) = (asset_address(&token0)?, asset_address(&token1)?);
                for &spacing in &self.tick_spacings {
                    let calldata = ICLFactory::getPoolCall {
                        tokenA: addr0,
                        tokenB: addr1,
                        tickSpacing: I24::try_from(spacing).unwrap_or(I24::ZERO),
                    }
                    .abi_encode();
                    calls.push(call(self.factory, calldata));
                    candidates.push(PairSpacing {
                        token0: token0.clone(),
                        token1: token1.clone(),
                        spacing,
                    });
                }
            }
        }
        Ok((calls, candidates))
    }

    /// Keep the candidates whose `getPool` resolved to a real pool, carrying the
    /// tick spacing in `fee_bps` (refresh reads the actual swap fee on-chain).
    fn pool_keys(
        &self,
        results: &[CallResult],
        candidates: Vec<PairSpacing>,
    ) -> Result<Vec<PoolKey>, ExchangeError> {
        let mut keys = Vec::new();
        for (result, candidate) in results.iter().zip(candidates) {
            if !result.success {
                continue;
            }
            let pool = ICLFactory::getPoolCall::abi_decode_returns(&result.data.0)
                .map_err(|e| ExchangeError::Decode(format!("getPool: {e}")))?;
            if pool.is_zero() {
                continue;
            }
            keys.push(PoolKey {
                exchange: self.id.clone(),
                chain: self.chain.clone(),
                address: pool.to_string(),
                assets: vec![candidate.token0, candidate.token1],
                fee_bps: Some(candidate.spacing as u32), // carries tick spacing, not fee
            });
        }
        Ok(keys)
    }

    /// Three calls per pool — slot0, fee, liquidity — in that order.
    fn state_calls(&self, keys: &[PoolKey]) -> Result<Vec<Call>, ExchangeError> {
        let mut calls = Vec::with_capacity(keys.len() * 3);
        for key in keys {
            let addr = pool_address(key)?;
            calls.push(call(addr, ICLPool::slot0Call {}.abi_encode()));
            calls.push(call(addr, ICLPool::feeCall {}.abi_encode()));
            calls.push(call(addr, ICLPool::liquidityCall {}.abi_encode()));
        }
        Ok(calls)
    }

    /// For each decoded pool, one `ticks(t)` at every spacing in a
    /// [`TICK_WINDOW`]-wide band around the active tick.
    fn tick_window_calls(&self, states: &[PoolState]) -> (Vec<Call>, Vec<TickRef>) {
        let mut calls = Vec::new();
        let mut refs = Vec::new();
        for (state_idx, state) in states.iter().enumerate() {
            let center = (state.tick / state.tick_spacing) * state.tick_spacing;
            for step in -TICK_WINDOW..=TICK_WINDOW {
                let tick = center + step * state.tick_spacing;
                let calldata = ICLPool::ticksCall {
                    tick: I24::try_from(tick).unwrap_or(I24::ZERO),
                }
                .abi_encode();
                calls.push(call(state.address, calldata));
                refs.push(TickRef { state_idx, tick });
            }
        }
        (calls, refs)
    }
}

#[async_trait]
impl Exchange for SlipstreamExchange {
    fn id(&self) -> ExchangeId {
        self.id.clone()
    }

    fn supports(&self, chain: &ChainId) -> bool {
        chain == &self.chain
    }

    async fn discover(
        &self,
        chain: &ChainId,
        tokens: &[AssetId],
        reader: &dyn ChainReader,
    ) -> Result<Vec<PoolKey>, ExchangeError> {
        let (calls, candidates) = self.get_pool_calls(tokens)?;
        let out = reader
            .call_batch(chain, BlockId::Latest, calls)
            .await
            .map_err(read_err)?;
        self.pool_keys(&out.results, candidates)
    }

    async fn refresh(
        &self,
        keys: &[PoolKey],
        at: BlockId,
        reader: &dyn ChainReader,
    ) -> Result<Vec<Box<dyn Pool>>, ExchangeError> {
        // Round 1: slot0 + fee + liquidity for every pool, pinned to `at`.
        let round1 = reader
            .call_batch(&self.chain, at, self.state_calls(keys)?)
            .await
            .map_err(read_err)?;
        let states = decode_states(keys, &round1.results)?;

        // Round 2 (same block): a window of ticks around each active tick.
        let (tick_calls, refs) = self.tick_window_calls(&states);
        let round2 = reader
            .call_batch(&self.chain, BlockId::Number(round1.block), tick_calls)
            .await
            .map_err(read_err)?;
        let windows = tick_windows(&states, &round2.results, &refs);

        Ok(build_pools(keys, states, windows))
    }
}

// ─── refresh pipeline ─────────────────────────────────────────────────────────

/// A discovery candidate: the ordered token pair and its tick spacing.
struct PairSpacing {
    token0: AssetId,
    token1: AssetId,
    spacing: i32,
}

/// A pool's round-1 state, plus what round 2 and the build step need.
struct PoolState {
    key_idx: usize,
    address: Address,
    sqrt_price_x96: U256,
    tick: i32,
    liquidity: u128,
    fee: u32,
    tick_spacing: i32,
}

/// Which pool and tick a round-2 `ticks(t)` result belongs to.
struct TickRef {
    state_idx: usize,
    tick: i32,
}

/// One pool's initialized ticks and the bitmap that marks them.
#[derive(Default)]
struct TickWindow {
    ticks: HashMap<i32, TickInfo>,
    bitmap: HashMap<i16, U256>,
}

/// Decode each pool's slot0 + fee + liquidity triple (skipping any that
/// reverted). Results are the round-1 batch, three entries per key.
fn decode_states(
    keys: &[PoolKey],
    results: &[CallResult],
) -> Result<Vec<PoolState>, ExchangeError> {
    let mut states = Vec::new();
    for (key_idx, key) in keys.iter().enumerate() {
        let slot0 = &results[key_idx * 3];
        let fee = &results[key_idx * 3 + 1];
        let liquidity = &results[key_idx * 3 + 2];
        if !slot0.success || !fee.success || !liquidity.success {
            continue;
        }
        let (sqrt_price_x96, tick) = decode_slot0(&slot0.data.0)?;
        states.push(PoolState {
            key_idx,
            address: pool_address(key)?,
            sqrt_price_x96,
            tick,
            liquidity: decode_liquidity(&liquidity.data.0)?,
            fee: decode_fee(&fee.data.0)?,
            // The tick spacing is carried in the discovery key's `fee_bps`.
            tick_spacing: key.fee_bps.unwrap_or(1) as i32,
        });
    }
    Ok(states)
}

/// Fold round-2 results into one [`TickWindow`] per pool, keeping the
/// initialized ticks (`liquidityGross != 0`) and marking each in the bitmap.
fn tick_windows(states: &[PoolState], results: &[CallResult], refs: &[TickRef]) -> Vec<TickWindow> {
    let mut windows: Vec<TickWindow> = (0..states.len()).map(|_| TickWindow::default()).collect();
    for (result, tick_ref) in results.iter().zip(refs) {
        if !result.success {
            continue;
        }
        let Ok((gross, net)) = decode_tick(&result.data.0) else {
            continue;
        };
        if gross == 0 {
            continue;
        }
        let window = &mut windows[tick_ref.state_idx];
        window.ticks.insert(
            tick_ref.tick,
            TickInfo {
                liquidity_net: net,
                initialized: true,
            },
        );
        set_bitmap_bit(
            &mut window.bitmap,
            tick_ref.tick,
            states[tick_ref.state_idx].tick_spacing,
        );
    }
    windows
}

/// Assemble each [`PoolState`] + its [`TickWindow`] into a boxed `UniswapV3Pool`.
fn build_pools(
    keys: &[PoolKey],
    states: Vec<PoolState>,
    windows: Vec<TickWindow>,
) -> Vec<Box<dyn Pool>> {
    states
        .into_iter()
        .zip(windows)
        .map(|(state, window)| {
            let key = &keys[state.key_idx];
            Box::new(UniswapV3Pool::new(
                &key.address,
                key.assets[0].clone(),
                key.assets[1].clone(),
                state.sqrt_price_x96,
                state.liquidity,
                state.tick,
                window.ticks,
                window.bitmap,
                state.fee,
                state.tick_spacing,
                0,
                0,
            )) as Box<dyn Pool>
        })
        .collect()
}

/// Set the bitmap bit for an initialized tick, matching Uniswap's compressed
/// indexing (with the floor-division adjustment for negative ticks).
fn set_bitmap_bit(bitmap: &mut HashMap<i16, U256>, tick: i32, spacing: i32) {
    let compressed = match tick < 0 && tick % spacing != 0 {
        true => (tick / spacing) - 1,
        false => tick / spacing,
    };
    let word_pos = (compressed >> 8) as i16;
    let bit_pos = (compressed % 256) as u8;
    *bitmap.entry(word_pos).or_insert(U256::ZERO) |= U256::from(1u64) << bit_pos;
}

// ─── ABI decoders ─────────────────────────────────────────────────────────────

/// Decode a Slipstream `slot0()` return into `(sqrtPriceX96, tick)`.
fn decode_slot0(data: &[u8]) -> Result<(U256, i32), ExchangeError> {
    let s = ICLPool::slot0Call::abi_decode_returns(data)
        .map_err(|e| ExchangeError::Decode(format!("slot0: {e}")))?;
    Ok((U256::from(s.sqrtPriceX96), s.tick.as_i32()))
}

/// Decode a `fee()` return (uint24 pips = millionths, as V3's fee units).
fn decode_fee(data: &[u8]) -> Result<u32, ExchangeError> {
    let fee = ICLPool::feeCall::abi_decode_returns(data)
        .map_err(|e| ExchangeError::Decode(format!("fee: {e}")))?;
    Ok(fee.to::<u32>())
}

/// Decode a `liquidity()` return.
fn decode_liquidity(data: &[u8]) -> Result<u128, ExchangeError> {
    ICLPool::liquidityCall::abi_decode_returns(data)
        .map_err(|e| ExchangeError::Decode(format!("liquidity: {e}")))
}

/// Decode a Slipstream `ticks(tick)` return into `(liquidityGross, liquidityNet)`
/// — the first two fields, as in V3 (later staked/reward fields are ignored).
fn decode_tick(data: &[u8]) -> Result<(u128, i128), ExchangeError> {
    let t = ICLPool::ticksCall::abi_decode_returns(data)
        .map_err(|e| ExchangeError::Decode(format!("ticks: {e}")))?;
    Ok((t.liquidityGross, t.liquidityNet))
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::deps::chain_reader::ChainReadError;
    use crate::primitives::asset::{Amount, Pair};
    use crate::primitives::chain::{BatchOutput, Bytes};
    use alloy::primitives::address;
    use alloy::primitives::aliases::{I56, U24, U160};
    use rust_decimal::Decimal;

    const USDC: &str = "base:0x0000000000000000000000000000000000000001";
    const WETH: &str = "base:0x0000000000000000000000000000000000000002";
    const FACTORY: Address = address!("0x5e7BB104d84c7CB9B682AaC2F3d509f5F406809A");
    const POOL: Address = address!("0x00000000000000000000000000000000000000cc");
    const SPACING: i32 = 100;
    const LIQ: u128 = 1_000_000_000_000_000_000;

    fn asset(s: &str) -> AssetId {
        AssetId::new(s).unwrap()
    }
    fn tokens() -> Vec<AssetId> {
        vec![asset(USDC), asset(WETH)]
    }
    fn exchange() -> SlipstreamExchange {
        SlipstreamExchange::new("slipstream", ChainId::new("base"), FACTORY, vec![SPACING])
    }

    /// A Slipstream pool at tick 0 (1:1), active liquidity between ticks ±1000.
    struct FakeCL {
        block: u64,
    }

    impl FakeCL {
        fn answer(&self, c: &Call) -> CallResult {
            let data = &c.calldata.0;
            let ok = |b: Vec<u8>| CallResult {
                success: true,
                data: Bytes(b),
            };
            if data.starts_with(&ICLFactory::getPoolCall::SELECTOR) {
                ok(ICLFactory::getPoolCall::abi_encode_returns(&POOL))
            } else if data.starts_with(&ICLPool::slot0Call::SELECTOR) {
                ok(ICLPool::slot0Call::abi_encode_returns(
                    &ICLPool::slot0Return {
                        sqrtPriceX96: U160::from(79_228_162_514_264_337_593_543_950_336u128), // 2^96
                        tick: I24::ZERO,
                        observationIndex: 0,
                        observationCardinality: 0,
                        observationCardinalityNext: 0,
                        unlocked: true,
                    },
                ))
            } else if data.starts_with(&ICLPool::feeCall::SELECTOR) {
                ok(ICLPool::feeCall::abi_encode_returns(&U24::from(500u32)))
            } else if data.starts_with(&ICLPool::liquidityCall::SELECTOR) {
                ok(ICLPool::liquidityCall::abi_encode_returns(&LIQ))
            } else if data.starts_with(&ICLPool::ticksCall::SELECTOR) {
                let tick = ICLPool::ticksCall::abi_decode(data).unwrap().tick.as_i32();
                let net = match tick {
                    -1000 => LIQ as i128,
                    1000 => -(LIQ as i128),
                    _ => 0,
                };
                let gross = if net == 0 { 0 } else { LIQ };
                ok(ICLPool::ticksCall::abi_encode_returns(
                    &ICLPool::ticksReturn {
                        liquidityGross: gross,
                        liquidityNet: net,
                        stakedLiquidityNet: 0,
                        feeGrowthOutside0X128: U256::ZERO,
                        feeGrowthOutside1X128: U256::ZERO,
                        rewardGrowthOutsideX128: U256::ZERO,
                        tickCumulativeOutside: I56::ZERO,
                        secondsPerLiquidityOutsideX128: U160::ZERO,
                        secondsOutside: 0,
                        initialized: gross != 0,
                    },
                ))
            } else {
                CallResult {
                    success: false,
                    data: Bytes(Vec::new()),
                }
            }
        }
    }

    #[async_trait]
    impl ChainReader for FakeCL {
        async fn latest_block(&self, _chain: &ChainId) -> Result<u64, ChainReadError> {
            Ok(self.block)
        }
        async fn call_batch(
            &self,
            _chain: &ChainId,
            _at: BlockId,
            calls: Vec<Call>,
        ) -> Result<BatchOutput, ChainReadError> {
            Ok(BatchOutput {
                block: self.block,
                results: calls.iter().map(|c| self.answer(c)).collect(),
            })
        }
    }

    #[tokio::test]
    async fn discover_keys_carry_tick_spacing() {
        let ex = exchange();
        let chain = ChainId::new("base");
        let keys = ex
            .discover(&chain, &tokens(), &FakeCL { block: 7 })
            .await
            .unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].fee_bps, Some(SPACING as u32));
        assert_eq!(keys[0].address, POOL.to_string());
    }

    #[tokio::test]
    async fn refresh_reads_v3_state_and_quotes() {
        let ex = exchange();
        let chain = ChainId::new("base");
        let keys = ex
            .discover(&chain, &tokens(), &FakeCL { block: 7 })
            .await
            .unwrap();
        let pools = ex
            .refresh(&keys, BlockId::Number(7), &FakeCL { block: 7 })
            .await
            .unwrap();
        assert_eq!(pools.len(), 1);

        // Tiny USDC→WETH swap at 1:1: non-zero output, below input (fee).
        let pair = Pair {
            source: asset(USDC),
            destination: asset(WETH),
        };
        let out = pools[0]
            .quote(&pair, Amount(Decimal::from(1_000_000u64)))
            .expect("slipstream pool must quote after refresh");
        assert!(out.0 > Decimal::ZERO);
        assert!(out.0 < Decimal::from(1_000_000u64));
    }
}
