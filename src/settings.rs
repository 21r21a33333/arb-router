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
    /// CoinGecko Demo API key (`x-cg-demo-api-key`); the public API is heavily
    /// rate-limited without one.
    #[serde(default)]
    pub coingecko_api_key: Option<String>,
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
    /// Address the executor sends from and returns arb output to. When set (and
    /// an amm-rs chain preset exists for this chain), ranked opportunities are
    /// enriched with sign-ready calldata. Unset leaves execution off.
    #[serde(default)]
    pub executor_address: Option<String>,
    /// Slippage tolerance (bps) applied to each span's minimum output.
    /// Defaults to 30 bps.
    #[serde(default)]
    pub execution_slippage_bps: Option<u16>,
    /// Uniswap V3 deployment on this chain, if present.
    #[serde(default)]
    pub uniswap_v3: Option<UniswapV3Settings>,
    /// Uniswap V2 (or V2-fork) deployment on this chain, if present.
    #[serde(default)]
    pub uniswap_v2: Option<UniswapV2Settings>,
    /// Uniswap V4 pools to track on this chain (config-driven, like Curve).
    #[serde(default)]
    pub uniswap_v4: Option<UniswapV4Settings>,
    /// Curve StableSwap pools to track on this chain (config-driven discovery).
    #[serde(default)]
    pub curve: Option<CurveSettings>,
    /// Aerodrome v2 (volatile + stable) deployment on this chain, if present.
    #[serde(default)]
    pub aerodrome_v2: Option<AerodromeV2Settings>,
    /// Aerodrome Slipstream (concentrated-liquidity) deployment, if present.
    #[serde(default)]
    pub aerodrome_slipstream: Option<AerodromeSlipstreamSettings>,
}

/// An Aerodrome v2 deployment: just its `PoolFactory` (fees and the stable flag
/// are read per pool on-chain).
#[derive(Debug, Clone, Deserialize)]
pub struct AerodromeV2Settings {
    pub factory: String,
}

/// An Aerodrome Slipstream deployment: its `CLFactory` and the tick spacings to
/// scan (Slipstream's discovery dimension, e.g. 1, 50, 100, 200, 2000).
#[derive(Debug, Clone, Deserialize)]
pub struct AerodromeSlipstreamSettings {
    pub factory: String,
    pub tick_spacings: Vec<i32>,
}

/// A Uniswap V4 deployment: the singleton `PoolManager` and the pools to track.
///
/// V4 has no factory to enumerate, so each pool is listed explicitly (its
/// `pool_id` is derived from these fields).
#[derive(Debug, Clone, Deserialize)]
pub struct UniswapV4Settings {
    /// The singleton `PoolManager` address on this chain.
    pub pool_manager: String,
    pub pools: Vec<V4PoolSettings>,
}

/// One configured Uniswap V4 pool. `coin0`/`coin1` are asset ids (order does
/// not matter — currencies are sorted by address before the id is derived).
#[derive(Debug, Clone, Deserialize)]
pub struct V4PoolSettings {
    pub coin0: String,
    pub coin1: String,
    /// Fee in hundredths of a bip (e.g. 500 = 0.05%).
    pub fee: u32,
    pub tick_spacing: i32,
    /// Hooks contract; omit for a hookless pool (the zero address).
    #[serde(default)]
    pub hooks: Option<String>,
}

/// The Curve pools to track on a chain.
#[derive(Debug, Clone, Deserialize)]
pub struct CurveSettings {
    pub pools: Vec<CurvePoolSettings>,
}

/// One configured Curve pool: address, variant name, and its coins (asset ids).
#[derive(Debug, Clone, Deserialize)]
pub struct CurvePoolSettings {
    pub address: String,
    /// Curve variant, e.g. `"StableSwapV1"`, `"TriCryptoNG"`.
    pub variant: String,
    pub coins: Vec<String>,
    /// Meta pools only: the base pool address (its `get_virtual_price` is the LP
    /// coin's rate). Required for `StableSwapMeta`.
    #[serde(default)]
    pub base_pool: Option<String>,
    /// `TwoCryptoV1` only: whether it's the WETH (ETH-variant) solver.
    #[serde(default)]
    pub eth_variant: Option<bool>,
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

    /// The shipped `Settings.example.toml` must always deserialize — this guards
    /// against config drift as new exchange blocks are added.
    #[test]
    fn example_config_parses() {
        let settings = Settings::load("Settings.example").expect("example must parse");
        let base = settings
            .chains
            .iter()
            .find(|c| c.chain_id == "base")
            .expect("example has a base chain");
        assert!(base.aerodrome_v2.is_some());
        assert!(base.aerodrome_slipstream.is_some());
        let ethereum = settings
            .chains
            .iter()
            .find(|c| c.chain_id == "ethereum")
            .expect("example has an ethereum chain");
        assert!(ethereum.uniswap_v4.is_some());
    }

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
