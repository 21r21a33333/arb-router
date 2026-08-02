//! Port for emitting detected opportunities to a sink (log, memory buffer, webhook).

use async_trait::async_trait;

use crate::primitives::asset::ChainId;
use crate::primitives::opportunity::Opportunity;

#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
    #[error("notify internal error: {0}")]
    Internal(String),
}

#[async_trait]
pub trait Notifier: Send + Sync {
    /// Emit the opportunities found for `chain` in one scan tick.
    async fn notify(&self, chain: &ChainId, opps: &[Opportunity]) -> Result<(), NotifyError>;
}
