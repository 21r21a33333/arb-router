//! Curve-family exchange adapter: [`pool::CurvePool`] wraps the `curve-math` engine,
//! which covers every StableSwap and CryptoSwap variant.
//!
//! Shared plumbing for N-asset pools (index resolution + the `get_dy` quote wrapper)
//! lives here.

pub mod pool;

use alloy_primitives::U256;

use super::{amount_to_u256, u256_to_amount};
use crate::primitives::asset::{Amount, AssetId, Pair};

/// Resolve `(i, j)` coin indices for an N-asset pool from a `Pair`.
///
/// `None` if either coin is absent from the pool or `i == j`.
pub(crate) fn pair_indices(coins: &[AssetId], pair: &Pair) -> Option<(usize, usize)> {
    let i = coins.iter().position(|c| c == &pair.source)?;
    let j = coins.iter().position(|c| c == &pair.destination)?;
    match i == j {
        true => None,
        false => Some((i, j)),
    }
}

/// Wrap an N-asset `get_dy(i, j, dx) -> Option<U256>` as a `Pool::quote`.
pub(crate) fn quote_n_asset(
    coins: &[AssetId],
    pair: &Pair,
    amount_in: Amount,
    get_dy: impl FnOnce(usize, usize, U256) -> Option<U256>,
) -> Option<Amount> {
    let (i, j) = pair_indices(coins, pair)?;
    let dx = amount_to_u256(amount_in)?;
    match dx.is_zero() {
        true => None,
        false => u256_to_amount(get_dy(i, j, dx)?),
    }
}
