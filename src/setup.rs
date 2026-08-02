//! Wiring: turn [`Settings`] into a running service — a shared reader, store,
//! valuation, and notifiers, plus a sync worker and scanner per chain, all
//! behind the read API.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::Address;

use crate::adapters::api::{self, ApiState};
use crate::adapters::chain_reader::MulticallChainReader;
use crate::adapters::exchanges::curve::exchange::{CurveExchange, CurvePoolConfig};
use crate::adapters::exchanges::uniswap::v2_exchange::UniswapV2Exchange;
use crate::adapters::exchanges::uniswap::v3_exchange::UniswapV3Exchange;
use crate::adapters::notifier::{CompositeNotifier, LogNotifier, MemoryNotifier};
use crate::adapters::pool_store::{ArcSwapPoolStore, SyncWorker};
use crate::adapters::rpc::provider::make_provider;
use crate::adapters::valuation::binance::BinanceFeed;
use crate::adapters::valuation::coingecko::CoinGeckoFeed;
use crate::adapters::valuation::{CachedValuation, PriceStore};
use crate::core::application::config::EngineConfig;
use crate::core::application::evaluation::detect::AssetRegistry;
use crate::core::application::scanner::Scanner;
use crate::core::deps::chain_reader::ChainReader;
use crate::core::deps::exchange::Exchange;
use crate::core::deps::notifier::Notifier;
use crate::core::deps::pool_store::PoolStore;
use crate::primitives::asset::{AssetId, ChainId, Usd};
use crate::settings::{ChainSettings, CurveSettings, Settings};
use curve_adapter::CurveVariant;

/// Calls per Multicall3 round trip.
const CHUNK_SIZE: usize = 50;

/// Wire everything from `settings`, spawn a sync + scan task per chain, and
/// serve the read API until the process ends.
pub async fn run(settings: Settings) -> eyre::Result<()> {
    let bind = settings.api_bind.clone();
    let state = build(settings)?;
    api::serve(&bind, state).await
}

/// Build the shared services and spawn per-chain sync/scan tasks, returning the
/// API state. A chain whose provider cannot be built is logged and skipped.
pub fn build(settings: Settings) -> eyre::Result<ApiState> {
    let registry = settings.asset_registry()?;

    // Shared reader over every chain that initialized.
    let (providers, overrides, chains) = build_providers(&settings);
    let reader: Arc<dyn ChainReader> =
        Arc::new(MulticallChainReader::new(providers, CHUNK_SIZE).with_overrides(overrides));

    let store = Arc::new(ArcSwapPoolStore::new(&chains));
    let valuation = build_valuation(&settings)?;

    // Log every opportunity and keep the latest set for the API.
    let memory = Arc::new(MemoryNotifier::new());
    let notifier = Arc::new(CompositeNotifier(vec![
        Arc::new(LogNotifier) as Arc<dyn Notifier>,
        memory.clone(),
    ]));

    for chain_settings in &settings.chains {
        let chain = ChainId::new(&chain_settings.chain_id);
        if !chains.contains(&chain) {
            continue; // provider failed to build
        }
        spawn_chain(
            chain,
            chain_settings,
            &store,
            &reader,
            &valuation,
            &notifier,
            &registry,
        )?;
    }

    Ok(ApiState {
        memory,
        store: store as Arc<dyn PoolStore>,
        chains,
    })
}

/// Build one HTTP provider per chain (skipping failures) plus any Multicall3
/// address overrides.
fn build_providers(
    settings: &Settings,
) -> (
    HashMap<ChainId, crate::adapters::rpc::provider::EthProvider>,
    HashMap<ChainId, Address>,
    Vec<ChainId>,
) {
    let mut providers = HashMap::new();
    let mut overrides = HashMap::new();
    let mut chains = Vec::new();
    for c in &settings.chains {
        let chain = ChainId::new(&c.chain_id);
        match make_provider(&c.rpc_url) {
            Ok(provider) => {
                providers.insert(chain.clone(), provider);
            }
            Err(err) => {
                tracing::warn!(chain = chain.as_str(), error = %err, "skipping chain: provider init failed");
                continue;
            }
        }
        if let Some(addr) = &c.multicall_address
            && let Ok(address) = addr.parse::<Address>()
        {
            overrides.insert(chain.clone(), address);
        }
        chains.push(chain);
    }
    (providers, overrides, chains)
}

/// Build the price store, spawn the Binance + CoinGecko feeds, and return the
/// valuation that reads the store.
fn build_valuation(settings: &Settings) -> eyre::Result<Arc<CachedValuation>> {
    let staleness = Duration::from_secs(settings.price_staleness_secs.unwrap_or(120));
    let store = Arc::new(PriceStore::new(staleness));

    // CoinGecko covers every priced asset.
    let mut price_ids = HashMap::new();
    for asset in &settings.assets {
        price_ids.insert(AssetId::new(&asset.id)?, asset.price_id.clone());
    }
    let refresh = Duration::from_secs(settings.price_refresh_secs.unwrap_or(30));
    tokio::spawn(
        CoinGeckoFeed::new(
            store.clone(),
            settings.fiat_provider_url.clone(),
            price_ids,
            refresh,
        )
        .run(),
    );

    // Binance covers the assets that name a symbol.
    let mut symbols = HashMap::new();
    for asset in &settings.assets {
        if let Some(symbol) = &asset.binance_symbol {
            symbols.insert(symbol.to_uppercase(), AssetId::new(&asset.id)?);
        }
    }
    let ws_base = settings
        .binance_ws_url
        .clone()
        .unwrap_or_else(|| "wss://stream.binance.com:9443".to_string());
    tokio::spawn(BinanceFeed::new(store.clone(), ws_base, symbols).run());

    Ok(Arc::new(CachedValuation::new(store)))
}

/// Spawn one chain's sync worker and scanner on background tasks.
fn spawn_chain(
    chain: ChainId,
    c: &ChainSettings,
    store: &Arc<ArcSwapPoolStore>,
    reader: &Arc<dyn ChainReader>,
    valuation: &Arc<CachedValuation>,
    notifier: &Arc<CompositeNotifier>,
    registry: &AssetRegistry,
) -> eyre::Result<()> {
    let worker = SyncWorker {
        chain: chain.clone(),
        store: store.clone(),
        exchanges: build_exchanges(&chain, c, registry),
        reader: reader.clone(),
        tracked_tokens: parse_assets(&c.tracked_tokens)?,
        interval: Duration::from_millis(c.sync_interval_ms),
    };
    tokio::spawn(worker.run());

    let scanner = Arc::new(Scanner {
        chain,
        pool_store: store.clone(),
        valuation: valuation.clone(),
        notifier: notifier.clone(),
        registry: registry.clone(),
        cfg: EngineConfig {
            start_assets: parse_assets(&c.start_assets)?,
            max_hops: c.max_hops,
            input_usd: Usd(c.input_usd),
        },
    });
    tokio::spawn(scanner.run(Duration::from_millis(c.scan_interval_ms)));
    Ok(())
}

/// The exchanges configured on a chain.
fn build_exchanges(
    chain: &ChainId,
    c: &ChainSettings,
    registry: &AssetRegistry,
) -> Vec<Arc<dyn Exchange>> {
    let mut exchanges: Vec<Arc<dyn Exchange>> = Vec::new();

    if let Some(v3) = &c.uniswap_v3 {
        match v3.factory.parse::<Address>() {
            Ok(factory) => exchanges.push(Arc::new(UniswapV3Exchange::new(
                "uniswap_v3",
                chain.clone(),
                factory,
                v3.fee_tiers.clone(),
            ))),
            Err(_) => {
                tracing::warn!(chain = chain.as_str(), factory = %v3.factory, "invalid uniswap_v3 factory address")
            }
        }
    }

    if let Some(v2) = &c.uniswap_v2 {
        match v2.factory.parse::<Address>() {
            Ok(factory) => exchanges.push(Arc::new(UniswapV2Exchange::new(
                "uniswap_v2",
                chain.clone(),
                factory,
                v2.fee_bps,
            ))),
            Err(_) => {
                tracing::warn!(chain = chain.as_str(), factory = %v2.factory, "invalid uniswap_v2 factory address")
            }
        }
    }

    if let Some(curve) = &c.curve
        && let Some(exchange) = build_curve(chain, curve, registry)
    {
        exchanges.push(exchange);
    }

    exchanges
}

/// Build the Curve exchange from configured pools, resolving each pool's coins
/// and decimals against the asset registry. Pools that don't resolve are skipped.
fn build_curve(
    chain: &ChainId,
    curve: &CurveSettings,
    registry: &AssetRegistry,
) -> Option<Arc<dyn Exchange>> {
    let mut pools = Vec::new();
    for pool in &curve.pools {
        let Ok(address) = pool.address.parse::<Address>() else {
            tracing::warn!(chain = chain.as_str(), address = %pool.address, "invalid curve pool address");
            continue;
        };
        let Ok(variant) = pool.variant.parse::<CurveVariant>() else {
            tracing::warn!(chain = chain.as_str(), variant = %pool.variant, "unknown curve variant");
            continue;
        };
        let Some((coins, decimals)) = resolve_coins(&pool.coins, registry) else {
            tracing::warn!(chain = chain.as_str(), address = %pool.address, "curve pool coins missing from registry");
            continue;
        };
        pools.push(CurvePoolConfig {
            address,
            variant,
            coins,
            decimals,
        });
    }
    match pools.is_empty() {
        true => None,
        false => Some(Arc::new(CurveExchange::new("curve", chain.clone(), pools))),
    }
}

/// Resolve each coin id to an `AssetId` and its decimals, or `None` if any coin
/// is unknown to the registry.
fn resolve_coins(ids: &[String], registry: &AssetRegistry) -> Option<(Vec<AssetId>, Vec<u8>)> {
    let mut coins = Vec::new();
    let mut decimals = Vec::new();
    for id in ids {
        let coin = AssetId::new(id).ok()?;
        decimals.push(registry.get(&coin)?.decimals);
        coins.push(coin);
    }
    Some((coins, decimals))
}

/// Parse a list of namespaced asset ids.
fn parse_assets(ids: &[String]) -> eyre::Result<Vec<AssetId>> {
    ids.iter()
        .map(|s| AssetId::new(s))
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        api_bind = "127.0.0.1:0"
        fiat_provider_url = "http://127.0.0.1:1"

        [[assets]]
        id = "ethereum:0x0000000000000000000000000000000000000001"
        decimals = 6
        symbol = "USDC"
        price_id = "usd-coin"

        [[chains]]
        chain_id = "ethereum"
        rpc_url = "http://127.0.0.1:1"
        sync_interval_ms = 60000
        scan_interval_ms = 60000
        max_hops = 4
        input_usd = "1000"
        start_assets = ["ethereum:0x0000000000000000000000000000000000000001"]
        tracked_tokens = ["ethereum:0x0000000000000000000000000000000000000001"]

        [chains.uniswap_v3]
        factory = "0x1F98431c8aD98523631AE4a59f267346ea31F984"
        fee_tiers = [500, 3000]
    "#;

    /// The wiring builds, spawns its tasks, and the API answers — even though
    /// the (bogus) RPC will never connect.
    #[tokio::test]
    async fn build_wires_and_serves_health() {
        let settings = Settings::from_toml(SAMPLE).unwrap();
        let state = build(settings).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, api::router(state)).await.unwrap() });

        let health: serde_json::Value = reqwest::Client::new()
            .get(format!("http://{addr}/v1/health"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(health["status"], "ok");
    }

    const SAMPLE_MULTI: &str = r#"
        api_bind = "127.0.0.1:0"
        fiat_provider_url = "http://127.0.0.1:1"

        [[assets]]
        id = "ethereum:0x0000000000000000000000000000000000000001"
        decimals = 6
        symbol = "USDC"
        price_id = "usd-coin"

        [[chains]]
        chain_id = "ethereum"
        rpc_url = "http://127.0.0.1:1"
        sync_interval_ms = 60000
        scan_interval_ms = 60000
        max_hops = 4
        input_usd = "1000"
        start_assets = ["ethereum:0x0000000000000000000000000000000000000001"]
        tracked_tokens = ["ethereum:0x0000000000000000000000000000000000000001"]

        [[chains]]
        chain_id = "arbitrum"
        rpc_url = "http://127.0.0.1:1"
        sync_interval_ms = 60000
        scan_interval_ms = 60000
        max_hops = 4
        input_usd = "1000"
        start_assets = ["arbitrum:0x0000000000000000000000000000000000000001"]
        tracked_tokens = ["arbitrum:0x0000000000000000000000000000000000000001"]
    "#;

    /// Two chains wire two independent sync+scan stacks.
    #[tokio::test]
    async fn build_wires_multiple_chains() {
        let settings = Settings::from_toml(SAMPLE_MULTI).unwrap();
        let state = build(settings).unwrap();
        assert_eq!(state.chains.len(), 2);
    }
}
