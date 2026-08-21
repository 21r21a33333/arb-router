//! Collapse duplicate opportunities and order what remains by attractiveness.
//!
//! The same cycle can be discovered from several starting rotations; they share
//! a `canonical_key`, so only the most profitable representative is kept.
//! Survivors are ordered by USD profit (priced opportunities ahead of unpriced),
//! ties broken by return in basis points.

use std::cmp::Reverse;
use std::collections::HashMap;

use crate::primitives::asset::Usd;
use crate::primitives::opportunity::Opportunity;

/// A total-order sort key: higher is better. Unpriced opportunities (`None`)
/// rank below any priced one because `None < Some`.
fn score(opp: &Opportunity) -> (Option<Usd>, u32) {
    (opp.profit_usd, opp.roi_bps)
}

/// Deduplicate by canonical path key (keeping the higher-scoring representative)
/// and return the survivors sorted best-first.
pub fn rank_and_dedup(opps: Vec<Opportunity>) -> Vec<Opportunity> {
    let mut best: HashMap<String, Opportunity> = HashMap::new();
    for opp in opps {
        let key = opp.path.canonical_key();
        match best.get(&key) {
            Some(existing) if score(existing) >= score(&opp) => {}
            _ => {
                best.insert(key, opp);
            }
        }
    }

    let mut ranked: Vec<Opportunity> = best.into_values().collect();
    ranked.sort_by_key(|opp| Reverse(score(opp)));
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::asset::{Amount, AssetId, ChainId, Pair};
    use crate::primitives::opportunity::{Hop, Path};
    use crate::primitives::pool::PoolId;
    use rust_decimal::Decimal;
    use time::OffsetDateTime;

    fn asset(s: &str) -> AssetId {
        AssetId::new(s).unwrap()
    }

    fn hop(pool: &str, src: &str, dst: &str) -> Hop {
        Hop {
            pool: PoolId::new(pool),
            pair: Pair {
                source: asset(src),
                destination: asset(dst),
            },
        }
    }

    fn opp(start: &str, hops: Vec<Hop>, profit: Option<i64>, roi: u32) -> Opportunity {
        Opportunity {
            chain: ChainId::new("ethereum"),
            path: Path {
                start: asset(start),
                hops,
            },
            input: Amount(Decimal::from(1000)),
            output: Amount(Decimal::from(1010)),
            profit_usd: profit.map(|p| Usd(Decimal::from(p))),
            roi_bps: roi,
            detected_at: OffsetDateTime::UNIX_EPOCH,
            worst_pool_synced_at: OffsetDateTime::UNIX_EPOCH,
            execution: None,
        }
    }

    #[test]
    fn dedups_cycle_rotations_and_sorts_by_profit() {
        // Two rotations of the same p1/p2/p3 cycle: identical canonical key.
        let rot_a = opp(
            "ethereum:a",
            vec![
                hop("p1", "ethereum:a", "ethereum:b"),
                hop("p2", "ethereum:b", "ethereum:c"),
                hop("p3", "ethereum:c", "ethereum:a"),
            ],
            Some(10),
            300,
        );
        let rot_b = opp(
            "ethereum:b",
            vec![
                hop("p2", "ethereum:b", "ethereum:c"),
                hop("p3", "ethereum:c", "ethereum:a"),
                hop("p1", "ethereum:a", "ethereum:b"),
            ],
            Some(5),
            150,
        );
        // A distinct, lower-profit cross-asset opportunity.
        let other = opp(
            "ethereum:a",
            vec![hop("q1", "ethereum:a", "ethereum:d")],
            Some(3),
            50,
        );

        let ranked = rank_and_dedup(vec![rot_b, other, rot_a]);

        // The two rotations collapsed to one, plus the distinct opportunity.
        assert_eq!(ranked.len(), 2);
        // Higher-profit rotation survived and leads.
        assert_eq!(ranked[0].profit_usd, Some(Usd(Decimal::from(10))));
        assert_eq!(ranked[1].profit_usd, Some(Usd(Decimal::from(3))));
    }

    #[test]
    fn priced_opportunities_outrank_unpriced() {
        let priced = opp(
            "ethereum:a",
            vec![hop("p1", "ethereum:a", "ethereum:b")],
            Some(1),
            10,
        );
        let unpriced = opp(
            "ethereum:a",
            vec![hop("p2", "ethereum:a", "ethereum:c")],
            None,
            9_000,
        );
        let ranked = rank_and_dedup(vec![unpriced, priced]);
        assert_eq!(ranked[0].profit_usd, Some(Usd(Decimal::from(1))));
        assert_eq!(ranked[1].profit_usd, None);
    }
}
