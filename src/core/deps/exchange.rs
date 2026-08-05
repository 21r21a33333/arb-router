//! Port for a DEX protocol family: it discovers which pools exist for a token
//! set and refreshes their on-chain state into quotable [`Pool`] instances.

use async_trait::async_trait;

use crate::core::deps::pool::Pool;
use crate::primitives::asset::{AssetId, ChainId};
use crate::primitives::chain::BlockId;
use crate::primitives::pool::{ExchangeId, PoolKey};

#[derive(Debug, thiserror::Error)]
pub enum ExchangeError {
    /// A required chain read failed.
    #[error("exchange read error: {0}")]
    Read(String),
    /// On-chain data could not be decoded into pool state.
    #[error("exchange decode error: {0}")]
    Decode(String),
    #[error("exchange internal error: {0}")]
    Internal(String),
}

#[async_trait]
pub trait Exchange: Send + Sync {
    fn id(&self) -> ExchangeId;

    /// Whether this exchange is deployed on `chain`.
    fn supports(&self, chain: &ChainId) -> bool;

    /// Enumerate the pools connecting `tokens` on `chain`.
    async fn discover(
        &self,
        chain: &ChainId,
        tokens: &[AssetId],
    ) -> Result<Vec<PoolKey>, ExchangeError>;

    /// Read current state for `keys` at block `at`, yielding quotable pools.
    async fn refresh(
        &self,
        keys: &[PoolKey],
        at: BlockId,
    ) -> Result<Vec<Box<dyn Pool>>, ExchangeError>;
}
