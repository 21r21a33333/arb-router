//! Quote a path end to end and decide whether it increases value.
//!
//! Two notions of profit coexist. A *cycle* returns to its start asset, so it
//! is profitable purely when `output > input` — no oracle needed. A
//! *cross-asset* path ends on a different asset, so profitability is the USD
//! delta between what went in and what came out, which requires a [`Valuation`].

use std::collections::HashMap;

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

use crate::core::deps::pool_store::PoolSnapshot;
use crate::core::deps::valuation::Valuation;
use crate::primitives::asset::{Amount, AssetId, AssetMeta, Usd};
use crate::primitives::opportunity::Path;

/// Per-asset metadata (decimals, symbol) needed to turn base units into USD.
pub type AssetRegistry = HashMap<AssetId, AssetMeta>;

/// A path with its quoted result and (when priceable) its net USD profit.
#[derive(Clone, Debug)]
pub struct Priced {
    pub path: Path,
    pub input: Amount,
    pub output: Amount,
    pub profit_usd: Option<Usd>,
}

/// Quote `input` hop by hop along `path`, threading each hop's output into the
/// next. `None` if the path is empty or any hop cannot quote.
pub fn quote_path(snapshot: &PoolSnapshot, path: &Path, input: Amount) -> Option<Amount> {
    match path.hops.is_empty() {
        true => None,
        false => {
            let mut amount = input;
            for hop in &path.hops {
                let entry = snapshot.get(&hop.pool)?;
                amount = entry.pool.quote(&hop.pair, amount)?;
            }
            Some(amount)
        }
    }
}

/// USD value of `amount` base units of `asset`, i.e. `amount / 10^decimals × price`.
pub async fn value_usd(
    valuation: &dyn Valuation,
    registry: &AssetRegistry,
    asset: &AssetId,
    amount: Amount,
) -> Option<Usd> {
    let meta = registry.get(asset)?;
    let divisor = pow10(meta.decimals)?;
    let price = valuation.price(asset).await.ok()?;
    let whole = amount.0.checked_div(divisor)?;
    Some(Usd(whole.checked_mul(price.0)?))
}

/// Base-unit amount of `asset` worth `usd`, i.e. `usd / price × 10^decimals`,
/// floored to whole units. The inverse of [`value_usd`]. `None` if the asset is
/// unknown or unpriced.
pub async fn amount_for_usd(
    valuation: &dyn Valuation,
    registry: &AssetRegistry,
    asset: &AssetId,
    usd: Usd,
) -> Option<Amount> {
    let meta = registry.get(asset)?;
    let price = valuation.price(asset).await.ok()?;
    let whole = usd.0.checked_div(price.0)?; // `None` when the price is zero
    Some(Amount(whole.checked_mul(pow10(meta.decimals)?)?.floor()))
}

/// Net USD gain of ending with `output` of `dest` versus starting with `input`
/// of `source`.
pub async fn net_profit_usd(
    valuation: &dyn Valuation,
    registry: &AssetRegistry,
    source: &AssetId,
    input: Amount,
    dest: &AssetId,
    output: Amount,
) -> Option<Usd> {
    let in_usd = value_usd(valuation, registry, source, input).await?;
    let out_usd = value_usd(valuation, registry, dest, output).await?;
    Some(Usd(out_usd.0 - in_usd.0))
}

/// Return over `base`, in basis points, clamped to `u32`. Negative or
/// zero-base cases return 0.
pub fn roi_bps(base: Decimal, gain: Decimal) -> u32 {
    match base <= Decimal::ZERO || gain <= Decimal::ZERO {
        true => 0,
        false => (gain / base * Decimal::from(10_000))
            .to_u32()
            .unwrap_or(u32::MAX),
    }
}

/// Whether a priced path increases value. A cycle qualifies when it returns
/// more of the start asset than it consumed; a cross-asset path qualifies when
/// its endpoints carry a positive net USD delta. v1 applies no further
/// threshold — every value-increasing path is reported.
pub fn is_profitable(priced: &Priced) -> bool {
    match priced.path.is_cycle() {
        true => priced.output > priced.input,
        false => matches!(priced.profit_usd, Some(profit) if profit.0 > Decimal::ZERO),
    }
}

/// `10^n` as a `Decimal`, or `None` if it exceeds the representable range.
fn pow10(n: u8) -> Option<Decimal> {
    let ten = Decimal::from(10u32);
    let mut value = Decimal::ONE;
    for _ in 0..n {
        value = value.checked_mul(ten)?;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::asset::Pair;
    use crate::primitives::opportunity::{Hop, Path};
    use crate::primitives::pool::PoolId;
    use crate::test_utils::{FakePool, FakeValuation, fake_snapshot};

    fn asset(s: &str) -> AssetId {
        AssetId::new(s).unwrap()
    }

    fn pair(source: &str, destination: &str) -> Pair {
        Pair {
            source: asset(source),
            destination: asset(destination),
        }
    }

    fn cycle_path() -> Path {
        Path {
            start: asset("ethereum:a"),
            hops: vec![
                Hop {
                    pool: PoolId::new("p1"),
                    pair: pair("ethereum:a", "ethereum:b"),
                },
                Hop {
                    pool: PoolId::new("p2"),
                    pair: pair("ethereum:b", "ethereum:a"),
                },
            ],
        }
    }

    #[test]
    fn quote_path_threads_amount_through_hops() {
        let snap = fake_snapshot(vec![
            FakePool::new("p1", &["ethereum:a", "ethereum:b"], Decimal::new(105, 2)),
            FakePool::new("p2", &["ethereum:a", "ethereum:b"], Decimal::ONE),
        ]);
        let out = quote_path(&snap, &cycle_path(), Amount(Decimal::from(1000))).unwrap();
        assert_eq!(out, Amount(Decimal::from(1050)));
    }

    #[test]
    fn cycle_is_profitable_when_output_exceeds_input() {
        let win = Priced {
            path: cycle_path(),
            input: Amount(Decimal::from(1000)),
            output: Amount(Decimal::from(1050)),
            profit_usd: None,
        };
        assert!(is_profitable(&win));

        let lose = Priced {
            output: Amount(Decimal::from(900)),
            ..win
        };
        assert!(!is_profitable(&lose));
    }

    #[tokio::test]
    async fn cross_asset_profit_is_usd_delta() {
        // 1 USDC (6 dec) in at $1.00 → 1 WETH (18 dec) out at $2.00: +$1.00.
        let registry: AssetRegistry = [
            (
                asset("ethereum:usdc"),
                AssetMeta {
                    decimals: 6,
                    symbol: "USDC".into(),
                },
            ),
            (
                asset("ethereum:weth"),
                AssetMeta {
                    decimals: 18,
                    symbol: "WETH".into(),
                },
            ),
        ]
        .into_iter()
        .collect();
        let valuation = FakeValuation::new(&[
            ("ethereum:usdc", Usd(Decimal::ONE)),
            ("ethereum:weth", Usd(Decimal::from(2))),
        ]);

        let profit = net_profit_usd(
            &valuation,
            &registry,
            &asset("ethereum:usdc"),
            Amount(Decimal::from(1_000_000)),
            &asset("ethereum:weth"),
            Amount(Decimal::from(1_000_000_000_000_000_000u64)),
        )
        .await
        .unwrap();
        assert_eq!(profit, Usd(Decimal::ONE));
    }

    #[tokio::test]
    async fn amount_for_usd_sizes_by_price_and_decimals() {
        // $2000 of WETH (18 dec) at $2000/WETH = exactly 1 WETH = 1e18 base units.
        let registry: AssetRegistry = [(
            asset("ethereum:weth"),
            AssetMeta {
                decimals: 18,
                symbol: "WETH".into(),
            },
        )]
        .into_iter()
        .collect();
        let valuation = FakeValuation::new(&[("ethereum:weth", Usd(Decimal::from(2000)))]);

        let amount = amount_for_usd(
            &valuation,
            &registry,
            &asset("ethereum:weth"),
            Usd(Decimal::from(2000)),
        )
        .await
        .unwrap();
        assert_eq!(amount, Amount(Decimal::from(1_000_000_000_000_000_000u64)));
    }

    #[tokio::test]
    async fn amount_for_usd_unpriced_asset_is_none() {
        let registry: AssetRegistry = [(
            asset("ethereum:weth"),
            AssetMeta {
                decimals: 18,
                symbol: "WETH".into(),
            },
        )]
        .into_iter()
        .collect();
        let valuation = FakeValuation::new(&[]);
        assert!(
            amount_for_usd(
                &valuation,
                &registry,
                &asset("ethereum:weth"),
                Usd(Decimal::from(1000)),
            )
            .await
            .is_none()
        );
    }

    #[test]
    fn cross_asset_profitability_follows_profit_sign() {
        let path = Path {
            start: asset("ethereum:usdc"),
            hops: vec![Hop {
                pool: PoolId::new("p1"),
                pair: pair("ethereum:usdc", "ethereum:weth"),
            }],
        };
        let priced = |profit: i64| Priced {
            path: path.clone(),
            input: Amount(Decimal::from(1_000_000)),
            output: Amount(Decimal::from(1)),
            profit_usd: Some(Usd(Decimal::from(profit))),
        };
        assert!(is_profitable(&priced(5)));
        assert!(!is_profitable(&priced(-5)));
    }
}
