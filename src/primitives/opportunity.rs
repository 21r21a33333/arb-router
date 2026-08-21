use crate::primitives::asset::{Amount, AssetId, ChainId, Pair, Usd};
use crate::primitives::execution::ExecutionPlan;
use crate::primitives::pool::PoolId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hop {
    pub pool: PoolId,
    pub pair: Pair,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Path {
    pub start: AssetId,
    pub hops: Vec<Hop>,
}

impl Path {
    pub fn destination(&self) -> &AssetId {
        self.hops
            .last()
            .map(|h| &h.pair.destination)
            .unwrap_or(&self.start)
    }

    pub fn is_cycle(&self) -> bool {
        self.destination() == &self.start
    }

    /// Returns a stable string key for this path.
    /// For cycles, rotates the pool-id sequence so the lexicographically-smallest
    /// pool id comes first, ensuring rotations of the same cycle deduplicate.
    pub fn canonical_key(&self) -> String {
        if self.hops.is_empty() {
            return String::new();
        }
        let ids: Vec<&str> = self.hops.iter().map(|h| h.pool.as_str()).collect();

        if self.is_cycle() {
            // Find the index of the lexicographically smallest pool id
            let min_idx = ids
                .iter()
                .enumerate()
                .min_by_key(|(_, s)| *s)
                .map(|(i, _)| i)
                .unwrap_or(0);
            // Rotate so that the smallest id comes first
            let rotated: Vec<&str> = ids[min_idx..]
                .iter()
                .chain(ids[..min_idx].iter())
                .copied()
                .collect();
            rotated.join("->")
        } else {
            ids.join("->")
        }
    }
}

#[derive(Clone, Debug)]
pub struct Opportunity {
    pub chain: ChainId,
    pub path: Path,
    pub input: Amount,
    pub output: Amount,
    pub profit_usd: Option<Usd>,
    pub roi_bps: u32,
    pub detected_at: time::OffsetDateTime,
    pub worst_pool_synced_at: time::OffsetDateTime,
    /// Sign-ready transactions to capture this opportunity, when an executor is
    /// configured and the build succeeds; `None` otherwise (execution is a
    /// best-effort enrichment and never affects detection).
    pub execution: Option<ExecutionPlan>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::asset::{AssetId, Pair};
    use crate::primitives::pool::PoolId;

    fn hop(p: &str, s: &str, d: &str) -> Hop {
        Hop {
            pool: PoolId::new(p),
            pair: Pair {
                source: AssetId::new(s).unwrap(),
                destination: AssetId::new(d).unwrap(),
            },
        }
    }

    #[test]
    fn detects_cycle() {
        let p = Path {
            start: AssetId::new("ethereum:usdc").unwrap(),
            hops: vec![
                hop("p1", "ethereum:usdc", "ethereum:weth"),
                hop("p2", "ethereum:weth", "ethereum:usdc"),
            ],
        };
        assert!(p.is_cycle());
        assert_eq!(p.destination().as_str(), "ethereum:usdc");
    }

    #[test]
    fn non_cycle_when_dest_differs() {
        let p = Path {
            start: AssetId::new("ethereum:usdc").unwrap(),
            hops: vec![hop("p1", "ethereum:usdc", "ethereum:weth")],
        };
        assert!(!p.is_cycle());
    }

    #[test]
    fn canonical_key_rotates_to_min_pool_id() {
        // Cycle: p2 -> p1 -> p3 rotating so p1 is first => p1->p3->p2
        let p = Path {
            start: AssetId::new("ethereum:usdc").unwrap(),
            hops: vec![
                hop("p2", "ethereum:usdc", "ethereum:weth"),
                hop("p1", "ethereum:weth", "ethereum:dai"),
                hop("p3", "ethereum:dai", "ethereum:usdc"),
            ],
        };
        assert_eq!(p.canonical_key(), "p1->p3->p2");
    }

    #[test]
    fn canonical_key_non_cycle_no_rotation() {
        let p = Path {
            start: AssetId::new("ethereum:usdc").unwrap(),
            hops: vec![
                hop("p2", "ethereum:usdc", "ethereum:weth"),
                hop("p1", "ethereum:weth", "ethereum:dai"),
            ],
        };
        assert_eq!(p.canonical_key(), "p2->p1");
    }
}
