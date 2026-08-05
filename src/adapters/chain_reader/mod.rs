//! `ChainReader` over per-chain HTTP providers: reads the latest block height.
//!
//! This is the only on-chain read the sync loop performs itself — it pins a
//! block, then each exchange source (from `amm-rpc`) fetches pool state through
//! its own provider at that block.

use std::collections::HashMap;

use alloy::providers::Provider;
use async_trait::async_trait;

use crate::adapters::rpc::provider::EthProvider;
use crate::core::deps::chain_reader::{ChainReadError, ChainReader};
use crate::primitives::asset::ChainId;

/// A [`ChainReader`] backed by one HTTP provider per chain.
pub struct BlockReader {
    providers: HashMap<ChainId, EthProvider>,
}

impl BlockReader {
    /// Wrap a per-chain provider map.
    pub fn new(providers: HashMap<ChainId, EthProvider>) -> Self {
        Self { providers }
    }

    fn provider(&self, chain: &ChainId) -> Result<&EthProvider, ChainReadError> {
        self.providers.get(chain).ok_or_else(|| {
            ChainReadError::Internal(format!("no provider for chain `{}`", chain.as_str()))
        })
    }
}

fn transport(err: impl std::fmt::Display) -> ChainReadError {
    ChainReadError::Transport(err.to_string())
}

#[async_trait]
impl ChainReader for BlockReader {
    async fn latest_block(&self, chain: &ChainId) -> Result<u64, ChainReadError> {
        self.provider(chain)?
            .get_block_number()
            .await
            .map_err(transport)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::rpc::provider::make_provider;

    /// Live sanity check against a real RPC (defaults to a public endpoint;
    /// override with `ETH_RPC_URL`). Run with:
    /// `cargo test --lib -- --ignored reads_a_live_block`
    #[tokio::test]
    #[ignore = "live: reads a real block"]
    async fn reads_a_live_block() {
        let url = std::env::var("ETH_RPC_URL")
            .unwrap_or_else(|_| crate::test_utils::ETH_RPC_DEFAULT.to_string());
        let chain = ChainId::new("ethereum");
        let reader = BlockReader::new(HashMap::from([(
            chain.clone(),
            make_provider(&url).unwrap(),
        )]));

        let block = reader.latest_block(&chain).await.unwrap();
        assert!(block > 0, "latest block should be positive");
    }
}
