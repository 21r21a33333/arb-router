//! `ChainReader` implementation over Multicall3.
//!
//! `call_batch` pins the block once, then splits the calls into fixed-size
//! chunks — one Multicall3 round trip each — and concatenates the results in the
//! original order. Every call in a batch therefore observes the same block, even
//! when the batch spans several round trips. A reverting sub-call is a
//! `success = false` result; only transport failures are `Err`.

use std::collections::HashMap;

use alloy::eips::BlockId as AlloyBlockId;
use alloy::primitives::{Address, Bytes as AlloyBytes, TxKind};
use alloy::providers::Provider;
use alloy::rpc::types::{TransactionInput, TransactionRequest};
use alloy::sol_types::SolCall;
use async_trait::async_trait;

use crate::adapters::rpc::multicall::{IMulticall3, MULTICALL3};
use crate::adapters::rpc::provider::EthProvider;
use crate::core::deps::chain_reader::{ChainReadError, ChainReader};
use crate::primitives::asset::ChainId;
use crate::primitives::chain::{BatchOutput, BlockId, Bytes, Call, CallResult};

/// A `ChainReader` backed by per-chain HTTP providers and the Multicall3 contract.
pub struct MulticallChainReader {
    providers: HashMap<ChainId, EthProvider>,
    /// Per-chain Multicall3 address overrides; chains absent here use the
    /// canonical [`MULTICALL3`] deployment.
    overrides: HashMap<ChainId, Address>,
    /// Calls per Multicall3 round trip.
    chunk_size: usize,
}

impl MulticallChainReader {
    /// `chunk_size` is clamped to at least 1. The canonical Multicall3 address is
    /// used on every chain unless overridden via [`with_overrides`](Self::with_overrides).
    pub fn new(providers: HashMap<ChainId, EthProvider>, chunk_size: usize) -> Self {
        Self {
            providers,
            overrides: HashMap::new(),
            chunk_size: chunk_size.max(1),
        }
    }

    /// Override the Multicall3 address on chains whose deployment differs from the
    /// canonical one. Chains left out fall back to [`MULTICALL3`].
    pub fn with_overrides(mut self, overrides: HashMap<ChainId, Address>) -> Self {
        self.overrides = overrides;
        self
    }

    fn provider(&self, chain: &ChainId) -> Result<&EthProvider, ChainReadError> {
        self.providers.get(chain).ok_or_else(|| {
            ChainReadError::Internal(format!("no provider for chain `{}`", chain.as_str()))
        })
    }

    fn multicall(&self, chain: &ChainId) -> Address {
        self.overrides.get(chain).copied().unwrap_or(MULTICALL3)
    }

    /// One Multicall3 `tryAggregate(false, …)` round trip at a pinned block.
    async fn aggregate(
        &self,
        chain: &ChainId,
        block: u64,
        calls: &[Call],
    ) -> Result<Vec<CallResult>, ChainReadError> {
        let provider = self.provider(chain)?;

        let mc_calls = calls
            .iter()
            .map(|c| {
                let target: Address = c.target.parse().map_err(|_| {
                    ChainReadError::Internal(format!("invalid target address `{}`", c.target))
                })?;
                Ok(IMulticall3::Call {
                    target,
                    callData: AlloyBytes::from(c.calldata.0.clone()),
                })
            })
            .collect::<Result<Vec<_>, ChainReadError>>()?;

        let calldata = IMulticall3::tryAggregateCall {
            requireSuccess: false,
            calls: mc_calls,
        }
        .abi_encode();

        let tx = TransactionRequest {
            to: Some(TxKind::Call(self.multicall(chain))),
            input: TransactionInput::new(AlloyBytes::from(calldata)),
            ..Default::default()
        };

        let output = provider
            .call(tx)
            .block(AlloyBlockId::number(block))
            .await
            .map_err(transport)?;

        let decoded = IMulticall3::tryAggregateCall::abi_decode_returns(&output)
            .map_err(|e| ChainReadError::Internal(format!("multicall decode: {e}")))?;

        Ok(decoded
            .into_iter()
            .map(|r| CallResult {
                success: r.success,
                data: Bytes(r.returnData.to_vec()),
            })
            .collect())
    }
}

fn transport(err: impl std::fmt::Display) -> ChainReadError {
    ChainReadError::Transport(err.to_string())
}

#[async_trait]
impl ChainReader for MulticallChainReader {
    async fn latest_block(&self, chain: &ChainId) -> Result<u64, ChainReadError> {
        self.provider(chain)?
            .get_block_number()
            .await
            .map_err(transport)
    }

    async fn call_batch(
        &self,
        chain: &ChainId,
        at: BlockId,
        calls: Vec<Call>,
    ) -> Result<BatchOutput, ChainReadError> {
        // Pin the block once so every chunk observes the same state.
        let block = match at {
            BlockId::Number(n) => n,
            BlockId::Latest => self.latest_block(chain).await?,
        };

        // Fire every chunk concurrently at the pinned block — a large discover is
        // many chunks, and the RPC (eRPC) hedges/load-balances them, so parallel
        // round trips collapse the batch to ~one round-trip of latency.
        // `join_all` preserves order, so concatenating keeps results aligned with
        // the input calls.
        let chunks = calls
            .chunks(self.chunk_size)
            .map(|chunk| self.aggregate(chain, block, chunk));
        let mut results = Vec::with_capacity(calls.len());
        for chunk_result in futures_util::future::join_all(chunks).await {
            results.extend(chunk_result?);
        }
        Ok(BatchOutput { block, results })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::rpc::provider::make_provider;
    use alloy::primitives::address;

    #[test]
    fn multicall_address_defaults_to_canonical_and_honors_override() {
        let custom_chain = ChainId::new("somechain");
        let custom_addr = address!("0x0000000000000000000000000000000000000abc");
        let reader = MulticallChainReader::new(HashMap::new(), 50)
            .with_overrides(HashMap::from([(custom_chain.clone(), custom_addr)]));

        // Unlisted chains fall back to the canonical deployment.
        assert_eq!(reader.multicall(&ChainId::new("ethereum")), MULTICALL3);
        // Listed chains use the override.
        assert_eq!(reader.multicall(&custom_chain), custom_addr);
    }

    /// Live sanity check against a real RPC (defaults to a public endpoint;
    /// override with `ETH_RPC_URL`). Run with:
    /// `cargo test --lib -- --ignored reads_a_live_block`
    #[tokio::test]
    #[ignore = "live: reads a real block"]
    async fn reads_a_live_block() {
        let url = std::env::var("ETH_RPC_URL")
            .unwrap_or_else(|_| crate::test_utils::ETH_RPC_DEFAULT.to_string());
        let chain = ChainId::new("ethereum");
        let reader = MulticallChainReader::new(
            HashMap::from([(chain.clone(), make_provider(&url).unwrap())]),
            50,
        );

        let block = reader.latest_block(&chain).await.unwrap();
        assert!(block > 0, "latest block should be positive");
    }
}
