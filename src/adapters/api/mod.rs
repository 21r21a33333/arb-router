//! Read-only HTTP API over the router's live results.
//!
//! Three endpoints: `GET /v1/health`, `GET /v1/opportunities` (from the
//! in-memory notifier, optionally filtered by chain / minimum ROI), and
//! `GET /v1/status` (per-chain pool count and last synced block/time). Output
//! uses flat DTOs so the wire shape is stable and independent of the internal
//! value types.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::adapters::notifier::MemoryNotifier;
use crate::core::deps::pool_store::PoolStore;
use crate::primitives::asset::ChainId;
use crate::primitives::opportunity::Opportunity;

/// Everything the handlers read from.
pub struct ApiState {
    pub memory: Arc<MemoryNotifier>,
    pub store: Arc<dyn PoolStore>,
    pub chains: Vec<ChainId>,
}

/// Build the router over `state`.
pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/opportunities", get(opportunities))
        .route("/v1/status", get(status))
        .with_state(Arc::new(state))
}

/// Bind `addr` and serve until the process ends.
pub async fn serve(addr: &str, state: ApiState) -> eyre::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router(state)).await?;
    Ok(())
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

/// `?chain=` restricts to one chain; `?min_roi_bps=` filters weaker opportunities.
#[derive(Deserialize)]
struct OpportunityQuery {
    chain: Option<String>,
    min_roi_bps: Option<u32>,
}

async fn opportunities(
    State(state): State<Arc<ApiState>>,
    Query(query): Query<OpportunityQuery>,
) -> Json<Vec<OpportunityView>> {
    let opps = match &query.chain {
        Some(chain) => state.memory.get(&ChainId::new(chain)),
        None => state.memory.all(),
    };
    let min_roi = query.min_roi_bps.unwrap_or(0);
    let views = opps
        .iter()
        .filter(|opp| opp.roi_bps >= min_roi)
        .map(OpportunityView::from)
        .collect();
    Json(views)
}

async fn status(State(state): State<Arc<ApiState>>) -> Json<Vec<ChainStatus>> {
    let statuses = state
        .chains
        .iter()
        .map(|chain| {
            let snapshot = state.store.snapshot(chain);
            ChainStatus {
                chain: chain.as_str().to_string(),
                pool_count: snapshot.pool_count(),
                block: snapshot.block(),
                synced_at: snapshot.taken_at().unix_timestamp(),
            }
        })
        .collect();
    Json(statuses)
}

/// Flat, JSON-friendly view of an [`Opportunity`].
#[derive(Serialize)]
struct OpportunityView {
    chain: String,
    path: String,
    input: String,
    output: String,
    profit_usd: Option<String>,
    roi_bps: u32,
    detected_at: i64,
    worst_pool_synced_at: i64,
}

impl From<&Opportunity> for OpportunityView {
    fn from(opp: &Opportunity) -> Self {
        Self {
            chain: opp.chain.as_str().to_string(),
            path: opp.path.canonical_key(),
            input: opp.input.0.to_string(),
            output: opp.output.0.to_string(),
            profit_usd: opp.profit_usd.map(|p| p.0.to_string()),
            roi_bps: opp.roi_bps,
            detected_at: opp.detected_at.unix_timestamp(),
            worst_pool_synced_at: opp.worst_pool_synced_at.unix_timestamp(),
        }
    }
}

#[derive(Serialize)]
struct ChainStatus {
    chain: String,
    pool_count: usize,
    block: u64,
    synced_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::pool_store::ArcSwapPoolStore;
    use crate::core::deps::notifier::Notifier;
    use crate::primitives::asset::{Amount, AssetId, Pair, Usd};
    use crate::primitives::opportunity::{Hop, Path};
    use crate::primitives::pool::PoolId;
    use rust_decimal::Decimal;
    use time::OffsetDateTime;

    fn opp() -> Opportunity {
        Opportunity {
            chain: ChainId::new("ethereum"),
            path: Path {
                start: AssetId::new("ethereum:a").unwrap(),
                hops: vec![Hop {
                    pool: PoolId::new("p1"),
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
    async fn health_and_opportunities_endpoints() {
        let chain = ChainId::new("ethereum");
        let memory = Arc::new(MemoryNotifier::new());
        memory.notify(&chain, &[opp()]).await.unwrap();
        let store: Arc<dyn PoolStore> =
            Arc::new(ArcSwapPoolStore::new(std::slice::from_ref(&chain)));
        let state = ApiState {
            memory,
            store,
            chains: vec![chain],
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });

        let base = format!("http://{addr}");
        let client = reqwest::Client::new();

        let health: serde_json::Value = client
            .get(format!("{base}/v1/health"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(health["status"], "ok");

        let opps: Vec<serde_json::Value> = client
            .get(format!("{base}/v1/opportunities"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(opps.len(), 1);
        assert_eq!(opps[0]["roi_bps"], 100);

        // The min-ROI filter excludes it.
        let filtered: Vec<serde_json::Value> = client
            .get(format!("{base}/v1/opportunities?min_roi_bps=200"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(filtered.len(), 0);
    }
}
