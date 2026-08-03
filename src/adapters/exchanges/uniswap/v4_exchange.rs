//! Uniswap V4 `Exchange`: turns singleton-`PoolManager` storage into quotable
//! [`UniswapV4Pool`]s.
//!
//! V4 collapses every pool into one `PoolManager` contract. A pool is no longer
//! its own contract; it is identified by a 32-byte
//! `pool_id = keccak256(abi.encode(poolKey))`. There is no factory to enumerate
//! pools, so discovery is **config-driven** (like Curve): each pool's
//! currencies, fee, tick spacing and hooks are configured, and we derive its
//! `pool_id` up front.
//!
//! State is read with `extsload(bytes32)` — raw storage-slot reads against the
//! `PoolManager`. The slot layout mirrors uniswap/v4-core's `StateLibrary`:
//!
//! | value                | slot                                            |
//! |----------------------|-------------------------------------------------|
//! | pool state base      | `keccak256(pool_id ‖ POOLS_SLOT)` (POOLS_SLOT=6) |
//! | slot0 (packed)       | `base` → sqrtPriceX96 (bits 0..160), tick (160..184) |
//! | liquidity (uint128)  | `base + 3`                                       |
//! | ticks[tick]          | `keccak256(int256(tick) ‖ (base + 4))` → gross/net |
//!
//! The `int256`-padded mapping keys and 160-byte `abi.encode` preimage let us
//! reproduce every slot exactly with alloy primitives. Refresh is the same two
//! dependent rounds as V3 — slot0+liquidity, then a tick window around the
//! active tick — because the swap math (identical to V3, in [`super::v4`]) needs
//! the concentrated liquidity it might cross. This adapter only feeds it state.

use std::collections::HashMap;

use alloy::primitives::aliases::{I24, U24};
use alloy::primitives::{Address, B256, I256, U256, keccak256};
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use async_trait::async_trait;

use super::v3::TickInfo;
use super::v4::UniswapV4Pool;
use crate::adapters::exchanges::{call, read_err};
use crate::core::deps::chain_reader::ChainReader;
use crate::core::deps::exchange::{Exchange, ExchangeError};
use crate::core::deps::pool::Pool;
use crate::primitives::asset::{AssetId, ChainId};
use crate::primitives::chain::{BlockId, Call};
use crate::primitives::pool::{ExchangeId, PoolKey};

sol! {
    /// The five fields whose `abi.encode` hashes to a V4 `pool_id`.
    struct V4PoolKey {
        address currency0;
        address currency1;
        uint24 fee;
        int24 tickSpacing;
        address hooks;
    }

    /// `PoolManager` raw storage reads (single-slot variant).
    interface IExtsload {
        function extsload(bytes32 slot) external view returns (bytes32);
    }
}

// ─── StateLibrary storage-layout constants (uniswap/v4-core) ──────────────────

/// Base slot of the `mapping(PoolId => Pool.State) _pools`.
const POOLS_SLOT: u64 = 6;
/// `Pool.State.liquidity` offset within a pool's state struct.
const LIQUIDITY_OFFSET: u64 = 3;
/// `Pool.State.ticks` mapping offset within a pool's state struct.
const TICKS_OFFSET: u64 = 4;

/// Tick-spacings read on each side of the active tick during refresh. Matches
/// the V3 adapter — wide enough for v1's modest quote sizes.
const TICK_WINDOW: i32 = 50;

// ─── config ───────────────────────────────────────────────────────────────────

/// A configured V4 pool: its derived id, sorted currencies + decimals, and the
/// fee / tick spacing the swap math needs. Decimals are informational (the
/// integer swap math does not use them) but are carried for parity with V3.
#[derive(Clone)]
pub struct V4PoolConfig {
    pub pool_id: [u8; 32],
    pub token0: AssetId,
    pub token1: AssetId,
    pub fee: u32,
    pub tick_spacing: i32,
    pub decimals0: u8,
    pub decimals1: u8,
}

impl V4PoolConfig {
    /// Build a config from currency addresses (already sorted `currency0 <
    /// currency1`) and their asset ids, deriving the `pool_id`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        currency0: Address,
        currency1: Address,
        token0: AssetId,
        token1: AssetId,
        fee: u32,
        tick_spacing: i32,
        hooks: Address,
        decimals0: u8,
        decimals1: u8,
    ) -> Self {
        Self {
            pool_id: derive_pool_id(currency0, currency1, fee, tick_spacing, hooks),
            token0,
            token1,
            fee,
            tick_spacing,
            decimals0,
            decimals1,
        }
    }
}

// ─── exchange ─────────────────────────────────────────────────────────────────

/// Refreshes a configured set of Uniswap V4 pools living in one `PoolManager`.
pub struct UniswapV4Exchange {
    id: ExchangeId,
    chain: ChainId,
    /// The singleton `PoolManager` every pool's state is read from.
    pool_manager: Address,
    pools: Vec<V4PoolConfig>,
}

impl UniswapV4Exchange {
    pub fn new(id: &str, chain: ChainId, pool_manager: Address, pools: Vec<V4PoolConfig>) -> Self {
        Self {
            id: ExchangeId::new(id),
            chain,
            pool_manager,
            pools,
        }
    }

    /// Find the config a `PoolKey` refers to by its stored `pool_id` hex.
    fn config_for(&self, key: &PoolKey) -> Option<&V4PoolConfig> {
        let want = key.address.parse::<B256>().ok()?;
        self.pools.iter().find(|p| B256::from(p.pool_id) == want)
    }
}

#[async_trait]
impl Exchange for UniswapV4Exchange {
    fn id(&self) -> ExchangeId {
        self.id.clone()
    }

    fn supports(&self, chain: &ChainId) -> bool {
        chain == &self.chain
    }

    async fn discover(
        &self,
        _chain: &ChainId,
        _tokens: &[AssetId],
        _reader: &dyn ChainReader,
    ) -> Result<Vec<PoolKey>, ExchangeError> {
        // Config-driven: each configured pool is a key, addressed by its pool id.
        Ok(self
            .pools
            .iter()
            .map(|pool| PoolKey {
                exchange: self.id.clone(),
                chain: self.chain.clone(),
                address: B256::from(pool.pool_id).to_string(),
                assets: vec![pool.token0.clone(), pool.token1.clone()],
                fee_bps: Some(pool.fee),
            })
            .collect())
    }

    async fn refresh(
        &self,
        keys: &[PoolKey],
        at: BlockId,
        reader: &dyn ChainReader,
    ) -> Result<Vec<Box<dyn Pool>>, ExchangeError> {
        // Round 1: slot0 + liquidity for every pool, all read from the manager.
        let (round1_calls, plans) = self.slot0_liquidity_calls(keys);
        let round1 = reader
            .call_batch(&self.chain, at, round1_calls)
            .await
            .map_err(read_err)?;
        let states = decode_states(&plans, &round1.results)?;

        // Round 2 (same block): a tick window around each active tick.
        let (tick_calls, refs) = self.tick_window_calls(&plans, &states);
        let round2 = reader
            .call_batch(&self.chain, BlockId::Number(round1.block), tick_calls)
            .await
            .map_err(read_err)?;
        let windows = tick_windows(&plans, &states, &round2.results, &refs);

        Ok(build_pools(&plans, states, windows))
    }
}

impl UniswapV4Exchange {
    /// Two `extsload`s per resolvable pool — slot0 then liquidity — against the
    /// manager, plus the [`PoolPlan`]s that record which config each pair is for.
    fn slot0_liquidity_calls<'a>(&'a self, keys: &'a [PoolKey]) -> (Vec<Call>, Vec<PoolPlan<'a>>) {
        let mut calls = Vec::new();
        let mut plans = Vec::new();
        for key in keys {
            let Some(config) = self.config_for(key) else {
                continue;
            };
            let state_slot = pool_state_slot(config.pool_id);
            calls.push(extsload_call(self.pool_manager, state_slot));
            calls.push(extsload_call(
                self.pool_manager,
                offset_slot(state_slot, LIQUIDITY_OFFSET),
            ));
            plans.push(PoolPlan {
                key,
                config,
                state_slot,
            });
        }
        (calls, plans)
    }

    /// For every decoded pool, one `extsload(ticks[t])` at each tick-spacing in a
    /// [`TICK_WINDOW`]-wide band around its active tick. Returned [`TickRef`]s run
    /// in lockstep with the calls.
    fn tick_window_calls(
        &self,
        plans: &[PoolPlan],
        states: &[PoolState],
    ) -> (Vec<Call>, Vec<TickRef>) {
        let mut calls = Vec::new();
        let mut refs = Vec::new();
        for (state_idx, state) in states.iter().enumerate() {
            let plan = &plans[state.plan_idx];
            let spacing = plan.config.tick_spacing;
            // Snap to the nearest spacing at or below the active tick.
            let center = (state.tick / spacing) * spacing;
            for step in -TICK_WINDOW..=TICK_WINDOW {
                let tick = center + step * spacing;
                calls.push(extsload_call(
                    self.pool_manager,
                    tick_info_slot(plan.state_slot, tick),
                ));
                refs.push(TickRef { state_idx, tick });
            }
        }
        (calls, refs)
    }
}

// ─── refresh pipeline ─────────────────────────────────────────────────────────

/// A resolvable pool's key, config, and precomputed state base slot.
struct PoolPlan<'a> {
    key: &'a PoolKey,
    config: &'a V4PoolConfig,
    state_slot: B256,
}

/// A pool's round-1 state, plus the plan index round 2 needs.
struct PoolState {
    /// Index into the `plans` slice.
    plan_idx: usize,
    sqrt_price_x96: U256,
    tick: i32,
    liquidity: u128,
    /// Current LP fee (pips), read from slot0 — for dynamic-fee pools this is
    /// the live value, not the static one in the pool key.
    lp_fee: u32,
    /// Packed per-direction protocol fee (pips): `zeroForOne` in the low 12
    /// bits, `oneForZero` in the high 12.
    protocol_fee: u32,
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

/// Decode each pool's `slot0`+`liquidity` word pair (skipping any that reverted).
/// Results are the round-1 batch, two entries per plan in order.
fn decode_states(
    plans: &[PoolPlan],
    results: &[crate::primitives::chain::CallResult],
) -> Result<Vec<PoolState>, ExchangeError> {
    let mut states = Vec::new();
    for (plan_idx, _plan) in plans.iter().enumerate() {
        let slot0 = &results[plan_idx * 2];
        let liquidity = &results[plan_idx * 2 + 1];
        if !slot0.success || !liquidity.success {
            continue;
        }
        let (sqrt_price_x96, tick, lp_fee, protocol_fee) =
            decode_slot0(decode_word(&slot0.data.0)?);
        states.push(PoolState {
            plan_idx,
            sqrt_price_x96,
            tick,
            liquidity: decode_liquidity(decode_word(&liquidity.data.0)?),
            lp_fee,
            protocol_fee,
        });
    }
    Ok(states)
}

/// Assemble each [`PoolState`] + its [`TickWindow`] into a boxed `UniswapV4Pool`.
fn build_pools(
    plans: &[PoolPlan],
    states: Vec<PoolState>,
    windows: Vec<TickWindow>,
) -> Vec<Box<dyn Pool>> {
    states
        .into_iter()
        .zip(windows)
        .map(|(state, window)| {
            let plan = &plans[state.plan_idx];
            let config = plan.config;
            // Effective fee = live LP fee compounded with the per-direction V4
            // protocol fee (low 12 bits = zeroForOne, high 12 = oneForZero).
            let fee_zero_for_one = combined_fee(state.lp_fee, state.protocol_fee & 0xfff);
            let fee_one_for_zero = combined_fee(state.lp_fee, state.protocol_fee >> 12);
            Box::new(UniswapV4Pool::new(
                &plan.key.address,
                config.pool_id,
                config.token0.clone(),
                config.token1.clone(),
                state.sqrt_price_x96,
                state.liquidity,
                state.tick,
                window.ticks,
                window.bitmap,
                fee_zero_for_one,
                fee_one_for_zero,
                config.tick_spacing,
                config.decimals0,
                config.decimals1,
            )) as Box<dyn Pool>
        })
        .collect()
}

/// V4's effective swap fee: the protocol fee is taken first, then the LP fee on
/// the remainder — `protocol + lp·(1e6 − protocol)/1e6` (pips), matching
/// v4-core's `ProtocolFeeLibrary.calculateSwapFee`.
fn combined_fee(lp_fee: u32, protocol_fee: u32) -> u32 {
    let (lp, pf) = (lp_fee as u64, protocol_fee as u64);
    (pf + lp * (1_000_000 - pf) / 1_000_000) as u32
}

/// Fold round-2 results into one [`TickWindow`] per pool, keeping the
/// initialized ticks (`liquidityGross != 0`) and marking each in the bitmap.
fn tick_windows(
    plans: &[PoolPlan],
    states: &[PoolState],
    results: &[crate::primitives::chain::CallResult],
    refs: &[TickRef],
) -> Vec<TickWindow> {
    let mut windows: Vec<TickWindow> = (0..states.len()).map(|_| TickWindow::default()).collect();
    for (result, tick_ref) in results.iter().zip(refs) {
        if !result.success {
            continue;
        }
        let Ok(word) = decode_word(&result.data.0) else {
            continue;
        };
        let (gross, net) = decode_tick(word);
        if gross == 0 {
            continue; // an uninitialized tick carries no liquidity
        }
        let spacing = plans[states[tick_ref.state_idx].plan_idx]
            .config
            .tick_spacing;
        let window = &mut windows[tick_ref.state_idx];
        window.ticks.insert(
            tick_ref.tick,
            TickInfo {
                liquidity_net: net,
                initialized: true,
            },
        );
        set_bitmap_bit(&mut window.bitmap, tick_ref.tick, spacing);
    }
    windows
}

// ─── slot derivation ──────────────────────────────────────────────────────────

/// `pool_id = keccak256(abi.encode(currency0, currency1, fee, tickSpacing, hooks))`.
fn derive_pool_id(
    currency0: Address,
    currency1: Address,
    fee: u32,
    tick_spacing: i32,
    hooks: Address,
) -> [u8; 32] {
    let key = V4PoolKey {
        currency0,
        currency1,
        fee: U24::from(fee),
        tickSpacing: I24::try_from(tick_spacing).unwrap_or(I24::ZERO),
        hooks,
    };
    keccak256(key.abi_encode()).0
}

/// Base storage slot of a pool's `Pool.State`: `keccak256(pool_id ‖ POOLS_SLOT)`.
fn pool_state_slot(pool_id: [u8; 32]) -> B256 {
    let mut preimage = [0u8; 64];
    preimage[..32].copy_from_slice(&pool_id);
    preimage[32..].copy_from_slice(&U256::from(POOLS_SLOT).to_be_bytes::<32>());
    keccak256(preimage)
}

/// A fixed offset added to a state base slot (for the packed scalar fields).
fn offset_slot(state_slot: B256, offset: u64) -> B256 {
    B256::from((U256::from_be_bytes(state_slot.0) + U256::from(offset)).to_be_bytes::<32>())
}

/// Storage slot of `ticks[tick]`: `keccak256(int256(tick) ‖ (base + TICKS_OFFSET))`.
/// The mapping key is the full sign-extended `int256`, as `StateLibrary` uses.
fn tick_info_slot(state_slot: B256, tick: i32) -> B256 {
    let ticks_map = U256::from_be_bytes(state_slot.0) + U256::from(TICKS_OFFSET);
    let mut preimage = [0u8; 64];
    preimage[..32].copy_from_slice(
        &I256::try_from(tick)
            .unwrap_or(I256::ZERO)
            .to_be_bytes::<32>(),
    );
    preimage[32..].copy_from_slice(&ticks_map.to_be_bytes::<32>());
    keccak256(preimage)
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

// ─── ABI / word decoders ──────────────────────────────────────────────────────

/// An `extsload(slot)` call against `target`.
fn extsload_call(target: Address, slot: B256) -> Call {
    call(target, IExtsload::extsloadCall { slot }.abi_encode())
}

/// Decode an `extsload` return (a single `bytes32`) into a word.
fn decode_word(data: &[u8]) -> Result<B256, ExchangeError> {
    IExtsload::extsloadCall::abi_decode_returns(data)
        .map_err(|e| ExchangeError::Decode(format!("extsload: {e}")))
}

/// Decode a packed `slot0` word into `(sqrtPriceX96, tick, lpFee, protocolFee)`.
///
/// Layout: `sqrtPriceX96` bits `0..160`, `tick` (int24) `160..184`,
/// `protocolFee` (uint24) `184..208`, `lpFee` (uint24) `208..232`.
fn decode_slot0(word: B256) -> (U256, i32, u32, u32) {
    let value = U256::from_be_bytes(word.0);
    let mask160 = (U256::from(1u8) << 160) - U256::from(1u8);
    let sqrt_price_x96 = value & mask160;
    let raw = ((value >> 160usize) & U256::from(0xFF_FFFFu32)).to::<u32>();
    // Sign-extend the 24-bit tick.
    let tick = match raw & 0x80_0000 != 0 {
        true => raw as i32 - (1 << 24),
        false => raw as i32,
    };
    let protocol_fee = ((value >> 184usize) & U256::from(0xFF_FFFFu32)).to::<u32>();
    let lp_fee = ((value >> 208usize) & U256::from(0xFF_FFFFu32)).to::<u32>();
    (sqrt_price_x96, tick, lp_fee, protocol_fee)
}

/// Decode a `liquidity` word — the pool's active liquidity in the low 128 bits.
fn decode_liquidity(word: B256) -> u128 {
    let value = U256::from_be_bytes(word.0);
    (value & low128()).to::<u128>()
}

/// Decode a `ticks[t]` word into `(liquidityGross, liquidityNet)` — gross in the
/// low 128 bits, net (signed) in the high 128.
fn decode_tick(word: B256) -> (u128, i128) {
    let value = U256::from_be_bytes(word.0);
    let gross = (value & low128()).to::<u128>();
    let net = (value >> 128usize).to::<u128>() as i128; // reinterpret bits as two's-complement
    (gross, net)
}

/// The `2^128 - 1` low-128-bit mask.
fn low128() -> U256 {
    (U256::from(1u8) << 128) - U256::from(1u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::deps::chain_reader::ChainReadError;
    use crate::primitives::asset::{Amount, Pair};
    use crate::primitives::chain::{BatchOutput, Bytes, CallResult};
    use alloy::primitives::{Address, address};
    use rust_decimal::Decimal;

    fn asset(s: &str) -> AssetId {
        AssetId::new(s).unwrap()
    }
    fn usdc() -> AssetId {
        asset("ethereum:usdc")
    }
    fn weth() -> AssetId {
        asset("ethereum:weth")
    }

    const C0: Address = address!("0x0000000000000000000000000000000000000001");
    const C1: Address = address!("0x0000000000000000000000000000000000000002");
    const MANAGER: Address = address!("0x000000000004444c5dc75cB358380D2e3dE08A90");

    fn config() -> V4PoolConfig {
        V4PoolConfig::new(C0, C1, usdc(), weth(), 3000, 60, Address::ZERO, 6, 18)
    }

    // ── encoding / derivation golden vectors ──────────────────────────────────

    /// The `PoolKey` abi-encoding is exactly five 32-byte words (0xa0 bytes),
    /// matching v4-core's `keccak256(poolKey, 0xa0)`.
    #[test]
    fn pool_key_encoding_is_160_bytes() {
        let key = V4PoolKey {
            currency0: C0,
            currency1: C1,
            fee: U24::from(3000u32),
            tickSpacing: I24::try_from(60).unwrap(),
            hooks: Address::ZERO,
        };
        assert_eq!(key.abi_encode().len(), 160);
    }

    /// `derive_pool_id` reproduces the spec preimage: the five fields each padded
    /// to a 32-byte word (address right-aligned, fee zero-padded, tickSpacing
    /// sign-extended), hashed with keccak256.
    #[test]
    fn derive_pool_id_matches_manual_preimage() {
        let mut preimage = Vec::new();
        preimage.extend_from_slice(&U256::from_be_slice(C0.as_slice()).to_be_bytes::<32>());
        preimage.extend_from_slice(&U256::from_be_slice(C1.as_slice()).to_be_bytes::<32>());
        preimage.extend_from_slice(&U256::from(3000u32).to_be_bytes::<32>());
        preimage.extend_from_slice(&I256::try_from(60).unwrap().to_be_bytes::<32>());
        preimage.extend_from_slice(&[0u8; 32]); // hooks = zero address
        assert_eq!(preimage.len(), 160);

        let expected = keccak256(&preimage).0;
        assert_eq!(derive_pool_id(C0, C1, 3000, 60, Address::ZERO), expected);
    }

    /// Golden vector: the canonical mainnet ETH/USDC 0.05% pool — native ETH as
    /// `currency0 = address(0)`, USDC as `currency1`, fee 500, tick spacing 10,
    /// no hooks — hashes to its published `pool_id`. This anchors the whole
    /// derivation (abi.encode shape + keccak) against a real on-chain value.
    #[test]
    fn derive_pool_id_matches_mainnet_eth_usdc() {
        let usdc_addr = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        let got = derive_pool_id(Address::ZERO, usdc_addr, 500, 10, Address::ZERO);
        let expected: [u8; 32] =
            "0x21c67e77068de97969ba93d4aab21826d33ca12bb9f565d8496e8fda8a82ca27"
                .parse::<B256>()
                .unwrap()
                .0;
        assert_eq!(got, expected);
    }

    /// A negative `tickSpacing` must sign-extend (differ from its positive twin).
    #[test]
    fn derive_pool_id_distinguishes_sign() {
        let pos = derive_pool_id(C0, C1, 3000, 60, Address::ZERO);
        let neg = derive_pool_id(C0, C1, 3000, -60, Address::ZERO);
        assert_ne!(pos, neg);
    }

    // ── word decoders ─────────────────────────────────────────────────────────

    fn slot0_word(sqrt: U256, tick: i32) -> B256 {
        let tick_bits = U256::from((tick as i64 & 0xFF_FFFF) as u64) << 160usize;
        // lpFee = 3000 (0.30%), no protocol fee — bits 208..232.
        let lp_fee_bits = U256::from(3000u32) << 208usize;
        B256::from((sqrt | tick_bits | lp_fee_bits).to_be_bytes::<32>())
    }

    #[test]
    fn decode_slot0_extracts_sqrt_tick_and_fee() {
        let sqrt = U256::from(1u8) << 96usize; // 2^96 → price 1
        let (got_sqrt, got_tick, lp_fee, protocol_fee) = decode_slot0(slot0_word(sqrt, 0));
        assert_eq!(got_sqrt, sqrt);
        assert_eq!(got_tick, 0);
        assert_eq!(lp_fee, 3000);
        assert_eq!(protocol_fee, 0);
    }

    #[test]
    fn decode_slot0_sign_extends_negative_tick() {
        let sqrt = U256::from(1u8) << 96usize;
        let (got_sqrt, got_tick, _, _) = decode_slot0(slot0_word(sqrt, -60));
        assert_eq!(got_sqrt, sqrt);
        assert_eq!(got_tick, -60);
    }

    /// The packed protocol fee splits into per-direction 12-bit halves, and the
    /// combined fee compounds LP + protocol as v4-core does.
    #[test]
    fn combined_fee_compounds_lp_and_protocol() {
        // lpFee 500 + protocolFee 125 (both directions) → ~624 pips, per v4-core.
        assert_eq!(combined_fee(500, 125), 624);
        // No protocol fee → just the LP fee.
        assert_eq!(combined_fee(3000, 0), 3000);
    }

    #[test]
    fn decode_liquidity_reads_low_128() {
        let liq = 1_234_567_890_123_456_789u128;
        let word = B256::from(U256::from(liq).to_be_bytes::<32>());
        assert_eq!(decode_liquidity(word), liq);
    }

    #[test]
    fn decode_tick_splits_gross_and_signed_net() {
        let gross = 5_000u128;
        let net = -250i128;
        let word = tick_info_word(gross, net);
        assert_eq!(decode_tick(word), (gross, net));
    }

    // ── refresh → quote through a fake PoolManager ────────────────────────────

    /// Encode a `ticks[t]` storage word: gross in low 128, signed net in high 128.
    fn tick_info_word(gross: u128, net: i128) -> B256 {
        let net_u = net as u128; // two's-complement reinterpret
        let value = U256::from(gross) | (U256::from(net_u) << 128usize);
        B256::from(value.to_be_bytes::<32>())
    }

    fn enc_word(word: B256) -> Vec<u8> {
        IExtsload::extsloadCall::abi_encode_returns(&word)
    }

    /// A single V4 pool at tick 0 (1:1), full active liquidity between ticks
    /// ±600 (within the refresh window), answering `extsload` by recomputing the
    /// pool's storage slots — so this also exercises the derivation end-to-end.
    struct FakeManager {
        block: u64,
        pool_id: [u8; 32],
        liq: u128,
    }

    #[async_trait]
    impl ChainReader for FakeManager {
        async fn latest_block(&self, _chain: &ChainId) -> Result<u64, ChainReadError> {
            Ok(self.block)
        }
        async fn call_batch(
            &self,
            _chain: &ChainId,
            _at: BlockId,
            calls: Vec<Call>,
        ) -> Result<BatchOutput, ChainReadError> {
            let state = pool_state_slot(self.pool_id);
            let liq_slot = offset_slot(state, LIQUIDITY_OFFSET);
            let lower = tick_info_slot(state, -600);
            let upper = tick_info_slot(state, 600);

            let results = calls
                .iter()
                .map(|c| {
                    let slot = IExtsload::extsloadCall::abi_decode(&c.calldata.0)
                        .unwrap()
                        .slot;
                    let word = if slot == state {
                        slot0_word(U256::from(1u8) << 96usize, 0)
                    } else if slot == liq_slot {
                        B256::from(U256::from(self.liq).to_be_bytes::<32>())
                    } else if slot == lower {
                        tick_info_word(self.liq, self.liq as i128)
                    } else if slot == upper {
                        tick_info_word(self.liq, -(self.liq as i128))
                    } else {
                        B256::ZERO // uninitialized tick
                    };
                    CallResult {
                        success: true,
                        data: Bytes(enc_word(word)),
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
    async fn discover_emits_one_key_per_configured_pool() {
        let chain = ChainId::new("ethereum");
        let ex = UniswapV4Exchange::new("uniswap_v4", chain.clone(), MANAGER, vec![config()]);
        let keys = ex
            .discover(
                &chain,
                &[],
                &FakeManager {
                    block: 1,
                    pool_id: config().pool_id,
                    liq: 0,
                },
            )
            .await
            .unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].assets, vec![usdc(), weth()]);
        assert_eq!(keys[0].fee_bps, Some(3000));
        // The key is addressed by the pool id.
        assert_eq!(
            keys[0].address.parse::<B256>().unwrap(),
            B256::from(config().pool_id)
        );
    }

    #[tokio::test]
    async fn refresh_reads_manager_storage_and_quotes_near_parity() {
        let chain = ChainId::new("ethereum");
        let cfg = config();
        let ex = UniswapV4Exchange::new("uniswap_v4", chain.clone(), MANAGER, vec![cfg.clone()]);

        let keys = ex
            .discover(
                &chain,
                &[],
                &FakeManager {
                    block: 1,
                    pool_id: cfg.pool_id,
                    liq: 0,
                },
            )
            .await
            .unwrap();

        let reader = FakeManager {
            block: 100,
            pool_id: cfg.pool_id,
            liq: 1_000_000_000_000_000_000,
        };
        let pools = ex
            .refresh(&keys, BlockId::Number(100), &reader)
            .await
            .unwrap();
        assert_eq!(pools.len(), 1);

        // Tiny USDC→WETH swap at 1:1 price: output is non-zero and below input (fee).
        let pair = Pair {
            source: usdc(),
            destination: weth(),
        };
        let out = pools[0]
            .quote(&pair, Amount(Decimal::from(1_000_000u64)))
            .expect("v4 pool must quote after refresh");
        assert!(out.0 > Decimal::ZERO);
        assert!(out.0 < Decimal::from(1_000_000u64));
    }
}
