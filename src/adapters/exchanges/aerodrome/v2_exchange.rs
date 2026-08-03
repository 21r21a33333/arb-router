//! Aerodrome **v2** `Exchange`: discovers and refreshes the Solidly-style
//! volatile and stable pools behind one `PoolFactory`.
//!
//! - **discover** — for each tracked token pair, ask `getPool(a, b, stable)` for
//!   both `stable = false` (volatile) and `stable = true`, keeping the ones that
//!   exist.
//! - **refresh** — for each pool read `getReserves()`, its `stable()` flag, and
//!   `factory.getFee(pool, stable)` (both fee variants, so it is a single round —
//!   the flag then selects which fee applies). A volatile pool becomes a
//!   constant-product [`UniswapV2Pool`]; a stable pool becomes an
//!   [`AerodromeStablePool`], which needs token decimals — supplied at
//!   construction from the asset registry.

use std::collections::HashMap;

use alloy::primitives::{Address, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use async_trait::async_trait;

use super::stable::AerodromeStablePool;
use crate::adapters::exchanges::uniswap::ordered;
use crate::adapters::exchanges::uniswap::v2::UniswapV2Pool;
use crate::adapters::exchanges::{asset_address, call, pool_address, read_err};
use crate::core::deps::chain_reader::ChainReader;
use crate::core::deps::exchange::{Exchange, ExchangeError};
use crate::core::deps::pool::Pool;
use crate::primitives::asset::{AssetId, ChainId};
use crate::primitives::chain::{BlockId, Call, CallResult};
use crate::primitives::pool::{ExchangeId, PoolKey};

sol! {
    interface IAerodromePoolFactory {
        function getPool(address tokenA, address tokenB, bool stable) external view returns (address pool);
        function getFee(address pool, bool stable) external view returns (uint256);
    }

    interface IAerodromePool {
        function getReserves() external view returns (uint256 reserve0, uint256 reserve1, uint256 blockTimestampLast);
        function stable() external view returns (bool);
    }
}

// ─── exchange ─────────────────────────────────────────────────────────────────

/// Discovers and refreshes Aerodrome v2 (volatile + stable) pools on one chain.
pub struct AerodromeV2Exchange {
    id: ExchangeId,
    chain: ChainId,
    factory: Address,
    /// Decimal counts for tracked tokens — the stable curve math needs them; a
    /// stable pool whose tokens are missing here is skipped.
    token_decimals: HashMap<AssetId, u8>,
}

impl AerodromeV2Exchange {
    pub fn new(
        id: &str,
        chain: ChainId,
        factory: Address,
        token_decimals: HashMap<AssetId, u8>,
    ) -> Self {
        Self {
            id: ExchangeId::new(id),
            chain,
            factory,
            token_decimals,
        }
    }

    /// One `getPool` call per token pair × {volatile, stable}, tracking in
    /// lockstep which pair/flag each call asks about.
    fn get_pool_calls(
        &self,
        tokens: &[AssetId],
    ) -> Result<(Vec<Call>, Vec<Candidate>), ExchangeError> {
        let mut calls = Vec::new();
        let mut candidates = Vec::new();
        for i in 0..tokens.len() {
            for j in (i + 1)..tokens.len() {
                let (token0, token1) = ordered(&tokens[i], &tokens[j])?;
                let (addr0, addr1) = (asset_address(&token0)?, asset_address(&token1)?);
                for stable in [false, true] {
                    let calldata = IAerodromePoolFactory::getPoolCall {
                        tokenA: addr0,
                        tokenB: addr1,
                        stable,
                    }
                    .abi_encode();
                    calls.push(call(self.factory, calldata));
                    candidates.push(Candidate {
                        token0: token0.clone(),
                        token1: token1.clone(),
                    });
                }
            }
        }
        Ok((calls, candidates))
    }

    /// Keep the candidates whose `getPool` resolved to a real (non-zero) pool.
    fn pool_keys(
        &self,
        results: &[CallResult],
        candidates: Vec<Candidate>,
    ) -> Result<Vec<PoolKey>, ExchangeError> {
        let mut keys = Vec::new();
        for (result, candidate) in results.iter().zip(candidates) {
            if !result.success {
                continue;
            }
            let pool = IAerodromePoolFactory::getPoolCall::abi_decode_returns(&result.data.0)
                .map_err(|e| ExchangeError::Decode(format!("getPool: {e}")))?;
            if pool.is_zero() {
                continue;
            }
            keys.push(PoolKey {
                exchange: self.id.clone(),
                chain: self.chain.clone(),
                address: pool.to_string(),
                assets: vec![candidate.token0, candidate.token1],
                // The stable flag and fee are read at refresh, not carried here.
                fee_bps: None,
            });
        }
        Ok(keys)
    }

    /// Four calls per pool — reserves, the stable flag, and both fee variants —
    /// so one round yields everything: the flag then picks the applicable fee.
    fn state_calls(&self, keys: &[PoolKey]) -> Result<Vec<Call>, ExchangeError> {
        let mut calls = Vec::with_capacity(keys.len() * 4);
        for key in keys {
            let addr = pool_address(key)?;
            calls.push(call(addr, IAerodromePool::getReservesCall {}.abi_encode()));
            calls.push(call(addr, IAerodromePool::stableCall {}.abi_encode()));
            calls.push(call(
                self.factory,
                IAerodromePoolFactory::getFeeCall {
                    pool: addr,
                    stable: true,
                }
                .abi_encode(),
            ));
            calls.push(call(
                self.factory,
                IAerodromePoolFactory::getFeeCall {
                    pool: addr,
                    stable: false,
                }
                .abi_encode(),
            ));
        }
        Ok(calls)
    }

    /// Build the right quoter for one pool from its decoded state, or `None` if a
    /// read reverted or (for a stable pool) a token's decimals are unknown.
    fn build_pool(
        &self,
        key: &PoolKey,
        results: &[CallResult],
        base: usize,
    ) -> Option<Box<dyn Pool>> {
        let reserves = &results[base];
        let stable = &results[base + 1];
        if !reserves.success || !stable.success {
            return None;
        }
        let (reserve0, reserve1) = decode_reserves(&reserves.data.0).ok()?;
        let is_stable = decode_bool(&stable.data.0).ok()?;

        // The flag selects which pre-fetched fee applies (index +2 stable, +3 volatile).
        let fee_result = &results[base + if is_stable { 2 } else { 3 }];
        if !fee_result.success {
            return None;
        }
        let fee_bps = decode_fee(&fee_result.data.0).ok()?;

        let (token0, token1) = (&key.assets[0], &key.assets[1]);
        match is_stable {
            true => {
                let d0 = *self.token_decimals.get(token0)?;
                let d1 = *self.token_decimals.get(token1)?;
                let pool = AerodromeStablePool::from_reserves(
                    &key.address,
                    token0.as_str(),
                    token1.as_str(),
                    reserve0,
                    reserve1,
                    d0,
                    d1,
                    fee_bps,
                )
                .ok()?;
                Some(Box::new(pool))
            }
            false => {
                let pool = UniswapV2Pool::from_reserves(
                    &key.address,
                    token0.as_str(),
                    token1.as_str(),
                    reserve0,
                    reserve1,
                    fee_bps,
                    0,
                    0,
                )
                .ok()?;
                Some(Box::new(pool))
            }
        }
    }
}

#[async_trait]
impl Exchange for AerodromeV2Exchange {
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
        let (calls, candidates) = self.get_pool_calls(tokens)?;
        let out = reader
            .call_batch(chain, BlockId::Latest, calls)
            .await
            .map_err(read_err)?;
        self.pool_keys(&out.results, candidates)
    }

    async fn refresh(
        &self,
        keys: &[PoolKey],
        at: BlockId,
        reader: &dyn ChainReader,
    ) -> Result<Vec<Box<dyn Pool>>, ExchangeError> {
        let out = reader
            .call_batch(&self.chain, at, self.state_calls(keys)?)
            .await
            .map_err(read_err)?;
        let pools = keys
            .iter()
            .enumerate()
            .filter_map(|(i, key)| self.build_pool(key, &out.results, i * 4))
            .collect();
        Ok(pools)
    }
}

// ─── candidate + decoders ─────────────────────────────────────────────────────

/// A discovery candidate: the ordered token pair a `getPool` call asked about.
struct Candidate {
    token0: AssetId,
    token1: AssetId,
}

/// Decode a `getReserves()` return into `(reserve0, reserve1)`.
fn decode_reserves(data: &[u8]) -> Result<(U256, U256), ExchangeError> {
    let r = IAerodromePool::getReservesCall::abi_decode_returns(data)
        .map_err(|e| ExchangeError::Decode(format!("getReserves: {e}")))?;
    Ok((r.reserve0, r.reserve1))
}

/// Decode a `stable()` return.
fn decode_bool(data: &[u8]) -> Result<bool, ExchangeError> {
    IAerodromePool::stableCall::abi_decode_returns(data)
        .map_err(|e| ExchangeError::Decode(format!("stable: {e}")))
}

/// Decode a `getFee()` return, clamping to `u32` (fees are small; an implausibly
/// large value clamps to `u32::MAX`, which the quote math treats as un-quotable).
fn decode_fee(data: &[u8]) -> Result<u32, ExchangeError> {
    let fee = IAerodromePoolFactory::getFeeCall::abi_decode_returns(data)
        .map_err(|e| ExchangeError::Decode(format!("getFee: {e}")))?;
    Ok(u32::try_from(fee).unwrap_or(u32::MAX))
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::deps::chain_reader::ChainReadError;
    use crate::primitives::asset::{Amount, Pair};
    use crate::primitives::chain::{BatchOutput, Bytes};
    use alloy::primitives::address;
    use rust_decimal::Decimal;

    const USDC: &str = "base:0x0000000000000000000000000000000000000001";
    const DAI: &str = "base:0x0000000000000000000000000000000000000002";
    const FACTORY: Address = address!("0x420DD381b31aEf6683db6B902084cB0FFECe40Da");
    const STABLE_POOL: Address = address!("0x00000000000000000000000000000000000000aa");
    const VOLATILE_POOL: Address = address!("0x00000000000000000000000000000000000000bb");

    fn asset(s: &str) -> AssetId {
        AssetId::new(s).unwrap()
    }

    fn decimals() -> HashMap<AssetId, u8> {
        HashMap::from([(asset(USDC), 6), (asset(DAI), 18)])
    }

    fn exchange() -> AerodromeV2Exchange {
        AerodromeV2Exchange::new("aerodrome_v2", ChainId::new("base"), FACTORY, decimals())
    }

    fn tokens() -> Vec<AssetId> {
        vec![asset(USDC), asset(DAI)]
    }

    /// Answers `getPool` (distinct address per stable flag), `getReserves`,
    /// `stable`, and `getFee` for a balanced USDC/DAI pair.
    struct FakeAero {
        block: u64,
    }

    impl FakeAero {
        fn answer(&self, c: &Call) -> CallResult {
            let data = &c.calldata.0;
            let ok = |bytes: Vec<u8>| CallResult {
                success: true,
                data: Bytes(bytes),
            };
            if data.starts_with(&IAerodromePoolFactory::getPoolCall::SELECTOR) {
                let stable = IAerodromePoolFactory::getPoolCall::abi_decode(data)
                    .unwrap()
                    .stable;
                let pool = if stable { STABLE_POOL } else { VOLATILE_POOL };
                ok(IAerodromePoolFactory::getPoolCall::abi_encode_returns(
                    &pool,
                ))
            } else if data.starts_with(&IAerodromePoolFactory::getFeeCall::SELECTOR) {
                let stable = IAerodromePoolFactory::getFeeCall::abi_decode(data)
                    .unwrap()
                    .stable;
                let fee = U256::from(if stable { 5u32 } else { 30u32 });
                ok(IAerodromePoolFactory::getFeeCall::abi_encode_returns(&fee))
            } else if data.starts_with(&IAerodromePool::getReservesCall::SELECTOR) {
                // 1M USDC (1e12) / 1M DAI (1e24) — value-balanced.
                ok(IAerodromePool::getReservesCall::abi_encode_returns(
                    &IAerodromePool::getReservesReturn {
                        reserve0: U256::from(1_000_000_000_000u64),
                        reserve1: U256::from(1_000_000_000_000_000_000_000_000u128),
                        blockTimestampLast: U256::from(self.block),
                    },
                ))
            } else if data.starts_with(&IAerodromePool::stableCall::SELECTOR) {
                // The stable-flag call targets the pool address; route by target.
                let is_stable = c.target == STABLE_POOL.to_string();
                ok(IAerodromePool::stableCall::abi_encode_returns(&is_stable))
            } else {
                CallResult {
                    success: false,
                    data: Bytes(Vec::new()),
                }
            }
        }
    }

    #[async_trait]
    impl ChainReader for FakeAero {
        async fn latest_block(&self, _chain: &ChainId) -> Result<u64, ChainReadError> {
            Ok(self.block)
        }
        async fn call_batch(
            &self,
            _chain: &ChainId,
            _at: BlockId,
            calls: Vec<Call>,
        ) -> Result<BatchOutput, ChainReadError> {
            let results = calls.iter().map(|c| self.answer(c)).collect();
            Ok(BatchOutput {
                block: self.block,
                results,
            })
        }
    }

    #[tokio::test]
    async fn discover_finds_both_volatile_and_stable_pools() {
        let ex = exchange();
        let chain = ChainId::new("base");
        let keys = ex
            .discover(&chain, &tokens(), &FakeAero { block: 10 })
            .await
            .unwrap();
        // One pair × {volatile, stable} → two pools at distinct addresses.
        assert_eq!(keys.len(), 2);
        let addrs: Vec<String> = keys.iter().map(|k| k.address.clone()).collect();
        assert!(addrs.contains(&STABLE_POOL.to_string()));
        assert!(addrs.contains(&VOLATILE_POOL.to_string()));
    }

    #[tokio::test]
    async fn refresh_builds_stable_and_volatile_quoters() {
        let ex = exchange();
        let chain = ChainId::new("base");
        let keys = ex
            .discover(&chain, &tokens(), &FakeAero { block: 10 })
            .await
            .unwrap();

        let pools = ex
            .refresh(&keys, BlockId::Number(10), &FakeAero { block: 10 })
            .await
            .unwrap();
        assert_eq!(pools.len(), 2);

        // Both pools must quote USDC → DAI (1 USDC → ~1 DAI, near parity).
        let pair = Pair {
            source: asset(USDC),
            destination: asset(DAI),
        };
        for pool in &pools {
            let out = pool
                .quote(&pair, Amount(Decimal::from(1_000_000u64)))
                .expect("aerodrome pool must quote");
            assert!(out.0 > Decimal::ZERO);
        }
    }
}
