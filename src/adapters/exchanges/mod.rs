//! DEX exchange adapters, grouped by protocol family.
//!
//! Helpers **universal to every exchange family** live here: the base-unit
//! `Amount` ↔ `U256` conversion used by quoters, plus the discover/refresh
//! plumbing (`call`, `read_err`, address parsing) shared by every `Exchange`
//! adapter. Family-specific plumbing lives in that family's module: [`uniswap`]
//! (2-asset AMMs: V2/V3/V4) and [`curve`] (N-asset stableswap / crypto).

pub mod aerodrome;
pub mod curve;
pub mod uniswap;

#[cfg(test)]
mod live;

use alloy_primitives::{Address, U256, keccak256};
use amm_core::primitives::asset::{AssetId as CoreAssetId, ChainId as CoreChainId};
use rust_decimal::Decimal;

use crate::core::deps::chain_reader::ChainReadError;
use crate::core::deps::exchange::ExchangeError;
use crate::primitives::asset::{Amount, AssetId};
use crate::primitives::chain::{Bytes, Call};
use crate::primitives::pool::PoolKey;

/// A `Call` to `target` carrying `calldata`.
pub(crate) fn call(target: Address, calldata: Vec<u8>) -> Call {
    Call {
        target: target.to_string(),
        calldata: Bytes(calldata),
    }
}

/// Wrap a chain-read failure as an exchange read error.
pub(crate) fn read_err(err: ChainReadError) -> ExchangeError {
    ExchangeError::Read(err.to_string())
}

/// Parse the address out of an `AssetId` (`"chain:0x…"`).
pub(crate) fn asset_address(asset: &AssetId) -> Result<Address, ExchangeError> {
    asset
        .as_str()
        .split(':')
        .nth(1)
        .and_then(|hex| hex.parse::<Address>().ok())
        .ok_or_else(|| {
            ExchangeError::Decode(format!("asset id `{}` is not `chain:0x…`", asset.as_str()))
        })
}

/// The pool address stored on a `PoolKey`, parsed.
pub(crate) fn pool_address(key: &PoolKey) -> Result<Address, ExchangeError> {
    key.address
        .parse::<Address>()
        .map_err(|_| ExchangeError::Decode(format!("pool address `{}`", key.address)))
}

/// Convert an `Amount` (base-unit integral Decimal) to `U256`.
///
/// Returns `None` if the value is negative, non-integral, or too large
/// to fit in a U256.
pub(crate) fn amount_to_u256(amount: Amount) -> Option<U256> {
    let d = amount.0;
    if d.is_sign_negative() {
        return None;
    }
    // Decimal must be an integer in base units — fractional part must be zero.
    if d.fract() != Decimal::ZERO {
        return None;
    }
    // Convert via the string representation to stay independent of internal layout.
    let s = d.to_string();
    // Parse as u128 first (covers all realistic on-chain values); fall back
    // to a larger-range parse for very large reserves.
    s.parse::<u128>().ok().map(U256::from).or_else(|| {
        // For values beyond u128::MAX parse digit-by-digit.
        let mut acc = U256::ZERO;
        let ten = U256::from(10u32);
        for ch in s.bytes() {
            let digit = ch.wrapping_sub(b'0');
            if digit > 9 {
                return None;
            }
            acc = acc.checked_mul(ten)?.checked_add(U256::from(digit))?;
        }
        Some(acc)
    })
}

/// Convert a `U256` base-unit integer to `Amount`.
///
/// Returns `None` if the value cannot be represented as a `Decimal`
/// (i.e. exceeds 96-bit mantissa — extremely unlikely for token amounts).
pub(crate) fn u256_to_amount(value: U256) -> Option<Amount> {
    // U256::to_string gives a decimal representation we can parse directly.
    let s = value.to_string();
    let d: Decimal = s.parse().ok()?;
    Some(Amount(d))
}

// ─── amm-core bridge ────────────────────────────────────────────────────────

/// Map an arb-router `AssetId` (`"chain:token"`) to an `amm_core::AssetId`.
///
/// The chain name maps to a numeric `ChainId`; the token maps to a 32-byte slot
/// (an address is used directly, any other token id is hashed to a stable slot).
/// Only equality matters for quoting — `amm-core`'s swap math is driven by
/// reserves/liquidity and direction, never by the concrete token value — so the
/// mapping just has to be deterministic and injective enough to resolve
/// direction within a pool.
pub(crate) fn core_asset(asset: &AssetId) -> Option<CoreAssetId> {
    let (chain, token) = asset.as_str().split_once(':')?;
    let slot = match token.parse::<Address>() {
        Ok(addr) => addr.into_word(),
        Err(_) => keccak256(token.as_bytes()),
    };
    Some(CoreAssetId::new(CoreChainId(chain_id(chain)), slot))
}

/// A numeric chain id for a chain name. Known chains get their canonical id;
/// anything else gets a stable FNV-1a hash (distinct and deterministic).
fn chain_id(name: &str) -> u64 {
    match name {
        "ethereum" => 1,
        "optimism" => 10,
        "bsc" | "bnb" => 56,
        "polygon" => 137,
        "base" => 8453,
        "arbitrum" => 42161,
        "avalanche" => 43114,
        other => {
            let mut h = 0xcbf29ce484222325u64;
            for b in other.bytes() {
                h ^= b as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
            h
        }
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip: 1_000_000 base units with 6 decimals ↔ Amount.
    #[test]
    fn amount_u256_round_trip_six_decimals() {
        let original = U256::from(1_000_000u64);
        let amount = u256_to_amount(original).expect("should convert to Amount");
        let back = amount_to_u256(amount).expect("should convert back to U256");
        assert_eq!(back, original, "round-trip must be lossless");
    }

    #[test]
    fn amount_to_u256_rejects_fractional() {
        let frac = Amount(Decimal::new(15, 1)); // 1.5
        assert!(amount_to_u256(frac).is_none());
    }

    #[test]
    fn amount_to_u256_rejects_negative() {
        let neg = Amount(Decimal::from(-1i64));
        assert!(amount_to_u256(neg).is_none());
    }

    #[test]
    fn u256_to_amount_roundtrips_large_value() {
        let v = U256::from(500_000_000_000_000_000_000u128);
        let a = u256_to_amount(v).unwrap();
        assert_eq!(a.0, Decimal::from(500_000_000_000_000_000_000u128));
    }
}
