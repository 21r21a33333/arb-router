//! Curve `Exchange`: refreshes configured StableSwap pools into quotable
//! [`CurvePool`]s via `curve_adapter::build_pool`.
//!
//! Discovery is **config-driven** — Curve's registry topology is intricate, so
//! each pool's address, variant, and coins come from configuration. Refresh
//! reads the per-block state (`A`, `fee`, `balances`) and hands it to the
//! adapter, which constructs the correct `curve_math` variant. Dynamic rates
//! default to `10^(36 - decimals)`, correct for plain StableSwap pools.

use alloy::primitives::{Address, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use async_trait::async_trait;
use curve_adapter::{CurveVariant, RawPoolState, build_pool};

use super::pool::CurvePool;
use crate::adapters::exchanges::{call, pool_address, read_err};
use crate::core::deps::chain_reader::ChainReader;
use crate::core::deps::exchange::{Exchange, ExchangeError};
use crate::core::deps::pool::Pool;
use crate::primitives::asset::{AssetId, ChainId};
use crate::primitives::chain::{BatchOutput, BlockId};
use crate::primitives::pool::{ExchangeId, PoolKey};

sol! {
    interface ICurveStableSwap {
        function A() external view returns (uint256);
        function fee() external view returns (uint256);
        function balances(int128 i) external view returns (uint256);
    }
}

/// A configured Curve pool: where it is, which variant, and its coins/decimals.
#[derive(Clone)]
pub struct CurvePoolConfig {
    pub address: Address,
    pub variant: CurveVariant,
    pub coins: Vec<AssetId>,
    pub decimals: Vec<u8>,
}

/// Refreshes a configured set of Curve StableSwap pools on one chain.
pub struct CurveExchange {
    id: ExchangeId,
    chain: ChainId,
    pools: Vec<CurvePoolConfig>,
}

impl CurveExchange {
    pub fn new(id: &str, chain: ChainId, pools: Vec<CurvePoolConfig>) -> Self {
        Self {
            id: ExchangeId::new(id),
            chain,
            pools,
        }
    }

    fn config_for(&self, key: &PoolKey) -> Option<&CurvePoolConfig> {
        let addr = key.address.parse::<Address>().ok()?;
        self.pools.iter().find(|p| p.address == addr)
    }
}

#[async_trait]
impl Exchange for CurveExchange {
    fn id(&self) -> ExchangeId {
        self.id.clone()
    }

    fn supports(&self, chain: &ChainId) -> bool {
        chain == &self.chain
    }

    async fn discover(
        &self,
        _chain: &ChainId,
        _tokens: &[AssetId],
        _reader: &dyn ChainReader,
    ) -> Result<Vec<PoolKey>, ExchangeError> {
        // Config-driven: every configured pool is a key (no on-chain discovery).
        Ok(self
            .pools
            .iter()
            .map(|pool| PoolKey {
                exchange: self.id.clone(),
                chain: self.chain.clone(),
                address: pool.address.to_string(),
                assets: pool.coins.clone(),
                fee_bps: None,
            })
            .collect())
    }

    async fn refresh(
        &self,
        keys: &[PoolKey],
        at: BlockId,
        reader: &dyn ChainReader,
    ) -> Result<Vec<Box<dyn Pool>>, ExchangeError> {
        // Per pool: A, fee, then balances(0..n). Track each pool's slice offset.
        struct Plan<'a> {
            key: &'a PoolKey,
            config: &'a CurvePoolConfig,
            offset: usize,
        }
        let mut calls = Vec::new();
        let mut plans = Vec::new();
        for key in keys {
            let Some(config) = self.config_for(key) else {
                continue;
            };
            let addr = pool_address(key)?;
            let offset = calls.len();
            calls.push(call(addr, ICurveStableSwap::ACall {}.abi_encode()));
            calls.push(call(addr, ICurveStableSwap::feeCall {}.abi_encode()));
            for i in 0..config.coins.len() {
                calls.push(call(
                    addr,
                    ICurveStableSwap::balancesCall { i: i as i128 }.abi_encode(),
                ));
            }
            plans.push(Plan {
                key,
                config,
                offset,
            });
        }

        let out = reader
            .call_batch(&self.chain, at, calls)
            .await
            .map_err(read_err)?;

        let mut pools: Vec<Box<dyn Pool>> = Vec::new();
        for plan in plans {
            let Some(pool) = build_curve_pool(&out, plan.config, plan.offset, &plan.key.address)?
            else {
                continue; // a reverted read drops the pool this tick
            };
            pools.push(pool);
        }
        Ok(pools)
    }
}

/// Assemble one pool from its `A`/`fee`/`balances` results, or `None` if any read reverted.
fn build_curve_pool(
    out: &BatchOutput,
    config: &CurvePoolConfig,
    offset: usize,
    id: &str,
) -> Result<Option<Box<dyn Pool>>, ExchangeError> {
    let (Some(amp), Some(fee)) = (decode_u256(out, offset), decode_u256(out, offset + 1)) else {
        return Ok(None);
    };
    let mut balances = Vec::with_capacity(config.coins.len());
    for i in 0..config.coins.len() {
        match decode_u256(out, offset + 2 + i) {
            Some(balance) => balances.push(balance),
            None => return Ok(None),
        }
    }

    let raw = RawPoolState {
        variant: config.variant,
        balances,
        token_decimals: config.decimals.clone(),
        amp,
        fee: Some(fee),
        mid_fee: None,
        out_fee: None,
        fee_gamma: None,
        offpeg_fee_multiplier: None,
        price_scale: None,
        d: None,
        gamma: None,
        dynamic_rates: None,
        precisions: None,
        eth_variant: None,
    };
    let inner =
        build_pool(&raw).map_err(|e| ExchangeError::Decode(format!("curve build: {e:?}")))?;
    Ok(Some(Box::new(CurvePool::new(
        id,
        config.coins.clone(),
        inner,
    ))))
}

/// Decode a single-`uint256` result, or `None` if it reverted or won't decode.
/// `A`, `fee`, and `balances` share the same return shape.
fn decode_u256(out: &BatchOutput, idx: usize) -> Option<U256> {
    let result = out.results.get(idx)?;
    match result.success {
        true => ICurveStableSwap::ACall::abi_decode_returns(&result.data.0).ok(),
        false => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::deps::chain_reader::ChainReadError;
    use crate::primitives::asset::{Amount, Pair};
    use crate::primitives::chain::{Bytes, Call, CallResult};
    use alloy::primitives::address;
    use rust_decimal::Decimal;

    fn asset(s: &str) -> AssetId {
        AssetId::new(s).unwrap()
    }

    fn enc_u256(v: U256) -> Vec<u8> {
        ICurveStableSwap::ACall::abi_encode_returns(&v)
    }

    fn config() -> CurvePoolConfig {
        CurvePoolConfig {
            address: address!("0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7"),
            variant: CurveVariant::StableSwapV1,
            coins: vec![
                asset("ethereum:dai"),
                asset("ethereum:usdc"),
                asset("ethereum:usdt"),
            ],
            decimals: vec![18, 6, 6],
        }
    }

    /// A balanced 3pool: A=2000, fee=0.01%, ~1M of each coin (native units).
    struct FakeCurve {
        block: u64,
        balances: [u128; 3],
    }

    #[async_trait]
    impl ChainReader for FakeCurve {
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
                    let data = if c.calldata.0.starts_with(&ICurveStableSwap::ACall::SELECTOR) {
                        enc_u256(U256::from(2000u64))
                    } else if c
                        .calldata
                        .0
                        .starts_with(&ICurveStableSwap::feeCall::SELECTOR)
                    {
                        enc_u256(U256::from(1_000_000u64)) // 0.01% (1e10 denom)
                    } else {
                        let decoded =
                            ICurveStableSwap::balancesCall::abi_decode(&c.calldata.0).unwrap();
                        enc_u256(U256::from(self.balances[decoded.i as usize]))
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
    async fn discover_then_refresh_quotes_near_parity() {
        let chain = ChainId::new("ethereum");
        let ex = CurveExchange::new("curve", chain.clone(), vec![config()]);

        let keys = ex
            .discover(
                &chain,
                &[],
                &FakeCurve {
                    block: 1,
                    balances: [0; 3],
                },
            )
            .await
            .unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].assets.len(), 3);

        // Value-balanced: 1M DAI (1e18), 1M USDC (1e6), 1M USDT (1e6).
        let reader = FakeCurve {
            block: 100,
            balances: [
                1_000_000_u128 * 1_000_000_000_000_000_000,
                1_000_000_u128 * 1_000_000,
                1_000_000_u128 * 1_000_000,
            ],
        };
        let pools = ex
            .refresh(&keys, BlockId::Number(100), &reader)
            .await
            .unwrap();
        assert_eq!(pools.len(), 1);

        // 1 DAI (1e18) → ~1 USDC (1e6), just under parity after fee.
        let pair = Pair {
            source: asset("ethereum:dai"),
            destination: asset("ethereum:usdc"),
        };
        let out = pools[0]
            .quote(&pair, Amount(Decimal::from(1_000_000_000_000_000_000u64)))
            .expect("balanced curve pool must quote");
        assert!(out.0 > Decimal::from(990_000)); // > 0.99 USDC
        assert!(out.0 < Decimal::from(1_000_000)); // < 1.00 USDC
    }
}
