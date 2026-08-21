//! The per-chain scan loop that assembles every application stage into one tick:
//! snapshot → graph → find paths → quote at fixed input → price → filter → rank
//! → notify.

use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;

use crate::core::application::config::{self, EngineConfig};
use crate::core::application::evaluation::detect::{
    AssetRegistry, Priced, amount_for_usd, is_profitable, net_profit_usd, quote_path, roi_bps,
    value_usd,
};
use crate::core::application::evaluation::rank::rank_and_dedup;
use crate::core::application::evaluation::validation::{is_fresh, worst_synced_at};
use crate::core::application::graph::Graph;
use crate::core::application::graph::finder::find_paths;
use crate::core::deps::executor::Executor;
use crate::core::deps::notifier::{Notifier, NotifyError};
use crate::core::deps::pool_store::{PoolSnapshot, PoolStore};
use crate::core::deps::valuation::Valuation;
use crate::primitives::asset::{Amount, ChainId, Usd};
use crate::primitives::opportunity::{Opportunity, Path};

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("notify failed: {0}")]
    Notify(#[from] NotifyError),
}

/// One chain's scanner, owning its ports and configuration.
pub struct Scanner<S: PoolStore, V: Valuation, N: Notifier> {
    pub chain: ChainId,
    pub pool_store: Arc<S>,
    pub valuation: Arc<V>,
    pub notifier: Arc<N>,
    pub registry: AssetRegistry,
    pub cfg: EngineConfig,
    /// Builds sign-ready calldata for ranked opportunities. `None` when no
    /// executor is configured for this chain (execution is left off).
    pub executor: Option<Box<dyn Executor>>,
}

impl<S: PoolStore, V: Valuation, N: Notifier> Scanner<S, V, N> {
    /// Run one scan tick, returning the number of opportunities emitted.
    pub async fn scan_once(&self) -> Result<usize, ScanError> {
        let snapshot = self.pool_store.snapshot(&self.chain);
        let graph = Graph::new(&snapshot);
        let now = OffsetDateTime::now_utc();

        // Enumerate every path from every start asset (bounded DFS, CPU-cheap),
        // then quote + price them concurrently — each `evaluate` awaits valuation
        // reads, so running them together overlaps that latency.
        let paths: Vec<Path> = self
            .cfg
            .start_assets
            .iter()
            .flat_map(|start| find_paths(&graph, start, self.cfg.max_hops))
            .collect();
        let evaluations = paths
            .into_iter()
            .map(|path| self.evaluate(&snapshot, path, now));
        let opps: Vec<Opportunity> = futures_util::future::join_all(evaluations)
            .await
            .into_iter()
            .flatten()
            .collect();

        let mut ranked = rank_and_dedup(opps);
        // Enrich the served set with sign-ready calldata. Best-effort: a build
        // failure leaves `execution: None` and never fails the tick.
        if let Some(executor) = &self.executor {
            for opp in &mut ranked {
                opp.execution = executor.build(opp, &snapshot).ok();
            }
        }
        let count = ranked.len();
        self.notifier.notify(&self.chain, &ranked).await?;
        Ok(count)
    }

    /// Scan repeatedly, sleeping `interval` between ticks. Tick errors are
    /// logged, never propagated — the loop must not die on a transient failure.
    pub async fn run(self: Arc<Self>, interval: Duration) {
        loop {
            match self.scan_once().await {
                Ok(count) => tracing::info!(
                    chain = self.chain.as_str(),
                    opportunities = count,
                    "scan tick complete"
                ),
                Err(err) => tracing::warn!(
                    chain = self.chain.as_str(),
                    error = %err,
                    "scan tick failed"
                ),
            }
            tokio::time::sleep(interval).await;
        }
    }

    /// Quote, price, and gate a single path into an [`Opportunity`] if it clears
    /// the freshness and profitability bars. The input is a fixed USD size of the
    /// path's source asset.
    async fn evaluate(
        &self,
        snapshot: &PoolSnapshot,
        path: Path,
        now: OffsetDateTime,
    ) -> Option<Opportunity> {
        if !is_fresh(snapshot, &path, now, config::MAX_POOL_STALENESS) {
            return None;
        }
        let input = amount_for_usd(
            &*self.valuation,
            &self.registry,
            &path.start,
            self.cfg.input_usd,
        )
        .await?;
        let output = quote_path(snapshot, &path, input)?;
        let (profit_usd, roi_bps) = self.price(&path, input, output).await;

        let priced = Priced {
            path,
            input,
            output,
            profit_usd,
        };
        match is_profitable(&priced) {
            false => None,
            true => Some(Opportunity {
                chain: self.chain.clone(),
                worst_pool_synced_at: worst_synced_at(snapshot, &priced.path),
                path: priced.path,
                input,
                output,
                profit_usd,
                roi_bps,
                detected_at: now,
                execution: None,
            }),
        }
    }

    /// USD profit and basis-point return for a sized path. Cycles price the gain
    /// in the start asset; cross-asset paths take the USD delta across endpoints.
    async fn price(&self, path: &Path, input: Amount, output: Amount) -> (Option<Usd>, u32) {
        let valuation = &*self.valuation;
        match path.is_cycle() {
            true => {
                let gain = output - input;
                let profit = value_usd(valuation, &self.registry, &path.start, gain).await;
                (profit, roi_bps(input.0, output.0 - input.0))
            }
            false => {
                let dest = path.destination();
                let profit =
                    net_profit_usd(valuation, &self.registry, &path.start, input, dest, output)
                        .await;
                let base = value_usd(valuation, &self.registry, &path.start, input).await;
                let roi = match (profit, base) {
                    (Some(p), Some(b)) => roi_bps(b.0, p.0),
                    _ => 0,
                };
                (profit, roi)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::asset::{AssetId, AssetMeta};
    use crate::test_utils::{
        FakePool, FakeValuation, RecordingNotifier, StaticPoolStore, fake_snapshot,
    };
    use rust_decimal::Decimal;

    fn asset(s: &str) -> AssetId {
        AssetId::new(s).unwrap()
    }

    fn registry() -> AssetRegistry {
        [
            (
                asset("ethereum:a"),
                AssetMeta {
                    decimals: 0,
                    symbol: "A".into(),
                },
            ),
            (
                asset("ethereum:b"),
                AssetMeta {
                    decimals: 0,
                    symbol: "B".into(),
                },
            ),
        ]
        .into_iter()
        .collect()
    }

    #[tokio::test]
    async fn emits_the_single_profitable_cycle() {
        // A->B->A round-trips to 1.1x through two distinct pools. Pricing B below
        // A keeps the one-hop cross-asset legs unprofitable, so only the cycle
        // survives — and its two rotations dedup to one.
        let snapshot = fake_snapshot(vec![
            FakePool::new("p1", &["ethereum:a", "ethereum:b"], Decimal::new(11, 1)),
            FakePool::new("p2", &["ethereum:a", "ethereum:b"], Decimal::ONE),
        ]);
        let notifier = Arc::new(RecordingNotifier::default());

        let mut cfg = EngineConfig::new(vec![asset("ethereum:a")]);
        cfg.max_hops = 4;

        let scanner = Scanner {
            chain: ChainId::new("ethereum"),
            pool_store: Arc::new(StaticPoolStore(snapshot)),
            valuation: Arc::new(FakeValuation::new(&[
                ("ethereum:a", Usd(Decimal::ONE)),
                ("ethereum:b", Usd(Decimal::new(5, 1))),
            ])),
            notifier: notifier.clone(),
            registry: registry(),
            cfg,
            executor: None,
        };

        let emitted = scanner.scan_once().await.unwrap();
        assert_eq!(emitted, 1);

        let recorded = notifier.recorded();
        assert_eq!(recorded.len(), 1);
        let opp = &recorded[0];
        assert!(opp.path.is_cycle());
        // $1000 of A (price $1, 0 decimals) = 1000 base units in; 1.1x round trip out.
        assert_eq!(opp.input, Amount(Decimal::from(1000)));
        assert_eq!(opp.output, Amount(Decimal::from(1100)));
        assert_eq!(opp.profit_usd, Some(Usd(Decimal::from(100))));
        // No executor configured → no calldata attached.
        assert!(opp.execution.is_none());
    }

    /// A configured executor enriches each ranked opportunity with calldata.
    #[tokio::test]
    async fn attaches_execution_when_executor_present() {
        use crate::core::deps::executor::{Executor, ExecutorError};
        use crate::primitives::execution::{ExecutionPlan, ExecutionTx};

        struct StubExecutor;
        impl Executor for StubExecutor {
            fn build(
                &self,
                _opp: &Opportunity,
                _snapshot: &PoolSnapshot,
            ) -> Result<ExecutionPlan, ExecutorError> {
                Ok(ExecutionPlan {
                    atomic: true,
                    transactions: vec![ExecutionTx {
                        to: "0xrouter".into(),
                        data: "0xabcd".into(),
                        value: "0".into(),
                        approval: None,
                    }],
                })
            }
        }

        let snapshot = fake_snapshot(vec![
            FakePool::new("p1", &["ethereum:a", "ethereum:b"], Decimal::new(11, 1)),
            FakePool::new("p2", &["ethereum:a", "ethereum:b"], Decimal::ONE),
        ]);
        let notifier = Arc::new(RecordingNotifier::default());
        let mut cfg = EngineConfig::new(vec![asset("ethereum:a")]);
        cfg.max_hops = 4;

        let scanner = Scanner {
            chain: ChainId::new("ethereum"),
            pool_store: Arc::new(StaticPoolStore(snapshot)),
            valuation: Arc::new(FakeValuation::new(&[
                ("ethereum:a", Usd(Decimal::ONE)),
                ("ethereum:b", Usd(Decimal::new(5, 1))),
            ])),
            notifier: notifier.clone(),
            registry: registry(),
            cfg,
            executor: Some(Box::new(StubExecutor)),
        };

        scanner.scan_once().await.unwrap();
        let recorded = notifier.recorded();
        let plan = recorded[0].execution.as_ref().expect("execution attached");
        assert!(plan.atomic);
        assert_eq!(plan.transactions.len(), 1);
    }
}
