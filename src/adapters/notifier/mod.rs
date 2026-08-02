//! Opportunity sinks: a structured log, an in-memory buffer the API reads, and
//! a composite that fans out to several sinks with isolated failures.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;

use crate::core::deps::notifier::{Notifier, NotifyError};
use crate::primitives::asset::ChainId;
use crate::primitives::opportunity::Opportunity;

/// Emits each opportunity as a structured `tracing` event.
pub struct LogNotifier;

#[async_trait]
impl Notifier for LogNotifier {
    async fn notify(&self, chain: &ChainId, opps: &[Opportunity]) -> Result<(), NotifyError> {
        for opp in opps {
            tracing::info!(
                chain = chain.as_str(),
                path = opp.path.canonical_key(),
                input = %opp.input.0,
                output = %opp.output.0,
                roi_bps = opp.roi_bps,
                "opportunity"
            );
        }
        Ok(())
    }
}

/// Holds the latest opportunities per chain — the read side for the API. Each
/// notify **replaces** the chain's set, so an empty batch clears it.
#[derive(Default)]
pub struct MemoryNotifier {
    latest: RwLock<HashMap<ChainId, Vec<Opportunity>>>,
}

impl MemoryNotifier {
    pub fn new() -> Self {
        Self::default()
    }

    /// The latest opportunities for `chain`.
    pub fn get(&self, chain: &ChainId) -> Vec<Opportunity> {
        self.latest
            .read()
            .unwrap()
            .get(chain)
            .cloned()
            .unwrap_or_default()
    }

    /// The latest opportunities across every chain.
    pub fn all(&self) -> Vec<Opportunity> {
        self.latest
            .read()
            .unwrap()
            .values()
            .flatten()
            .cloned()
            .collect()
    }
}

#[async_trait]
impl Notifier for MemoryNotifier {
    async fn notify(&self, chain: &ChainId, opps: &[Opportunity]) -> Result<(), NotifyError> {
        self.latest
            .write()
            .unwrap()
            .insert(chain.clone(), opps.to_vec());
        Ok(())
    }
}

/// Fans out to several notifiers; a child failure is logged, never propagated —
/// one broken sink must not stop the others.
pub struct CompositeNotifier(pub Vec<Arc<dyn Notifier>>);

#[async_trait]
impl Notifier for CompositeNotifier {
    async fn notify(&self, chain: &ChainId, opps: &[Opportunity]) -> Result<(), NotifyError> {
        for notifier in &self.0 {
            if let Err(err) = notifier.notify(chain, opps).await {
                tracing::warn!(chain = chain.as_str(), error = %err, "notifier failed");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::asset::{Amount, AssetId, Pair, Usd};
    use crate::primitives::opportunity::{Hop, Path};
    use crate::primitives::pool::PoolId;
    use rust_decimal::Decimal;
    use time::OffsetDateTime;

    fn opp(pool: &str) -> Opportunity {
        Opportunity {
            chain: ChainId::new("ethereum"),
            path: Path {
                start: AssetId::new("ethereum:a").unwrap(),
                hops: vec![Hop {
                    pool: PoolId::new(pool),
                    pair: Pair {
                        source: AssetId::new("ethereum:a").unwrap(),
                        destination: AssetId::new("ethereum:b").unwrap(),
                    },
                }],
            },
            input: Amount(Decimal::from(1000)),
            output: Amount(Decimal::from(1010)),
            profit_usd: Some(Usd(Decimal::from(10))),
            roi_bps: 100,
            detected_at: OffsetDateTime::UNIX_EPOCH,
            worst_pool_synced_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[tokio::test]
    async fn memory_replaces_per_chain() {
        let mem = MemoryNotifier::new();
        let chain = ChainId::new("ethereum");

        mem.notify(&chain, &[opp("p1"), opp("p2")]).await.unwrap();
        assert_eq!(mem.get(&chain).len(), 2);

        // An empty batch replaces (clears) the chain's set.
        mem.notify(&chain, &[]).await.unwrap();
        assert_eq!(mem.get(&chain).len(), 0);
    }

    struct FailingNotifier;

    #[async_trait]
    impl Notifier for FailingNotifier {
        async fn notify(&self, _chain: &ChainId, _opps: &[Opportunity]) -> Result<(), NotifyError> {
            Err(NotifyError::Internal("boom".into()))
        }
    }

    #[tokio::test]
    async fn composite_calls_all_children_despite_a_failure() {
        let mem = Arc::new(MemoryNotifier::new());
        let composite = CompositeNotifier(vec![
            Arc::new(FailingNotifier),
            mem.clone() as Arc<dyn Notifier>,
        ]);
        let chain = ChainId::new("ethereum");

        // The first child errors, but the composite still succeeds and the
        // second child (memory) receives the batch.
        composite.notify(&chain, &[opp("p1")]).await.unwrap();
        assert_eq!(mem.get(&chain).len(), 1);
    }
}
