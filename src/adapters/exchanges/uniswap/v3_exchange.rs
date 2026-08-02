//! Uniswap V3 `Exchange`: turns on-chain state into quotable [`UniswapV3Pool`]s.
//!
//! Two responsibilities, both read-only:
//!
//! - **discover** — ask the V3 factory `getPool(a, b, fee)` for every tracked
//!   token pair × fee tier and keep the ones that exist, as [`PoolKey`]s.
//! - **refresh** — for a set of pools, read their state at a pinned block and
//!   decode it into `UniswapV3Pool`. A V3 pool's price depends not just on a
//!   spot value but on the *liquidity distribution across ticks*, so a refresh
//!   is two dependent rounds: first `slot0` (price + active tick) and
//!   `liquidity`, then — now that we know the active tick — a window of
//!   `ticks(t)` around it. Both rounds hit the same block, so the pool is a
//!   consistent snapshot.
//!
//! The Q64.96 swap math itself lives in [`super::v3`] and is already
//! golden-tested; this file only feeds it correct state.

use std::collections::HashMap;

use alloy::primitives::aliases::I24;
use alloy::primitives::{Address, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use async_trait::async_trait;

use super::ordered;
use super::v3::{TickInfo, UniswapV3Pool};
use crate::adapters::exchanges::{asset_address, call, pool_address, read_err};
use crate::core::deps::chain_reader::ChainReader;
use crate::core::deps::exchange::{Exchange, ExchangeError};
use crate::core::deps::pool::Pool;
use crate::primitives::asset::{AssetId, ChainId};
use crate::primitives::chain::{BlockId, Call, CallResult};
use crate::primitives::pool::{ExchangeId, PoolKey};

sol! {
    interface IUniswapV3Factory {
        function getPool(address tokenA, address tokenB, uint24 fee) external view returns (address pool);
    }

    interface IUniswapV3Pool {
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            uint8 feeProtocol,
            bool unlocked
        );
        function liquidity() external view returns (uint128);
        function ticks(int24 tick) external view returns (
            uint128 liquidityGross,
            int128 liquidityNet,
            uint256 feeGrowthOutside0X128,
            uint256 feeGrowthOutside1X128,
            int56 tickCumulativeOutside,
            uint160 secondsPerLiquidityOutsideX128,
            uint32 secondsOutside,
            bool initialized
        );
    }
}

/// Tick-spacings read on each side of the active tick during refresh. Wide
/// enough to cover the price movement of v1's modest quote sizes.
const TICK_WINDOW: i32 = 50;

/// Fallback fee tier when a `PoolKey` carries none (the 0.30% tier).
const DEFAULT_FEE: u32 = 3_000;

// ─── exchange ───────────────────────────────────────────────────────────────

/// Discovers and refreshes Uniswap V3 pools on one chain.
pub struct UniswapV3Exchange {
    id: ExchangeId,
    chain: ChainId,
    factory: Address,
    fee_tiers: Vec<u32>,
}

impl UniswapV3Exchange {
    pub fn new(id: &str, chain: ChainId, factory: Address, fee_tiers: Vec<u32>) -> Self {
        Self {
            id: ExchangeId::new(id),
            chain,
            factory,
            fee_tiers,
        }
    }

    /// Build one `getPool` call per unordered token pair × fee tier, tracking in
    /// lockstep which pair/tier each call is asking about.
    fn get_pool_calls(
        &self,
        tokens: &[AssetId],
    ) -> Result<(Vec<Call>, Vec<PairTier>), ExchangeError> {
        let mut calls = Vec::new();
        let mut candidates = Vec::new();
        for i in 0..tokens.len() {
            for j in (i + 1)..tokens.len() {
                let (token0, token1) = ordered(&tokens[i], &tokens[j])?;
                let (addr0, addr1) = (asset_address(&token0)?, asset_address(&token1)?);
                for &fee in &self.fee_tiers {
                    let calldata = IUniswapV3Factory::getPoolCall {
                        tokenA: addr0,
                        tokenB: addr1,
                        fee: fee.try_into().unwrap_or_default(),
                    }
                    .abi_encode();
                    calls.push(call(self.factory, calldata));
                    candidates.push(PairTier {
                        token0: token0.clone(),
                        token1: token1.clone(),
                        fee,
                    });
                }
            }
        }
        Ok((calls, candidates))
    }

    /// Keep the candidates whose `getPool` resolved to a real (non-zero) pool.
    fn pool_keys(
        &self,
        results: &[CallResult],
        candidates: Vec<PairTier>,
    ) -> Result<Vec<PoolKey>, ExchangeError> {
        let mut keys = Vec::new();
        for (result, candidate) in results.iter().zip(candidates) {
            if !result.success {
                continue;
            }
            let pool = IUniswapV3Factory::getPoolCall::abi_decode_returns(&result.data.0)
                .map_err(|e| ExchangeError::Decode(format!("getPool: {e}")))?;
            // The factory returns the zero address for a pair with no such pool.
            if pool.is_zero() {
                continue;
            }
            keys.push(PoolKey {
                exchange: self.id.clone(),
                chain: self.chain.clone(),
                address: pool.to_string(),
                assets: vec![candidate.token0, candidate.token1],
                fee_bps: Some(candidate.fee),
            });
        }
        Ok(keys)
    }
}

#[async_trait]
impl Exchange for UniswapV3Exchange {
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
        // Round 1: price, active tick, and liquidity for every pool, pinned to `at`.
        let round1 = reader
            .call_batch(&self.chain, at, slot0_liquidity_calls(keys)?)
            .await
            .map_err(read_err)?;
        let states = decode_states(keys, &round1.results)?;

        // Round 2 (same block): a window of ticks around each active tick, so the
        // swap math has the concentrated liquidity it might cross.
        let (tick_calls, refs) = tick_window_calls(&states);
        let round2 = reader
            .call_batch(&self.chain, BlockId::Number(round1.block), tick_calls)
            .await
            .map_err(read_err)?;
        let windows = tick_windows(&states, &round2.results, &refs);

        Ok(build_pools(keys, states, windows))
    }
}

// ─── refresh pipeline ───────────────────────────────────────────────────────

/// A pool's round-1 state, plus the parsed address and spacing round 2 needs.
struct PoolState {
    /// Index into the original `keys` slice.
    key_idx: usize,
    address: Address,
    sqrt_price_x96: U256,
    tick: i32,
    liquidity: u128,
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

/// A candidate pool from discovery: the ordered token pair and its fee tier.
struct PairTier {
    token0: AssetId,
    token1: AssetId,
    fee: u32,
}

/// Two calls per pool — `slot0` then `liquidity`, in that order.
fn slot0_liquidity_calls(keys: &[PoolKey]) -> Result<Vec<Call>, ExchangeError> {
    let mut calls = Vec::with_capacity(keys.len() * 2);
    for key in keys {
        let addr = pool_address(key)?;
        calls.push(call(addr, IUniswapV3Pool::slot0Call {}.abi_encode()));
        calls.push(call(addr, IUniswapV3Pool::liquidityCall {}.abi_encode()));
    }
    Ok(calls)
}

/// Decode each pool's `slot0`+`liquidity` pair (skipping any that reverted) into
/// a [`PoolState`]. Results are the round-1 batch, two entries per key.
fn decode_states(
    keys: &[PoolKey],
    results: &[CallResult],
) -> Result<Vec<PoolState>, ExchangeError> {
    let mut states = Vec::new();
    for (key_idx, key) in keys.iter().enumerate() {
        let slot0 = &results[key_idx * 2];
        let liquidity = &results[key_idx * 2 + 1];
        if !slot0.success || !liquidity.success {
            continue;
        }
        let (sqrt_price_x96, tick) = decode_slot0(&slot0.data.0)?;
        states.push(PoolState {
            key_idx,
            address: pool_address(key)?,
            sqrt_price_x96,
            tick,
            liquidity: decode_liquidity(&liquidity.data.0)?,
            tick_spacing: tick_spacing(key.fee_bps.unwrap_or(DEFAULT_FEE)),
        });
    }
    Ok(states)
}

/// For each pool, one `ticks(t)` call at every tick-spacing in a
/// [`TICK_WINDOW`]-wide band around its active tick. The returned [`TickRef`]s
/// run in lockstep with the calls, recording which pool/tick each fetches.
fn tick_window_calls(states: &[PoolState]) -> (Vec<Call>, Vec<TickRef>) {
    let mut calls = Vec::new();
    let mut refs = Vec::new();
    for (state_idx, state) in states.iter().enumerate() {
        // Snap to the nearest spacing at or below the active tick.
        let center = (state.tick / state.tick_spacing) * state.tick_spacing;
        for step in -TICK_WINDOW..=TICK_WINDOW {
            let tick = center + step * state.tick_spacing;
            let calldata = IUniswapV3Pool::ticksCall {
                tick: I24::try_from(tick).unwrap_or(I24::ZERO),
            }
            .abi_encode();
            calls.push(call(state.address, calldata));
            refs.push(TickRef { state_idx, tick });
        }
    }
    (calls, refs)
}

/// Fold the round-2 results into one [`TickWindow`] per pool, keeping only the
/// initialized ticks (`liquidityGross != 0`) and marking each in the bitmap.
/// A per-tick decode error or revert simply drops that tick.
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
            continue; // an uninitialized tick carries no liquidity
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
                key.fee_bps.unwrap_or(DEFAULT_FEE),
                state.tick_spacing,
                0,
                0,
            )) as Box<dyn Pool>
        })
        .collect()
}

// ─── uniswap V3 helpers ───────────────────────────────────────────────────────

/// The tick spacing for a Uniswap V3 fee tier (fee in millionths).
fn tick_spacing(fee: u32) -> i32 {
    match fee {
        100 => 1,
        500 => 10,
        3_000 => 60,
        10_000 => 200,
        _ => 60,
    }
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

// ─── ABI decoders ───────────────────────────────────────────────────────────

/// `int24` → `i32` (sign-preserving).
fn i24_to_i32(t: I24) -> i32 {
    t.as_i32()
}

/// Decode a `slot0()` return into `(sqrtPriceX96, tick)`.
fn decode_slot0(data: &[u8]) -> Result<(U256, i32), ExchangeError> {
    let s = IUniswapV3Pool::slot0Call::abi_decode_returns(data)
        .map_err(|e| ExchangeError::Decode(format!("slot0: {e}")))?;
    Ok((U256::from(s.sqrtPriceX96), i24_to_i32(s.tick)))
}

/// Decode a `liquidity()` return.
fn decode_liquidity(data: &[u8]) -> Result<u128, ExchangeError> {
    IUniswapV3Pool::liquidityCall::abi_decode_returns(data)
        .map_err(|e| ExchangeError::Decode(format!("liquidity: {e}")))
}

/// Decode a `ticks(tick)` return into `(liquidityGross, liquidityNet)`.
fn decode_tick(data: &[u8]) -> Result<(u128, i128), ExchangeError> {
    let t = IUniswapV3Pool::ticksCall::abi_decode_returns(data)
        .map_err(|e| ExchangeError::Decode(format!("ticks: {e}")))?;
    Ok((t.liquidityGross, t.liquidityNet))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::deps::chain_reader::ChainReadError;
    use crate::primitives::chain::{BatchOutput, Bytes};
    use alloy::primitives::address;
    use alloy::primitives::aliases::{I56, U160};

    fn enc_slot0(sqrt: u128, tick: i32) -> Vec<u8> {
        IUniswapV3Pool::slot0Call::abi_encode_returns(&IUniswapV3Pool::slot0Return {
            sqrtPriceX96: U160::from(sqrt),
            tick: I24::try_from(tick).unwrap(),
            observationIndex: 0,
            observationCardinality: 0,
            observationCardinalityNext: 0,
            feeProtocol: 0,
            unlocked: true,
        })
    }

    fn enc_liquidity(l: u128) -> Vec<u8> {
        IUniswapV3Pool::liquidityCall::abi_encode_returns(&l)
    }

    fn enc_tick(gross: u128, net: i128) -> Vec<u8> {
        IUniswapV3Pool::ticksCall::abi_encode_returns(&IUniswapV3Pool::ticksReturn {
            liquidityGross: gross,
            liquidityNet: net,
            feeGrowthOutside0X128: U256::ZERO,
            feeGrowthOutside1X128: U256::ZERO,
            tickCumulativeOutside: I56::ZERO,
            secondsPerLiquidityOutsideX128: U160::ZERO,
            secondsOutside: 0,
            initialized: true,
        })
    }

    #[test]
    fn tick_spacing_matches_fee_tiers() {
        assert_eq!(tick_spacing(500), 10);
        assert_eq!(tick_spacing(3_000), 60);
        assert_eq!(tick_spacing(10_000), 200);
    }

    #[test]
    fn asset_address_parses_chain_qualified_hex() {
        let ok = AssetId::new("ethereum:0x0000000000000000000000000000000000000001").unwrap();
        assert_eq!(
            asset_address(&ok).unwrap(),
            address!("0x0000000000000000000000000000000000000001")
        );
        // A symbolic (non-address) token part is rejected.
        assert!(asset_address(&AssetId::new("ethereum:usdc").unwrap()).is_err());
    }

    #[test]
    fn decodes_slot0_liquidity_and_ticks() {
        let sqrt = 79_228_162_514_264_337_593_543_950_336u128; // 2^96 (price 1)
        let (s, tick) = decode_slot0(&enc_slot0(sqrt, -60)).unwrap();
        assert_eq!(s, U256::from(sqrt));
        assert_eq!(tick, -60);
        assert_eq!(
            decode_liquidity(&enc_liquidity(1_000_000)).unwrap(),
            1_000_000
        );
        assert_eq!(
            decode_tick(&enc_tick(500, -250)).unwrap(),
            (500u128, -250i128)
        );
    }

    /// Returns the same pool address for every `getPool` call.
    struct FakeFactory {
        block: u64,
        pool: Address,
    }

    #[async_trait]
    impl ChainReader for FakeFactory {
        async fn latest_block(&self, _chain: &ChainId) -> Result<u64, ChainReadError> {
            Ok(self.block)
        }
        async fn call_batch(
            &self,
            _chain: &ChainId,
            _at: BlockId,
            calls: Vec<Call>,
        ) -> Result<BatchOutput, ChainReadError> {
            let results = calls
                .iter()
                .map(|c| {
                    match c
                        .calldata
                        .0
                        .starts_with(&IUniswapV3Factory::getPoolCall::SELECTOR)
                    {
                        true => CallResult {
                            success: true,
                            data: Bytes(IUniswapV3Factory::getPoolCall::abi_encode_returns(
                                &self.pool,
                            )),
                        },
                        false => CallResult {
                            success: false,
                            data: Bytes(Vec::new()),
                        },
                    }
                })
                .collect();
            Ok(BatchOutput {
                block: self.block,
                results,
            })
        }
    }

    #[tokio::test]
    async fn discover_builds_pool_keys_per_pair_and_tier() {
        let chain = ChainId::new("ethereum");
        let factory = address!("0x1F98431c8aD98523631AE4a59f267346ea31F984");
        let ex = UniswapV3Exchange::new("univ3", chain.clone(), factory, vec![500, 3_000]);
        let tokens = vec![
            AssetId::new("ethereum:0x0000000000000000000000000000000000000001").unwrap(),
            AssetId::new("ethereum:0x0000000000000000000000000000000000000002").unwrap(),
        ];
        let reader = FakeFactory {
            block: 100,
            pool: address!("0x00000000000000000000000000000000000000aa"),
        };

        let keys = ex.discover(&chain, &tokens, &reader).await.unwrap();
        // One token pair × two fee tiers → two pool keys.
        assert_eq!(keys.len(), 2);
        let fees: Vec<u32> = keys.iter().filter_map(|k| k.fee_bps).collect();
        assert!(fees.contains(&500) && fees.contains(&3_000));
        assert!(keys.iter().all(|k| k.assets.len() == 2));
    }
}
