//! Port for reading on-chain state: latest block height and batched `eth_call`.

use async_trait::async_trait;

use crate::primitives::asset::ChainId;
use crate::primitives::chain::{BatchOutput, BlockId, Call};

#[derive(Debug, thiserror::Error)]
pub enum ChainReadError {
    /// The RPC transport failed (connection, timeout, malformed response).
    #[error("chain transport error: {0}")]
    Transport(String),
    #[error("chain read internal error: {0}")]
    Internal(String),
}

#[async_trait]
pub trait ChainReader: Send + Sync {
    /// Current head block number for `chain`.
    async fn latest_block(&self, chain: &ChainId) -> Result<u64, ChainReadError>;

    /// Execute `calls` against `chain` at block `at`, preserving order.
    async fn call_batch(
        &self,
        chain: &ChainId,
        at: BlockId,
        calls: Vec<Call>,
    ) -> Result<BatchOutput, ChainReadError>;
}
