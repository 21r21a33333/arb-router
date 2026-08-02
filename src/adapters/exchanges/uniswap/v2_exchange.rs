//! Uniswap V2 `Exchange`: pool discovery via the factory and reserve refresh
//! into [`UniswapV2Pool`].
//!
//! Far simpler than V3 — one pool per token pair (no fee tiers), and state is
//! just two reserves read in a single `getReserves` call.

use alloy::primitives::{Address, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use async_trait::async_trait;

use super::ordered;
use super::v2::UniswapV2Pool;
use crate::adapters::exchanges::{asset_address, call, pool_address, read_err};
use crate::core::deps::chain_reader::ChainReader;
use crate::core::deps::exchange::{Exchange, ExchangeError};
use crate::core::deps::pool::Pool;
use crate::primitives::asset::{AssetId, ChainId};
use crate::primitives::chain::{BlockId, Call, CallResult};
use crate::primitives::pool::{ExchangeId, PoolKey};

/// An ordered token pair a `getPair` call is asking about.
type TokenPair = (AssetId, AssetId);

sol! {
    interface IUniswapV2Factory {
        function getPair(address tokenA, address tokenB) external view returns (address pair);
    }

    interface IUniswapV2Pair {
        function getReserves() external view returns (
            uint112 reserve0,
            uint112 reserve1,
            uint32 blockTimestampLast
        );
    }
}

/// Discovers and refreshes Uniswap V2 (and V2-fork) pools on one chain.
pub struct UniswapV2Exchange {
    id: ExchangeId,
    chain: ChainId,
    factory: Address,
    /// Swap fee in basis points (30 for standard Uniswap V2).
    fee_bps: u32,
}

impl UniswapV2Exchange {
    pub fn new(id: &str, chain: ChainId, factory: Address, fee_bps: u32) -> Self {
        Self {
            id: ExchangeId::new(id),
            chain,
            factory,
            fee_bps,
        }
    }

    /// One `getPair` call per unordered token pair, tracking each pair in lockstep.
    fn get_pair_calls(
        &self,
        tokens: &[AssetId],
    ) -> Result<(Vec<Call>, Vec<TokenPair>), ExchangeError> {
        let mut calls = Vec::new();
        let mut pairs = Vec::new();
        for i in 0..tokens.len() {
            for j in (i + 1)..tokens.len() {
                let (token0, token1) = ordered(&tokens[i], &tokens[j])?;
                let calldata = IUniswapV2Factory::getPairCall {
                    tokenA: asset_address(&token0)?,
                    tokenB: asset_address(&token1)?,
                }
                .abi_encode();
                calls.push(call(self.factory, calldata));
                pairs.push((token0, token1));
            }
        }
        Ok((calls, pairs))
    }

    /// Keep the pairs whose `getPair` resolved to a real (non-zero) pool.
    fn pool_keys(
        &self,
        results: &[CallResult],
        pairs: Vec<TokenPair>,
    ) -> Result<Vec<PoolKey>, ExchangeError> {
        let mut keys = Vec::new();
        for (result, (token0, token1)) in results.iter().zip(pairs) {
            if !result.success {
                continue;
            }
            let pair = IUniswapV2Factory::getPairCall::abi_decode_returns(&result.data.0)
                .map_err(|e| ExchangeError::Decode(format!("getPair: {e}")))?;
            if pair.is_zero() {
                continue;
            }
            keys.push(PoolKey {
                exchange: self.id.clone(),
                chain: self.chain.clone(),
                address: pair.to_string(),
                assets: vec![token0, token1],
                fee_bps: Some(self.fee_bps),
            });
        }
        Ok(keys)
    }
}

#[async_trait]
impl Exchange for UniswapV2Exchange {
    fn id(&self) -> ExchangeId {
        self.id.clone()
    }

    fn supports(&self, chain: &ChainId) -> bool {
        chain == &self.chain
    }

    async fn discover(
        &self,
        chain: &ChainId,
        tokens: &[AssetId],
        reader: &dyn ChainReader,
    ) -> Result<Vec<PoolKey>, ExchangeError> {
        let (calls, pairs) = self.get_pair_calls(tokens)?;
        let out = reader
            .call_batch(chain, BlockId::Latest, calls)
            .await
            .map_err(read_err)?;
        self.pool_keys(&out.results, pairs)
    }

    async fn refresh(
        &self,
        keys: &[PoolKey],
        at: BlockId,
        reader: &dyn ChainReader,
    ) -> Result<Vec<Box<dyn Pool>>, ExchangeError> {
        // One `getReserves` per pool, all pinned to the same block.
        let calls = keys
            .iter()
            .map(|key| {
                Ok(call(
                    pool_address(key)?,
                    IUniswapV2Pair::getReservesCall {}.abi_encode(),
                ))
            })
            .collect::<Result<Vec<_>, ExchangeError>>()?;
        let out = reader
            .call_batch(&self.chain, at, calls)
            .await
            .map_err(read_err)?;

        let mut pools: Vec<Box<dyn Pool>> = Vec::new();
        for (key, result) in keys.iter().zip(&out.results) {
            if !result.success {
                continue;
            }
            let (reserve0, reserve1) = decode_reserves(&result.data.0)?;
            let pool = UniswapV2Pool::from_reserves(
                &key.address,
                key.assets[0].as_str(),
                key.assets[1].as_str(),
                reserve0,
                reserve1,
                self.fee_bps,
                0,
                0,
            )
            .map_err(|e| ExchangeError::Decode(e.to_string()))?;
            pools.push(Box::new(pool));
        }
        Ok(pools)
    }
}

/// Decode a `getReserves()` return into `(reserve0, reserve1)`.
fn decode_reserves(data: &[u8]) -> Result<(U256, U256), ExchangeError> {
    let reserves = IUniswapV2Pair::getReservesCall::abi_decode_returns(data)
        .map_err(|e| ExchangeError::Decode(format!("getReserves: {e}")))?;
    Ok((U256::from(reserves.reserve0), U256::from(reserves.reserve1)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::deps::chain_reader::ChainReadError;
    use crate::primitives::asset::Amount;
    use crate::primitives::chain::{BatchOutput, Bytes};
    use alloy::primitives::address;
    use alloy::primitives::aliases::U112;
    use rust_decimal::Decimal;

    fn asset(s: &str) -> AssetId {
        AssetId::new(s).unwrap()
    }

    fn enc_reserves(reserve0: u128, reserve1: u128) -> Vec<u8> {
        IUniswapV2Pair::getReservesCall::abi_encode_returns(&IUniswapV2Pair::getReservesReturn {
            reserve0: U112::from(reserve0),
            reserve1: U112::from(reserve1),
            blockTimestampLast: 0,
        })
    }

    #[test]
    fn decodes_reserves() {
        let (r0, r1) = decode_reserves(&enc_reserves(1_000_000, 500)).unwrap();
        assert_eq!(r0, U256::from(1_000_000u64));
        assert_eq!(r1, U256::from(500u64));
    }

    /// Answers `getPair` with a fixed pool address and `getReserves` with fixed reserves.
    struct FakeReader {
        block: u64,
        pool: Address,
        reserve0: u128,
        reserve1: u128,
    }

    #[async_trait]
    impl ChainReader for FakeReader {
        async fn latest_block(&self, _chain: &ChainId) -> Result<u64, ChainReadError> {
            Ok(self.block)
        }
        async fn call_batch(
            &self,
            _chain: &ChainId,
            _at: BlockId,
            calls: Vec<Call>,
        ) -> Result<BatchOutput, ChainReadError> {
            let results = calls
                .iter()
                .map(|c| {
                    let data = if c
                        .calldata
                        .0
                        .starts_with(&IUniswapV2Factory::getPairCall::SELECTOR)
                    {
                        IUniswapV2Factory::getPairCall::abi_encode_returns(&self.pool)
                    } else {
                        enc_reserves(self.reserve0, self.reserve1)
                    };
                    CallResult {
                        success: true,
                        data: Bytes(data),
                    }
                })
                .collect();
            Ok(BatchOutput {
                block: self.block,
                results,
            })
        }
    }

    #[tokio::test]
    async fn discover_then_refresh_quotes() {
        let chain = ChainId::new("ethereum");
        let ex = UniswapV2Exchange::new(
            "uniswap_v2",
            chain.clone(),
            address!("0x5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f"),
            30,
        );
        let tokens = vec![
            asset("ethereum:0x0000000000000000000000000000000000000001"),
            asset("ethereum:0x0000000000000000000000000000000000000002"),
        ];
        let reader = FakeReader {
            block: 100,
            pool: address!("0x00000000000000000000000000000000000000aa"),
            reserve0: 1_000_000_000_000,
            reserve1: 500_000_000_000_000_000_000,
        };

        let keys = ex.discover(&chain, &tokens, &reader).await.unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].fee_bps, Some(30));

        let pools = ex
            .refresh(&keys, BlockId::Number(100), &reader)
            .await
            .unwrap();
        assert_eq!(pools.len(), 1);
        // The refreshed pool quotes from the decoded reserves.
        let pair = crate::primitives::asset::Pair {
            source: keys[0].assets[0].clone(),
            destination: keys[0].assets[1].clone(),
        };
        assert!(
            pools[0]
                .quote(&pair, Amount(Decimal::from(1_000_000)))
                .is_some()
        );
    }
}
