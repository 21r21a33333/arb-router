//! Port for pricing an asset in USD, used to rank cross-asset opportunities and
//! express profit in a common unit.

use async_trait::async_trait;

use crate::primitives::asset::{AssetId, Usd};

#[derive(Debug, thiserror::Error)]
pub enum ValuationError {
    /// No price is known for the asset.
    #[error("no USD price for asset `{0}`")]
    NotFound(String),
    #[error("valuation internal error: {0}")]
    Internal(String),
}

#[async_trait]
pub trait Valuation: Send + Sync {
    /// USD price of one whole unit of `asset`.
    async fn price(&self, asset: &AssetId) -> Result<Usd, ValuationError>;
}
