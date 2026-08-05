//! Bridge from `amm-rs` into arb-router's ports.
//!
//! All on-chain fetch/decode (discover + refresh) and swap math live in `amm-rs`
//! (`amm-rpc` + `amm-core`). This module is the thin type-translation seam:
//! - [`AmmRpcExchange`] adapts any `amm_rpc::StateSource` into arb-router's
//!   `Exchange` port. It owns its provider (inside the source), so the injected
//!   `ChainReader` is unused.
//! - [`AmmCorePool`] adapts an `amm_core` pool into arb-router's `Pool` port,
//!   delegating quoting and translating `Pair`/`Amount` at the boundary.

use alloy::eips::BlockId as AlloyBlockId;
use alloy::primitives::Address;
use amm_core::primitives::asset::AssetAmount as CoreAmount;
use amm_core::primitives::ratio::Bps;
use amm_core::traits::pool::Pool as CorePool;
use amm_rpc::RpcError;
use amm_rpc::source::StateSource;
use async_trait::async_trait;

use super::{amount_to_u256, chain_id, core_asset, u256_to_amount};
use crate::core::deps::chain_reader::ChainReader;
use crate::core::deps::exchange::{Exchange, ExchangeError};
use crate::core::deps::pool::Pool;
use crate::primitives::asset::{Amount, AssetId, ChainId, Pair};
use crate::primitives::chain::BlockId;
use crate::primitives::pool::{ExchangeId, PoolId, PoolKey};

/// An arb-router [`Pool`] backed by an `amm_core` pool.
pub struct AmmCorePool {
    id: PoolId,
    assets: Vec<AssetId>,
    inner: Box<dyn CorePool>,
}

impl AmmCorePool {
    /// Wrap an amm-core pool, deriving arb-router assets from it by re-namespacing
    /// each token address under `chain` (the chain name).
    fn new(chain: &str, inner: Box<dyn CorePool>) -> Self {
        let assets = inner
            .assets()
            .iter()
            .filter_map(|a| arb_asset(chain, a))
            .collect();
        Self {
            id: PoolId::new(inner.id().as_str()),
            assets,
            inner,
        }
    }
}

impl Pool for AmmCorePool {
    fn id(&self) -> PoolId {
        self.id.clone()
    }

    fn assets(&self) -> &[AssetId] {
        &self.assets
    }

    fn quote(&self, pair: &Pair, amount_in: Amount) -> Option<Amount> {
        let from = core_asset(&pair.source)?;
        let to = core_asset(&pair.destination)?;
        let raw = amount_to_u256(amount_in)?;
        if raw.is_zero() {
            return None;
        }
        let out = self.inner.quote(&CoreAmount::new(from, raw), &to).ok()?;
        match out.raw.is_zero() {
            true => None,
            false => u256_to_amount(out.raw),
        }
    }
}

/// An arb-router [`Exchange`] backed by an `amm_rpc` [`StateSource`]. The source
/// owns the provider it reads through, so the `ChainReader` handed to
/// `discover`/`refresh` is ignored.
pub struct AmmRpcExchange<S> {
    id: ExchangeId,
    chain: ChainId,
    source: S,
}

impl<S> AmmRpcExchange<S> {
    /// Wrap a state `source` as the exchange `id` on `chain`.
    pub fn new(id: &str, chain: ChainId, source: S) -> Self {
        Self {
            id: ExchangeId::new(id),
            chain,
            source,
        }
    }

    /// Translate an amm-core pool key back into an arb-router key (this exchange's
    /// id + chain, the pool address, and re-namespaced assets).
    fn to_arb_key(&self, key: &amm_core::primitives::pool::PoolKey) -> PoolKey {
        PoolKey {
            exchange: self.id.clone(),
            chain: self.chain.clone(),
            address: key.address.clone(),
            assets: key
                .assets
                .iter()
                .filter_map(|a| arb_asset(self.chain.as_str(), a))
                .collect(),
            fee_bps: key.fee_bps.map(|b| u32::from(b.0)),
        }
    }

    /// Translate an arb-router key into an amm-core key for refresh. `None` if an
    /// asset id doesn't resolve (never in practice — ids are `chain:0x…`).
    fn to_core_key(&self, key: &PoolKey) -> Option<amm_core::primitives::pool::PoolKey> {
        let assets = key
            .assets
            .iter()
            .map(core_asset)
            .collect::<Option<Vec<_>>>()?;
        Some(amm_core::primitives::pool::PoolKey {
            exchange: amm_core::primitives::pool::ExchangeId::new(key.exchange.as_str()),
            chain: core_chain(&key.chain),
            address: key.address.clone(),
            assets,
            fee_bps: key.fee_bps.map(|f| Bps(f as u16)),
        })
    }
}

#[async_trait]
impl<S: StateSource + Send + Sync> Exchange for AmmRpcExchange<S> {
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
        _reader: &dyn ChainReader,
    ) -> Result<Vec<PoolKey>, ExchangeError> {
        let core_tokens: Vec<_> = tokens.iter().filter_map(core_asset).collect();
        let keys = self
            .source
            .discover(&core_chain(chain), &core_tokens)
            .await
            .map_err(rpc_err)?;
        Ok(keys.iter().map(|k| self.to_arb_key(k)).collect())
    }

    async fn refresh(
        &self,
        keys: &[PoolKey],
        at: BlockId,
        _reader: &dyn ChainReader,
    ) -> Result<Vec<Box<dyn Pool>>, ExchangeError> {
        let core_keys: Vec<_> = keys.iter().filter_map(|k| self.to_core_key(k)).collect();
        let pools = self
            .source
            .refresh(&core_keys, alloy_block(at))
            .await
            .map_err(rpc_err)?;
        let chain = self.chain.as_str();
        Ok(pools
            .into_iter()
            .map(|p| Box::new(AmmCorePool::new(chain, p)) as Box<dyn Pool>)
            .collect())
    }
}

/// An amm-core asset id → an arb-router `"chain:0xaddress"` id.
///
/// The address is **lower-cased**: alloy's `Address` Display is EIP-55
/// checksummed (mixed case), but arb-router compares asset ids byte-for-byte and
/// its config / registry / valuation keys are all lowercase. Emitting checksummed
/// ids here would make every real token fail those exact-string lookups.
fn arb_asset(chain: &str, asset: &amm_core::primitives::asset::AssetId) -> Option<AssetId> {
    let addr = Address::from_word(asset.token);
    AssetId::new(&format!("{chain}:{addr:#x}")).ok()
}

/// An arb-router chain name → an amm-core numeric `ChainId`.
fn core_chain(chain: &ChainId) -> amm_core::primitives::asset::ChainId {
    amm_core::primitives::asset::ChainId(chain_id(chain.as_str()))
}

/// arb-router's `BlockId` → alloy's.
fn alloy_block(at: BlockId) -> AlloyBlockId {
    match at {
        BlockId::Latest => AlloyBlockId::latest(),
        BlockId::Number(n) => AlloyBlockId::number(n),
    }
}

/// Map an amm-rpc error into an exchange read error.
fn rpc_err(err: RpcError) -> ExchangeError {
    ExchangeError::Read(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use amm_core::primitives::asset::{AssetId as CoreAssetId, ChainId as CoreChainId};

    /// A refreshed pool must report asset ids that byte-match the lowercase ids
    /// used in config / registry / start-assets — alloy's `Address` Display is
    /// EIP-55 checksummed, so the bridge must lower-case it or the scanner's
    /// exact-string graph/pricing lookups miss every real token.
    #[test]
    fn arb_asset_lowercases_to_match_config_ids() {
        // USDC — an address with hex letters that checksum to mixed case.
        let usdc = alloy::primitives::address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let core = CoreAssetId::new(CoreChainId(1), usdc.into_word());
        let arb = arb_asset("ethereum", &core).expect("valid id");

        let config_id = "ethereum:0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
        assert_eq!(arb.as_str(), config_id, "id must be lowercase");
        assert_eq!(
            arb,
            AssetId::new(config_id).unwrap(),
            "must byte-match the config id"
        );
    }
}
