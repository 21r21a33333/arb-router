//! Typed configuration parsed from `Settings.toml` (via the `config` crate).
//!
//! Settings are strings/scalars only — the wiring in [`crate::setup`] turns them
//! into the typed values each adapter needs (asset ids, addresses, durations).

use rust_decimal::Decimal;
use serde::Deserialize;

use crate::core::application::evaluation::detect::AssetRegistry;
use crate::primitives::asset::{AssetId, AssetMeta};

/// Top-level configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct Settings {
    /// Socket the read API binds to, e.g. `"0.0.0.0:8080"`.
    pub api_bind: String,
    /// Base URL of the fiat price provider (CoinGecko-compatible).
    pub fiat_provider_url: String,
    /// Binance combined-stream WebSocket base (defaults to the public mainnet).
    #[serde(default)]
    pub binance_ws_url: Option<String>,
    /// CoinGecko poll interval in seconds (default 30).
    #[serde(default)]
    pub price_refresh_secs: Option<u64>,
    /// A cached price older than this (seconds) is treated as missing (default 120).
    #[serde(default)]
    pub price_staleness_secs: Option<u64>,
    /// Every asset the router knows: decimals, symbol, and price lookup ids.
    pub assets: Vec<AssetSettings>,
    /// Per-chain configuration.
    pub chains: Vec<ChainSettings>,
}

/// One asset's metadata and how to price it.
#[derive(Debug, Clone, Deserialize)]
pub struct AssetSettings {
    /// Namespaced asset id, e.g. `"ethereum:0xa0b8…"`.
    pub id: String,
    pub decimals: u8,
    pub symbol: String,
    /// CoinGecko price id, e.g. `"usd-coin"`.
    pub price_id: String,
    /// Binance symbol for the live feed, e.g. `"ETHUSDT"`. Omit for the long tail.
    #[serde(default)]
    pub binance_symbol: Option<String>,
}

/// One chain's RPC, sync/scan cadence, engine bounds, and exchanges.
#[derive(Debug, Clone, Deserialize)]
pub struct ChainSettings {
    /// Chain namespace, e.g. `"ethereum"`.
    pub chain_id: String,
    pub rpc_url: String,
    /// Non-canonical Multicall3 address, if this chain needs one.
    #[serde(default)]
    pub multicall_address: Option<String>,
    pub sync_interval_ms: u64,
    pub scan_interval_ms: u64,
    /// Maximum path length in hops.
    pub max_hops: usize,
    /// Fixed USD size quoted into each path.
    pub input_usd: Decimal,
    /// Assets each scan starts from.
    pub start_assets: Vec<String>,
    /// Tokens pool discovery ranges over.
    pub tracked_tokens: Vec<String>,
    /// Uniswap V3 deployment on this chain, if present.
    #[serde(default)]
    pub uniswap_v3: Option<UniswapV3Settings>,
    /// Uniswap V2 (or V2-fork) deployment on this chain, if present.
    #[serde(default)]
    pub uniswap_v2: Option<UniswapV2Settings>,
}

/// A Uniswap V3 deployment: its factory and the fee tiers to scan.
#[derive(Debug, Clone, Deserialize)]
pub struct UniswapV3Settings {
    pub factory: String,
    pub fee_tiers: Vec<u32>,
}

/// A Uniswap V2 deployment: its factory and swap fee in basis points.
#[derive(Debug, Clone, Deserialize)]
pub struct UniswapV2Settings {
    pub factory: String,
    pub fee_bps: u32,
}

impl Settings {
    /// Load and parse a `Settings` file (extension inferred, e.g. `Settings.toml`).
    pub fn load(path: &str) -> eyre::Result<Self> {
        let settings = config::Config::builder()
            .add_source(config::File::with_name(path))
            .build()?
            .try_deserialize()?;
        Ok(settings)
    }

    /// Parse settings from an inline TOML string (used by tests).
    pub fn from_toml(toml: &str) -> eyre::Result<Self> {
        let settings = config::Config::builder()
            .add_source(config::File::from_str(toml, config::FileFormat::Toml))
            .build()?
            .try_deserialize()?;
        Ok(settings)
    }

    /// The decimals + symbol of every configured asset, keyed by id.
    pub fn asset_registry(&self) -> eyre::Result<AssetRegistry> {
        let mut registry = AssetRegistry::new();
        for asset in &self.assets {
            registry.insert(
                AssetId::new(&asset.id)?,
                AssetMeta {
                    decimals: asset.decimals,
                    symbol: asset.symbol.clone(),
                },
            );
        }
        Ok(registry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        api_bind = "0.0.0.0:8080"
        fiat_provider_url = "https://api.coingecko.com/api/v3"

        [[assets]]
        id = "ethereum:usdc"
        decimals = 6
        symbol = "USDC"
        price_id = "usd-coin"

        [[assets]]
        id = "ethereum:weth"
        decimals = 18
        symbol = "WETH"
        price_id = "weth"

        [[chains]]
        chain_id = "ethereum"
        rpc_url = "https://eth.example/rpc"
        sync_interval_ms = 12000
        scan_interval_ms = 3000
        max_hops = 16
        input_usd = "1000"
        start_assets = ["ethereum:usdc", "ethereum:weth"]
        tracked_tokens = ["ethereum:usdc", "ethereum:weth"]

        [chains.uniswap_v3]
        factory = "0x1F98431c8aD98523631AE4a59f267346ea31F984"
        fee_tiers = [500, 3000]
    "#;

    #[test]
    fn parses_sample_and_builds_registry() {
        let settings = Settings::from_toml(SAMPLE).unwrap();

        assert_eq!(settings.chains.len(), 1);
        let chain = &settings.chains[0];
        assert_eq!(chain.max_hops, 16);
        assert_eq!(chain.input_usd, Decimal::from(1000));
        assert_eq!(
            chain.uniswap_v3.as_ref().unwrap().fee_tiers,
            vec![500, 3000]
        );

        let registry = settings.asset_registry().unwrap();
        let usdc = registry
            .get(&AssetId::new("ethereum:usdc").unwrap())
            .unwrap();
        assert_eq!(usdc.decimals, 6);
        assert_eq!(usdc.symbol, "USDC");
    }
}
