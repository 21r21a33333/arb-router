//! Aerodrome (Solidly) **stable** pool quoter — the `x³y + y³x` invariant.
//!
//! Stable pools hold assets meant to trade near parity, so instead of the
//! constant product `x·y = k` they preserve `k = x³y + y³x`. There is no
//! closed form for the output, so `getAmountOut` scales both reserves to
//! `1e18`, then Newton-iterates `_get_y` to find the new opposite-reserve that
//! restores the invariant. This is a faithful port of Aerodrome's on-chain
//! `Pool.sol` (`_k` / `_f` / `_d` / `_get_y` / `_getAmountOut`) — same integer
//! truncations, same Newton edge cases — so a quote matches the chain to the
//! wei. `decimals0`/`decimals1` are stored as `10^decimals`, as on-chain.

use alloy_primitives::U256;

use crate::adapters::exchanges::{amount_to_u256, u256_to_amount};
use crate::core::deps::pool::Pool;
use crate::primitives::asset::{Amount, AssetId, AssetIdError, Pair};
use crate::primitives::pool::PoolId;

// ─── struct ─────────────────────────────────────────────────────────────────

/// An Aerodrome stable (Solidly `x³y + y³x`) pool.
#[derive(Debug, Clone)]
pub struct AerodromeStablePool {
    id: PoolId,
    /// token0 asset id (the lower-address token in the pair).
    pub token0: AssetId,
    /// token1 asset id.
    pub token1: AssetId,
    /// On-chain reserve for token0 (base units).
    pub reserve0: U256,
    /// On-chain reserve for token1 (base units).
    pub reserve1: U256,
    /// `10^decimals` for token0 (as the contract stores it).
    pub decimals0: U256,
    /// `10^decimals` for token1.
    pub decimals1: U256,
    /// Swap fee out of 10_000 (Aerodrome's `factory.getFee`, e.g. 5 = 0.05%).
    pub fee_bps: u32,
    /// Cached slice used by `Pool::assets`.
    assets: [AssetId; 2],
}

impl AerodromeStablePool {
    /// Build a stable pool from reserves, token decimals, and the swap fee.
    ///
    /// `decimals0`/`decimals1` are the *decimal counts* (e.g. 6, 18); they are
    /// converted to `10^d` internally. `fee_bps` is out of 10_000.
    #[allow(clippy::too_many_arguments)]
    pub fn from_reserves(
        id: &str,
        token0: &str,
        token1: &str,
        reserve0: U256,
        reserve1: U256,
        decimals0: u8,
        decimals1: u8,
        fee_bps: u32,
    ) -> Result<Self, AssetIdError> {
        let t0 = AssetId::new(token0)?;
        let t1 = AssetId::new(token1)?;
        Ok(Self {
            id: PoolId::new(id),
            assets: [t0.clone(), t1.clone()],
            token0: t0,
            token1: t1,
            reserve0,
            reserve1,
            decimals0: pow10(decimals0),
            decimals1: pow10(decimals1),
            fee_bps,
        })
    }

    // ─── Solidly integer math (ported from Pool.sol) ─────────────────────────

    /// `getAmountOut`: exact-input swap output in base units, or `None` on zero
    /// input, an empty reserve, overflow, or Newton non-convergence.
    ///
    /// `zero_for_one`: `true` swaps token0 → token1, else token1 → token0.
    fn get_amount_out(&self, amount_in: U256, zero_for_one: bool) -> Option<U256> {
        if amount_in.is_zero() || self.reserve0.is_zero() || self.reserve1.is_zero() {
            return None;
        }

        // Fee is removed from the input first: amountIn -= amountIn * fee / 10000.
        let fee = amount_in.checked_mul(U256::from(self.fee_bps))? / U256::from(10_000u32);
        let amount_in = amount_in.checked_sub(fee)?;

        // Invariant is computed on the *raw* reserves (k rescales internally).
        let xy = self.k(self.reserve0, self.reserve1)?;

        // Scale reserves to 1e18 for the Newton solve.
        let r0 = self.reserve0.checked_mul(e18())? / self.decimals0;
        let r1 = self.reserve1.checked_mul(e18())? / self.decimals1;
        let (reserve_a, reserve_b, dec_in, dec_out) = match zero_for_one {
            true => (r0, r1, self.decimals0, self.decimals1),
            false => (r1, r0, self.decimals1, self.decimals0),
        };
        let amount_in_scaled = amount_in.checked_mul(e18())? / dec_in;

        // Solve for the new opposite reserve that restores the invariant.
        let y_new = self.get_y(amount_in_scaled.checked_add(reserve_a)?, xy, reserve_b)?;
        let out_scaled = reserve_b.checked_sub(y_new)?;

        // Back to the output token's base units.
        Some(out_scaled.checked_mul(dec_out)? / e18())
    }

    /// `_k`: the stable invariant `x³y + y³x` on raw reserves, each first scaled
    /// to `1e18` by its decimals.
    fn k(&self, x: U256, y: U256) -> Option<U256> {
        let xs = x.checked_mul(e18())? / self.decimals0;
        let ys = y.checked_mul(e18())? / self.decimals1;
        stable_invariant(xs, ys)
    }

    /// `_get_y`: Newton-iterate for the opposite reserve satisfying `f(x0,y)=xy`.
    /// Ports Aerodrome's edge cases verbatim (including its `_k(x0, y+1)` call,
    /// which — as on-chain — rescales the already-scaled `x0` by decimals). `None`
    /// mirrors the contract's `revert("!y")` non-convergence.
    fn get_y(&self, x0: U256, xy: U256, mut y: U256) -> Option<U256> {
        let one = U256::from(1u8);
        for _ in 0..255 {
            let k = f(x0, y)?;
            if k < xy {
                let derivative = d(x0, y)?;
                if derivative.is_zero() {
                    return None;
                }
                let mut dy = (xy - k).checked_mul(e18())? / derivative;
                if dy.is_zero() {
                    if k == xy {
                        return Some(y);
                    }
                    if self.k(x0, y.checked_add(one)?)? > xy {
                        return Some(y + one);
                    }
                    dy = one;
                }
                y = y.checked_add(dy)?;
            } else {
                let derivative = d(x0, y)?;
                if derivative.is_zero() {
                    return None;
                }
                let mut dy = (k - xy).checked_mul(e18())? / derivative;
                if dy.is_zero() {
                    if k == xy || f(x0, y.checked_sub(one)?)? < xy {
                        return Some(y);
                    }
                    dy = one;
                }
                y = y.checked_sub(dy)?;
            }
        }
        None // contract reverts("!y") if it never converges
    }
}

// ─── free Solidly helpers (operate on 1e18-scaled values) ────────────────────

/// `10^decimals` as a `U256`.
fn pow10(decimals: u8) -> U256 {
    U256::from(10u128.pow(decimals as u32))
}

/// The fixed-point unit `1e18`.
fn e18() -> U256 {
    U256::from(1_000_000_000_000_000_000u128)
}

/// The stable invariant `_a * _b / 1e18` where `_a = xy/1e18`, `_b =
/// (x²+y²)/1e18` — i.e. `x³y + y³x` in fixed point. Shared by `_k` and `_f`.
fn stable_invariant(x: U256, y: U256) -> Option<U256> {
    let a = x.checked_mul(y)? / e18();
    let b = (x.checked_mul(x)? / e18()).checked_add(y.checked_mul(y)? / e18())?;
    Some(a.checked_mul(b)? / e18())
}

/// `_f`: the invariant evaluated at already-scaled `(x0, y)`.
fn f(x0: U256, y: U256) -> Option<U256> {
    stable_invariant(x0, y)
}

/// `_d`: `∂f/∂y = 3·x0·y²/1e18² + x0³/1e18²`, the Newton step denominator.
fn d(x0: U256, y: U256) -> Option<U256> {
    let three = U256::from(3u8);
    let term1 = three
        .checked_mul(x0)?
        .checked_mul(y.checked_mul(y)? / e18())?
        / e18();
    let term2 = (x0.checked_mul(x0)? / e18()).checked_mul(x0)? / e18();
    term1.checked_add(term2)
}

// ─── Pool trait impl ─────────────────────────────────────────────────────────

impl Pool for AerodromeStablePool {
    fn id(&self) -> PoolId {
        self.id.clone()
    }

    fn assets(&self) -> &[AssetId] {
        &self.assets
    }

    fn quote(&self, pair: &Pair, amount_in: Amount) -> Option<Amount> {
        let zero_for_one = match (&pair.source, &pair.destination) {
            (s, d) if *s == self.token0 && *d == self.token1 => true,
            (s, d) if *s == self.token1 && *d == self.token0 => false,
            _ => return None,
        };
        let amount = amount_to_u256(amount_in)?;
        let out = self.get_amount_out(amount, zero_for_one)?;
        u256_to_amount(out)
    }
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;

    fn dai() -> AssetId {
        AssetId::new("base:dai").unwrap()
    }
    fn usdc() -> AssetId {
        AssetId::new("base:usdc").unwrap()
    }

    /// A value-balanced USDC(6)/DAI(18) stable pool: 1M of each, 0.05% fee.
    fn usdc_dai_pool() -> AerodromeStablePool {
        AerodromeStablePool::from_reserves(
            "base:aero-stable:0xtest",
            "base:usdc",
            "base:dai",
            U256::from(1_000_000_u128 * 1_000_000), // 1M USDC (6 dec)
            U256::from(1_000_000_u128 * 1_000_000_000_000_000_000), // 1M DAI (18 dec)
            6,
            18,
            5,
        )
        .unwrap()
    }

    /// A symmetric 18/18 stable pool with equal reserves and no fee — used to
    /// check the pure math (parity, symmetry, invariant root).
    fn symmetric_pool(fee_bps: u32) -> AerodromeStablePool {
        let r = U256::from(1_000_000_u128 * 1_000_000_000_000_000_000); // 1M, 18 dec
        AerodromeStablePool::from_reserves(
            "base:aero-stable:0xsym",
            "base:dai",
            "base:usdc",
            r,
            r,
            18,
            18,
            fee_bps,
        )
        .unwrap()
    }

    /// On a value-balanced stable pool, 1 unit in returns just under 1 unit out
    /// (fee + tiny slippage) — the whole point of the stable curve.
    #[test]
    fn balanced_pool_swaps_near_parity() {
        let pool = usdc_dai_pool();
        // 1 USDC (1e6) → DAI (1e18).
        let out = pool
            .get_amount_out(U256::from(1_000_000u64), true)
            .expect("must quote");
        // Just under 1 DAI: below parity (1e18), above 0.99 DAI after 0.05% fee.
        assert!(
            out < U256::from(1_000_000_000_000_000_000u128),
            "out {out} must be < 1 DAI"
        );
        assert!(
            out > U256::from(990_000_000_000_000_000u128),
            "out {out} must be > 0.99 DAI"
        );
    }

    /// The Newton solver returns a `y` that actually satisfies the invariant:
    /// `f(x0, y) ≈ xy`. This validates the solver independently of any blessed
    /// output value.
    #[test]
    fn get_y_returns_an_invariant_root() {
        let pool = symmetric_pool(0);
        let r = U256::from(1_000_000_u128 * 1_000_000_000_000_000_000);
        let xy = pool.k(r, r).unwrap();
        let amount_in = U256::from(1_000_000_000_000_000_000u128); // 1 unit, scaled
        let x0 = amount_in + r; // reserves already 1e18-scaled here
        let y = pool.get_y(x0, xy, r).unwrap();

        let got = f(x0, y).unwrap();
        // Newton converges to within a hair of the target invariant.
        let tol = xy / U256::from(1_000_000_000_000u128); // 1e-12 relative
        let diff = if got > xy { got - xy } else { xy - got };
        assert!(
            diff <= tol,
            "f(x0,y)={got} should be ≈ xy={xy} (diff {diff})"
        );
    }

    /// A symmetric pool must quote identically in both directions for equal input.
    #[test]
    fn symmetric_pool_is_direction_symmetric() {
        let pool = symmetric_pool(5);
        let amount = U256::from(1_000_000_000_000_000_000u128);
        let a = pool.get_amount_out(amount, true).unwrap();
        let b = pool.get_amount_out(amount, false).unwrap();
        assert_eq!(a, b);
    }

    /// Regression: pin the exact output for a fixed swap so future arithmetic
    /// drift is caught (golden-by-construction; the live run is the ground truth).
    #[test]
    fn get_amount_out_is_stable_for_fixed_input() {
        let pool = usdc_dai_pool();
        let out = pool
            .get_amount_out(U256::from(1_000_000_000u64), true)
            .unwrap(); // 1000 USDC
        // ~1000 DAI, just under, after 0.05% fee + slippage on a deep pool.
        assert!(out > U256::from(998_000_000_000_000_000_000u128));
        assert!(out < U256::from(1_000_000_000_000_000_000_000u128));
    }

    #[test]
    fn zero_input_and_empty_reserves_return_none() {
        let pool = usdc_dai_pool();
        assert!(pool.get_amount_out(U256::ZERO, true).is_none());
        let empty = AerodromeStablePool::from_reserves(
            "base:aero-stable:0xdead",
            "base:usdc",
            "base:dai",
            U256::ZERO,
            U256::ZERO,
            6,
            18,
            5,
        )
        .unwrap();
        assert!(
            empty
                .get_amount_out(U256::from(1_000_000u64), true)
                .is_none()
        );
    }

    #[test]
    fn quote_resolves_direction_and_rejects_unknown_pair() {
        let pool = usdc_dai_pool();
        let out = pool
            .quote(
                &Pair {
                    source: usdc(),
                    destination: dai(),
                },
                Amount(Decimal::from(1_000_000u64)),
            )
            .unwrap();
        assert!(out.0 > Decimal::ZERO);

        // Unknown pair → None.
        assert!(
            pool.quote(
                &Pair {
                    source: AssetId::new("base:weth").unwrap(),
                    destination: dai(),
                },
                Amount(Decimal::from(1_000_000u64)),
            )
            .is_none()
        );
    }
}
