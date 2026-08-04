//! Uniswap-family exchange adapters (2-asset AMMs): V2 (constant product),
//! V3 and V4 (concentrated liquidity). Aerodrome Slipstream reuses the V3 math.
//!
//! The swap math is delegated to `amm-core`; this module holds the shared
//! discover/refresh plumbing (token ordering).

pub mod v2;
pub mod v2_exchange;
pub mod v3;
pub mod v3_exchange;
pub mod v4;
pub mod v4_exchange;

#[cfg(test)]
use alloy_primitives::U256;

use super::asset_address;
use crate::core::deps::exchange::ExchangeError;
use crate::primitives::asset::AssetId;

/// The `(token0, token1)` pair sorted by address, as Uniswap orders a pool's
/// tokens. Shared by the V2 and V3 exchange adapters.
pub(crate) fn ordered(a: &AssetId, b: &AssetId) -> Result<(AssetId, AssetId), ExchangeError> {
    match asset_address(a)? < asset_address(b)? {
        true => Ok((a.clone(), b.clone())),
        false => Ok((b.clone(), a.clone())),
    }
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
