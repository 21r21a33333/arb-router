//! Uniswap-family exchange adapters (2-asset AMMs): V2 (constant product),
//! V3 and V4 (concentrated liquidity). Aerodrome Slipstream reuses the V3 math.
//!
//! Shared plumbing for 2-asset pools lives here; the tick-crossing math
//! (`simulate_v3_swap`, `TickInfo`) lives in [`v3`] and is reused by [`v4`].

pub mod v2;
pub mod v2_exchange;
pub mod v3;
pub mod v3_exchange;
pub mod v4;
pub mod v4_exchange;

use alloy_primitives::U256;

use super::{amount_to_u256, asset_address, u256_to_amount};
use crate::core::deps::exchange::ExchangeError;
use crate::primitives::asset::{Amount, AssetId, Pair};

/// The `(token0, token1)` pair sorted by address, as Uniswap orders a pool's
/// tokens. Shared by the V2 and V3 exchange adapters.
pub(crate) fn ordered(a: &AssetId, b: &AssetId) -> Result<(AssetId, AssetId), ExchangeError> {
    match asset_address(a)? < asset_address(b)? {
        true => Ok((a.clone(), b.clone())),
        false => Ok((b.clone(), a.clone())),
    }
}

/// Resolve swap direction for a 2-asset pool from a `Pair`.
///
/// `Some(true)` ⇒ token0 → token1 (`zero_for_one`); `Some(false)` ⇒ token1 → token0;
/// `None` if the pair is not this pool's pair.
pub(crate) fn two_asset_direction(token0: &AssetId, token1: &AssetId, pair: &Pair) -> Option<bool> {
    match (&pair.source, &pair.destination) {
        (s, d) if s == token0 && d == token1 => Some(true),
        (s, d) if s == token1 && d == token0 => Some(false),
        _ => None,
    }
}

/// Wrap a 2-asset `simulate(zero_for_one, amount_in) -> Option<U256>` as a `Pool::quote`:
/// resolves direction and converts base-unit `Amount` ↔ `U256`. A zero/oversized input is
/// rejected by the conversion + the pool's own `simulate` (which returns `None` on zero).
pub(crate) fn quote_two_asset(
    token0: &AssetId,
    token1: &AssetId,
    pair: &Pair,
    amount_in: Amount,
    simulate: impl FnOnce(bool, U256) -> Option<U256>,
) -> Option<Amount> {
    let zero_for_one = two_asset_direction(token0, token1, pair)?;
    let amount_out = simulate(zero_for_one, amount_to_u256(amount_in)?)?;
    u256_to_amount(amount_out)
}

/// Test-only helper: set a bit in a Uniswap V3/V4 tick bitmap (shared by v3 & v4 tests).
#[cfg(test)]
pub(crate) fn set_tick_bitmap_bit(
    bitmap: &mut std::collections::HashMap<i16, U256>,
    tick: i32,
    tick_spacing: i32,
) {
    let compressed = match tick < 0 && tick % tick_spacing != 0 {
        true => (tick / tick_spacing) - 1,
        false => tick / tick_spacing,
    };
    let word_pos = (compressed >> 8) as i16;
    let bit_pos = (compressed % 256) as u8;
    let entry = bitmap.entry(word_pos).or_insert(U256::ZERO);
    *entry |= U256::from(1u64) << bit_pos;
}
