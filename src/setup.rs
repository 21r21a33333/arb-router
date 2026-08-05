//! Wiring: turn [`Settings`] into a running service — a shared reader, store,
//! valuation, and notifiers, plus a sync worker and scanner per chain, all
//! behind the read API.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::Address;

use crate::adapters::api::{self, ApiState};
use crate::adapters::chain_reader::BlockReader;
use crate::adapters::exchanges::amm_rpc::AmmRpcExchange;
use crate::adapters::exchanges::{asset_address, core_asset};
use crate::adapters::notifier::{CompositeNotifier, LogNotifier, MemoryNotifier};
use crate::adapters::pool_store::{ArcSwapPoolStore, SyncWorker};
use crate::adapters::rpc::provider::{EthProvider, make_provider};
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
use crate::core::deps::valuation::Valuation;
use crate::primitives::asset::{AssetId, ChainId, Usd};
use crate::primitives::opportunity::Opportunity;
use crate::settings::{ChainSettings, CurveSettings, Settings, UniswapV4Settings};
use amm_core::protocols::uniswap::v4::Hooks;
use amm_rpc::protocols::aerodrome::AerodromeSource;
use amm_rpc::protocols::curve::{CurvePoolConfig, CurveSource};
use amm_rpc::protocols::slipstream::SlipstreamSource;
use amm_rpc::protocols::uniswap_v2::UniswapV2Source;
use amm_rpc::protocols::uniswap_v3::UniswapV3Source;
use amm_rpc::protocols::uniswap_v4::{UniswapV4Source, V4PoolConfig};
use curve_adapter::CurveVariant;

/// Wire everything from `settings`, spawn a sync + scan task per chain, and
/// serve the read API until the process ends.
pub async fn run(settings: Settings) -> eyre::Result<()> {
    let bind = settings.api_bind.clone();
    let state = build(settings)?;
    api::serve(&bind, state).await
}

/// A chain's constructed (not-yet-running) scanner.
type AppScanner = Scanner<ArcSwapPoolStore, CachedValuation, CompositeNotifier>;

/// The shared services every chain's worker + scanner draw on.
struct Services {
    reader: Arc<dyn ChainReader>,
    /// Per-chain providers the exchange sources read through (the reader keeps
    /// its own clones for block-height reads).
    providers: HashMap<ChainId, EthProvider>,
    store: Arc<ArcSwapPoolStore>,
    valuation: Arc<CachedValuation>,
    memory: Arc<MemoryNotifier>,
    notifier: Arc<CompositeNotifier>,
    /// Chains whose provider initialized (unbuildable chains are dropped).
    chains: Vec<ChainId>,
    registry: AssetRegistry,
}

/// Build the shared reader, store, valuation (price feeds spawned here), and
/// notifiers. A chain whose provider cannot be built is logged and skipped.
fn build_services(settings: &Settings) -> eyre::Result<Services> {
    let registry = settings.asset_registry()?;
    let (providers, chains) = build_providers(settings);
    // Exchange sources read through their own provider clones; the reader keeps a
    // set for block-height reads. `EthProvider` is reference-counted, cheap to clone.
    let providers_for_exchanges = providers.clone();
    let reader: Arc<dyn ChainReader> = Arc::new(BlockReader::new(providers));
    let store = Arc::new(ArcSwapPoolStore::new(&chains));
    let valuation = build_valuation(settings)?;
    // Log every opportunity and keep the latest set for the API / one-shot report.
    let memory = Arc::new(MemoryNotifier::new());
    let notifier = Arc::new(CompositeNotifier(vec![
        Arc::new(LogNotifier) as Arc<dyn Notifier>,
        memory.clone(),
    ]));
    Ok(Services {
        reader,
        providers: providers_for_exchanges,
        store,
        valuation,
        memory,
        notifier,
        chains,
        registry,
    })
}

/// Construct one chain's sync worker + scanner (without spawning any loop).
fn build_chain(c: &ChainSettings, s: &Services) -> eyre::Result<(SyncWorker, Arc<AppScanner>)> {
    let chain = ChainId::new(&c.chain_id);
    let worker = SyncWorker {
        chain: chain.clone(),
        store: s.store.clone(),
        exchanges: build_exchanges(&chain, c, &s.registry, s.providers.get(&chain).cloned()),
        reader: s.reader.clone(),
        tracked_tokens: parse_assets(&c.tracked_tokens)?,
        interval: Duration::from_millis(c.sync_interval_ms),
    };
    let scanner = Arc::new(Scanner {
        chain,
        pool_store: s.store.clone(),
        valuation: s.valuation.clone(),
        notifier: s.notifier.clone(),
        registry: s.registry.clone(),
        cfg: EngineConfig {
            start_assets: parse_assets(&c.start_assets)?,
            max_hops: c.max_hops,
            input_usd: Usd(c.input_usd),
        },
    });
    Ok((worker, scanner))
}

/// Build shared services and spawn a sync + scan loop per chain, returning the
/// API state (used by the long-running `run`).
pub fn build(settings: Settings) -> eyre::Result<ApiState> {
    let services = build_services(&settings)?;
    for c in &settings.chains {
        if !services.chains.contains(&ChainId::new(&c.chain_id)) {
            continue; // provider failed to build
        }
        let (worker, scanner) = build_chain(c, &services)?;
        tokio::spawn(worker.run());
        tokio::spawn(scanner.run(Duration::from_millis(c.scan_interval_ms)));
    }
    Ok(ApiState {
        memory: services.memory,
        store: services.store as Arc<dyn PoolStore>,
        chains: services.chains,
    })
}

/// Build one HTTP provider per chain, skipping any whose init fails.
fn build_providers(settings: &Settings) -> (HashMap<ChainId, EthProvider>, Vec<ChainId>) {
    let mut providers = HashMap::new();
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
        chains.push(chain);
    }
    (providers, chains)
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
            settings.coingecko_api_key.clone(),
        )
        .run(),
    );

    // Binance covers the assets that name a symbol — one symbol fans out to
    // every asset that shares it (e.g. WETH on Ethereum, Base, and Arbitrum).
    let mut symbols: HashMap<String, Vec<AssetId>> = HashMap::new();
    for asset in &settings.assets {
        if let Some(symbol) = &asset.binance_symbol {
            symbols
                .entry(symbol.to_uppercase())
                .or_default()
                .push(AssetId::new(&asset.id)?);
        }
    }
    let ws_base = settings
        .binance_ws_url
        .clone()
        .unwrap_or_else(|| "wss://stream.binance.com:9443".to_string());
    tokio::spawn(BinanceFeed::new(store.clone(), ws_base, symbols).run());

    Ok(Arc::new(CachedValuation::new(store)))
}

/// Sync + scan **every chain once**, concurrently, then print every detected
/// arbitrage opportunity and return. A one-shot scan — no long-running loops and
/// no API server.
pub async fn run_once(settings: Settings) -> eyre::Result<()> {
    let services = build_services(&settings)?;
    let chains: Vec<(SyncWorker, Arc<AppScanner>)> = settings
        .chains
        .iter()
        .filter(|c| services.chains.contains(&ChainId::new(&c.chain_id)))
        .map(|c| build_chain(c, &services))
        .collect::<eyre::Result<_>>()?;

    // Scans need USD prices to size and value paths, so wait for the feeds
    // (Binance primary), then show which oracle supplied each price.
    warm_prices(&services, &settings).await;
    report_prices(&services, &settings);

    // Every chain: sync once, then scan once — all chains running concurrently.
    let ticks = chains.iter().map(|(worker, scanner)| async move {
        let chain = worker.chain.as_str().to_string();
        match worker.refresh_once().await {
            Ok(block) => tracing::info!(chain = %chain, block, "synced"),
            Err(err) => {
                tracing::warn!(chain = %chain, error = %err, "sync failed");
                return;
            }
        }
        match scanner.scan_once().await {
            Ok(count) => tracing::info!(chain = %chain, opportunities = count, "scanned"),
            Err(err) => tracing::warn!(chain = %chain, error = %err, "scan failed"),
        }
    });
    futures_util::future::join_all(ticks).await;

    report_opportunities(&services.memory.all());
    Ok(())
}

/// Wait for the price feeds to warm: every start asset priceable, and — since
/// Binance is the primary oracle — every start asset that names a Binance symbol
/// actually sourced from Binance (not the CoinGecko fallback), up to a deadline.
async fn warm_prices(services: &Services, settings: &Settings) {
    let needed: std::collections::HashSet<AssetId> = settings
        .chains
        .iter()
        .flat_map(|c| c.start_assets.iter())
        .filter_map(|a| AssetId::new(a).ok())
        .collect();
    // Start assets that should be priced live by Binance.
    let want_binance: std::collections::HashSet<AssetId> = settings
        .assets
        .iter()
        .filter(|a| a.binance_symbol.is_some())
        .filter_map(|a| AssetId::new(&a.id).ok())
        .filter(|id| needed.contains(id))
        .collect();

    let deadline = std::time::Instant::now() + Duration::from_secs(25);
    loop {
        let mut priced = true;
        for asset in &needed {
            if services.valuation.price(asset).await.is_err() {
                priced = false;
            }
        }
        let binance_ready = want_binance
            .iter()
            .all(|a| matches!(services.valuation.price_and_source(a), Some((_, "binance"))));

        if priced && binance_ready {
            tracing::info!("price feeds warm (binance live)");
            return;
        }
        if std::time::Instant::now() >= deadline {
            tracing::warn!(
                binance_ready,
                "warm timed out; scanning with available prices"
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// Print the oracle price + which feed supplied it for every configured asset.
fn report_prices(services: &Services, settings: &Settings) {
    println!("\n──────── oracle prices ────────");
    for asset in &settings.assets {
        let Ok(id) = AssetId::new(&asset.id) else {
            continue;
        };
        let chain = asset.id.split(':').next().unwrap_or("?");
        match services.valuation.price_and_source(&id) {
            Some((usd, source)) => {
                println!("  {:>8} ${:<15} [{source:<9}] {chain}", asset.symbol, usd.0)
            }
            None => println!("  {:>8} (unpriced)             {chain}", asset.symbol),
        }
    }
}

/// Print every detected opportunity as a compact report.
fn report_opportunities(opps: &[Opportunity]) {
    let plural = if opps.len() == 1 { "y" } else { "ies" };
    println!(
        "\n════════ arbitrage scan complete: {} opportunit{plural} ════════",
        opps.len()
    );
    for o in opps {
        let route = match o.path.is_cycle() {
            true => format!("{} ↺ cycle", o.path.start.as_str()),
            false => format!(
                "{} → {}",
                o.path.start.as_str(),
                o.path.destination().as_str()
            ),
        };
        let profit = o
            .profit_usd
            .map(|p| format!("${}", p.0))
            .unwrap_or_else(|| "?".into());
        println!(
            "  [{}] {route}  in {} out {}  profit {profit}  {} bps",
            o.chain.as_str(),
            o.input.0,
            o.output.0,
            o.roi_bps
        );
    }
    println!();
}

/// The exchanges configured on a chain, each backed by an `amm-rpc` source.
fn build_exchanges(
    chain: &ChainId,
    c: &ChainSettings,
    registry: &AssetRegistry,
    provider: Option<EthProvider>,
) -> Vec<Arc<dyn Exchange>> {
    let Some(provider) = provider else {
        return Vec::new();
    };
    let mut exchanges: Vec<Arc<dyn Exchange>> = Vec::new();

    if let Some(v3) = &c.uniswap_v3 {
        match v3.factory.parse::<Address>() {
            Ok(factory) => exchanges.push(Arc::new(AmmRpcExchange::new(
                "uniswap_v3",
                chain.clone(),
                UniswapV3Source::with_factory(provider.clone(), factory, v3.fee_tiers.clone()),
            ))),
            Err(_) => {
                tracing::warn!(chain = chain.as_str(), factory = %v3.factory, "invalid uniswap_v3 factory address")
            }
        }
    }

    if let Some(v2) = &c.uniswap_v2 {
        match v2.factory.parse::<Address>() {
            Ok(factory) => exchanges.push(Arc::new(AmmRpcExchange::new(
                "uniswap_v2",
                chain.clone(),
                UniswapV2Source::with_factory(provider.clone(), factory, v2.fee_bps),
            ))),
            Err(_) => {
                tracing::warn!(chain = chain.as_str(), factory = %v2.factory, "invalid uniswap_v2 factory address")
            }
        }
    }

    if let Some(v4) = &c.uniswap_v4
        && let Some(exchange) = build_uniswap_v4(chain, v4, registry, provider.clone())
    {
        exchanges.push(exchange);
    }

    if let Some(curve) = &c.curve
        && let Some(exchange) = build_curve(chain, curve, registry, provider.clone())
    {
        exchanges.push(exchange);
    }

    if let Some(aero) = &c.aerodrome_v2 {
        match aero.factory.parse::<Address>() {
            Ok(factory) => exchanges.push(Arc::new(AmmRpcExchange::new(
                "aerodrome_v2",
                chain.clone(),
                AerodromeSource::new(provider.clone(), factory),
            ))),
            Err(_) => {
                tracing::warn!(chain = chain.as_str(), factory = %aero.factory, "invalid aerodrome_v2 factory address")
            }
        }
    }

    if let Some(slip) = &c.aerodrome_slipstream {
        match slip.factory.parse::<Address>() {
            Ok(factory) => exchanges.push(Arc::new(AmmRpcExchange::new(
                "aerodrome_slipstream",
                chain.clone(),
                SlipstreamSource::with_factory(
                    provider.clone(),
                    factory,
                    slip.tick_spacings.clone(),
                ),
            ))),
            Err(_) => {
                tracing::warn!(chain = chain.as_str(), factory = %slip.factory, "invalid aerodrome_slipstream factory address")
            }
        }
    }

    exchanges
}

/// Build the Uniswap V4 exchange from configured pools, resolving each pool's
/// currencies against the registry and sorting them so `currency0 < currency1`
/// before the pool id is derived. Pools that don't resolve are skipped; an
/// invalid `PoolManager` address skips the whole exchange.
fn build_uniswap_v4(
    chain: &ChainId,
    v4: &UniswapV4Settings,
    registry: &AssetRegistry,
    provider: EthProvider,
) -> Option<Arc<dyn Exchange>> {
    let pool_manager = match v4.pool_manager.parse::<Address>() {
        Ok(addr) => addr,
        Err(_) => {
            tracing::warn!(chain = chain.as_str(), pool_manager = %v4.pool_manager, "invalid uniswap_v4 pool_manager address");
            return None;
        }
    };

    let mut pools = Vec::new();
    for pool in &v4.pools {
        let Some(config) = resolve_v4_pool(chain, pool, registry) else {
            continue;
        };
        pools.push(config);
    }
    match pools.is_empty() {
        true => None,
        false => Some(Arc::new(AmmRpcExchange::new(
            "uniswap_v4",
            chain.clone(),
            UniswapV4Source::new(provider, pool_manager, pools),
        ))),
    }
}

/// Resolve one configured V4 pool into an amm-rpc [`V4PoolConfig`], or `None`
/// (with a warning) if a coin, address, or hooks value doesn't resolve. Hooks
/// are treated as static (`Hooks::None`); the hook address still feeds the pool
/// id derivation.
fn resolve_v4_pool(
    chain: &ChainId,
    pool: &crate::settings::V4PoolSettings,
    registry: &AssetRegistry,
) -> Option<V4PoolConfig> {
    let (Ok(a0), Ok(a1)) = (AssetId::new(&pool.coin0), AssetId::new(&pool.coin1)) else {
        tracing::warn!(chain = chain.as_str(), "invalid uniswap_v4 coin id");
        return None;
    };
    if registry.get(&a0).is_none() || registry.get(&a1).is_none() {
        tracing::warn!(
            chain = chain.as_str(),
            "uniswap_v4 pool coins missing from registry"
        );
        return None;
    }
    let (Ok(addr0), Ok(addr1)) = (asset_address(&a0), asset_address(&a1)) else {
        tracing::warn!(
            chain = chain.as_str(),
            "uniswap_v4 coin is not a chain:0x… address"
        );
        return None;
    };
    let (Some(t0), Some(t1)) = (core_asset(&a0), core_asset(&a1)) else {
        return None;
    };
    let hooks = match &pool.hooks {
        Some(h) => match h.parse::<Address>() {
            Ok(addr) => addr,
            Err(_) => {
                tracing::warn!(chain = chain.as_str(), hooks = %h, "invalid uniswap_v4 hooks address");
                return None;
            }
        },
        None => Address::ZERO,
    };

    // V4 orders currencies by address; keep the asset ids paired with them.
    let ((c0, ct0), (c1, ct1)) = match addr0 < addr1 {
        true => ((addr0, t0), (addr1, t1)),
        false => ((addr1, t1), (addr0, t0)),
    };
    Some(V4PoolConfig::new(
        c0,
        c1,
        ct0,
        ct1,
        pool.fee,
        pool.tick_spacing,
        hooks,
        Hooks::None,
    ))
}

/// Build the Curve exchange from configured pools, resolving each pool's coins
/// and decimals against the asset registry. Pools that don't resolve are skipped.
fn build_curve(
    chain: &ChainId,
    curve: &CurveSettings,
    registry: &AssetRegistry,
    provider: EthProvider,
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
        // Meta pools need a valid base-pool address; a malformed one is dropped.
        let base_pool = match &pool.base_pool {
            Some(addr) => match addr.parse::<Address>() {
                Ok(parsed) => Some(parsed),
                Err(_) => {
                    tracing::warn!(chain = chain.as_str(), base_pool = %addr, "invalid curve base_pool address");
                    continue;
                }
            },
            None => None,
        };
        pools.push(CurvePoolConfig {
            address,
            variant,
            coins,
            decimals,
            base_pool,
            eth_variant: pool.eth_variant,
        });
    }
    match pools.is_empty() {
        true => None,
        false => Some(Arc::new(AmmRpcExchange::new(
            "curve",
            chain.clone(),
            CurveSource::new(provider, pools),
        ))),
    }
}

/// Resolve each coin id to an amm-core `AssetId` and its decimals, or `None` if
/// any coin is unknown to the registry.
fn resolve_coins(
    ids: &[String],
    registry: &AssetRegistry,
) -> Option<(Vec<amm_core::primitives::asset::AssetId>, Vec<u8>)> {
    let mut coins = Vec::new();
    let mut decimals = Vec::new();
    for id in ids {
        let coin = AssetId::new(id).ok()?;
        decimals.push(registry.get(&coin)?.decimals);
        coins.push(core_asset(&coin)?);
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

    /// V4 pool resolution sorts currencies by address regardless of config order,
    /// so `currency0 < currency1` before the pool id is derived.
    #[test]
    fn resolve_v4_pool_sorts_currencies() {
        use crate::adapters::exchanges::core_asset;
        use crate::primitives::asset::AssetMeta;
        use crate::settings::V4PoolSettings;

        let low = "ethereum:0x0000000000000000000000000000000000000001";
        let high = "ethereum:0x0000000000000000000000000000000000000002";
        let mut registry = AssetRegistry::new();
        registry.insert(
            AssetId::new(low).unwrap(),
            AssetMeta {
                decimals: 6,
                symbol: "LOW".into(),
            },
        );
        registry.insert(
            AssetId::new(high).unwrap(),
            AssetMeta {
                decimals: 18,
                symbol: "HIGH".into(),
            },
        );

        // Config lists them high-then-low; resolution must still sort low first.
        let pool = V4PoolSettings {
            coin0: high.to_string(),
            coin1: low.to_string(),
            fee: 500,
            tick_spacing: 10,
            hooks: None,
        };
        let chain = ChainId::new("ethereum");
        let config = resolve_v4_pool(&chain, &pool, &registry).unwrap();

        assert_eq!(
            config.token0,
            core_asset(&AssetId::new(low).unwrap()).unwrap()
        );
        assert_eq!(
            config.token1,
            core_asset(&AssetId::new(high).unwrap()).unwrap()
        );
        assert_eq!(config.fee, 500);
        assert_eq!(config.tick_spacing, 10);
    }
}
