//! Port for reading on-chain state: latest block height and batched `eth_call`.

use async_trait::async_trait;

use crate::primitives::asset::ChainId;

#[derive(Debug, thiserror::Error)]
pub enum ChainReadError {
    /// The RPC transport failed (connection, timeout, malformed response).
    #[error("chain transport error: {0}")]
    Transport(String),
    #[error("chain read internal error: {0}")]
    Internal(String),
}

/// Reads a chain's latest block height — the one on-chain read the sync loop
/// needs itself (pool state is fetched by the exchange sources, which own their
/// providers).
#[async_trait]
pub trait ChainReader: Send + Sync {
    /// Current head block number for `chain`.
    async fn latest_block(&self, chain: &ChainId) -> Result<u64, ChainReadError>;
}
