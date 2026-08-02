//! Freshness guard: a path is only tradeable if every pool it touches was
//! synced recently enough. Stale state means the quote no longer reflects the
//! chain, so the opportunity is likely a mirage.

use std::time::Duration;

use time::OffsetDateTime;

use crate::core::deps::pool_store::PoolSnapshot;
use crate::primitives::opportunity::Path;

/// Whether every pool on `path` was synced within `max_staleness` of `now`.
/// A missing pool counts as not fresh.
pub fn is_fresh(
    snapshot: &PoolSnapshot,
    path: &Path,
    now: OffsetDateTime,
    max_staleness: Duration,
) -> bool {
    let max = time::Duration::try_from(max_staleness).unwrap_or(time::Duration::MAX);
    path.hops.iter().all(|hop| match snapshot.get(&hop.pool) {
        Some(entry) => now - entry.meta.synced_at <= max,
        None => false,
    })
}

/// The oldest sync time among the path's pools — the freshness the whole
/// opportunity inherits. Missing pools pin the result to the epoch (maximally
/// stale).
pub fn worst_synced_at(snapshot: &PoolSnapshot, path: &Path) -> OffsetDateTime {
    path.hops
        .iter()
        .map(|hop| {
            snapshot
                .get(&hop.pool)
                .map_or(OffsetDateTime::UNIX_EPOCH, |entry| entry.meta.synced_at)
        })
        .min()
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::deps::pool::Pool;
    use crate::core::deps::pool_store::{PoolEntry, PoolMeta};
    use crate::primitives::asset::{Amount, AssetId, Pair};
    use crate::primitives::opportunity::Hop;
    use crate::primitives::pool::PoolId;
    use std::sync::Arc;

    struct Stub(PoolId, Vec<AssetId>);
    impl Pool for Stub {
        fn id(&self) -> PoolId {
            self.0.clone()
        }
        fn assets(&self) -> &[AssetId] {
            &self.1
        }
        fn quote(&self, _pair: &Pair, amount_in: Amount) -> Option<Amount> {
            Some(amount_in)
        }
    }

    fn snapshot_with(ages: &[(&str, i64)], now: OffsetDateTime) -> PoolSnapshot {
        let entries = ages
            .iter()
            .map(|(id, secs_ago)| PoolEntry {
                pool: Arc::new(Stub(
                    PoolId::new(id),
                    vec![
                        AssetId::new("ethereum:a").unwrap(),
                        AssetId::new("ethereum:b").unwrap(),
                    ],
                )),
                meta: PoolMeta {
                    synced_block: 1,
                    synced_at: now - time::Duration::seconds(*secs_ago),
                },
            })
            .collect();
        PoolSnapshot::from_entries(1, now, entries)
    }

    fn two_hop() -> Path {
        Path {
            start: AssetId::new("ethereum:a").unwrap(),
            hops: vec![
                Hop {
                    pool: PoolId::new("fresh"),
                    pair: Pair {
                        source: AssetId::new("ethereum:a").unwrap(),
                        destination: AssetId::new("ethereum:b").unwrap(),
                    },
                },
                Hop {
                    pool: PoolId::new("stale"),
                    pair: Pair {
                        source: AssetId::new("ethereum:b").unwrap(),
                        destination: AssetId::new("ethereum:a").unwrap(),
                    },
                },
            ],
        }
    }

    #[test]
    fn one_stale_pool_fails_freshness() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(1);
        let snap = snapshot_with(&[("fresh", 10), ("stale", 7200)], now);
        let path = two_hop();
        assert!(!is_fresh(&snap, &path, now, Duration::from_secs(3600)));
        // Widen the window past the stale pool's age and it passes.
        assert!(is_fresh(&snap, &path, now, Duration::from_secs(8000)));
        // Worst sync is the older of the two.
        assert_eq!(
            worst_synced_at(&snap, &path),
            now - time::Duration::seconds(7200)
        );
    }
}
