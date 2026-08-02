//! DEX exchange adapters, grouped by protocol family.
//!
//! Only helpers that are **universal to every exchange family** live here — the
//! base-unit `Amount` ↔ `U256` conversion used by all quoters. Family-specific
//! plumbing lives in that family's module: [`uniswap`] (2-asset AMMs: V2/V3/V4)
//! and [`curve`] (N-asset stableswap / crypto).

pub mod curve;
pub mod uniswap;

use alloy_primitives::U256;
use rust_decimal::Decimal;

use crate::primitives::asset::Amount;

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
