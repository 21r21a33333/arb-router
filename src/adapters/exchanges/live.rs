//! Live mainnet **differential** tests — our locally-computed quote vs the
//! contract's own quote function, wei-exact, at a pinned block. `#[ignore]`d
//! and opt-in.
//!
//! Every supported pool kind is covered: Uniswap V2 / V3 / V4, Curve (all 12
//! `curve-math` variants), and Aerodrome (v2 volatile + Solidly stable +
//! Slipstream). Each test follows the same shape:
//!
//! 1. pin the latest block `B`,
//! 2. refresh our pool state at `B` and quote locally,
//! 3. call the pool's own quote fn (`get_dy` / `getAmountOut` / a Quoter) at
//!    the **same** block `B` through the same reader,
//! 4. assert the two agree — to the wei for constant-product / stableswap, and
//!    within a few ppm for concentrated liquidity (our bounded tick window).
//!
//! Same-block pinning is essential: prices move between blocks, so an unpinned
//! comparison would spuriously differ. This is the end-to-end proof that our
//! decoders + quote math match the deployed contracts; the offline tests prove
//! the same math deterministically without network.
//!
//! Run: `cargo test --lib -- --ignored diff_ --nocapture`
//! (public RPCs are used by default; override with `ETH_RPC_URL` / `BASE_RPC_URL`.)

#![cfg(test)]

use std::collections::HashMap;

use alloy::primitives::{Address, U256, address};
use alloy::sol;
use alloy::sol_types::SolCall;
use curve_adapter::CurveVariant;
use rust_decimal::Decimal;

use crate::adapters::chain_reader::MulticallChainReader;
use crate::adapters::exchanges::aerodrome::slipstream_exchange::SlipstreamExchange;
use crate::adapters::exchanges::aerodrome::v2_exchange::AerodromeV2Exchange;
use crate::adapters::exchanges::curve::exchange::{CurveExchange, CurvePoolConfig};
use crate::adapters::exchanges::uniswap::v2_exchange::UniswapV2Exchange;
use crate::adapters::exchanges::uniswap::v3_exchange::UniswapV3Exchange;
use crate::adapters::exchanges::uniswap::v4_exchange::{UniswapV4Exchange, V4PoolConfig};
use crate::adapters::exchanges::{amount_to_u256, call};
use crate::core::deps::chain_reader::ChainReader;
use crate::core::deps::exchange::Exchange;
use crate::primitives::asset::{Amount, AssetId, ChainId, Pair};
use crate::primitives::chain::BlockId;
use crate::test_utils::{BASE_RPC_DEFAULT, ETH_RPC_DEFAULT, live_reader};

// ─── token addresses ──────────────────────────────────────────────────────────

// Ethereum.
const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
const DAI: &str = "0x6b175474e89094c44da98b954eedeac495271d0f";
const USDT: &str = "0xdac17f958d2ee523a2206206994597c13d831ec7";
const WBTC: &str = "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599";
const CRVUSD: &str = "0xf939e0a03fb07f59a73314e73794be0e57ac1b4e";
const CRV: &str = "0xD533a949740bb3306d119CC777fa900bA034cd52";
const CBBTC: &str = "0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf";
const STETH: &str = "0xae7ab96520de3a18e5e111b5eaab095312d7fe84";
const ETH: &str = "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"; // Curve native-ETH placeholder
const MIM: &str = "0x99d8a9c45b2eca8864373a26d1459e3dff1e17f3";
const THREE_CRV: &str = "0x6c3f90f043a72fa612cbac8115ee7e52bde6e490";
const SUSD: &str = "0x57Ab1ec28D129707052df4dF418D58a2D46d5f51";
const FRAX: &str = "0x853d955aCEf822Db058eb8505911ED77F175b99e";
const ADAI: &str = "0x028171bCA77440897B824Ca71D1c56caC55b68A3";
const AUSDC: &str = "0xBcca60bB61934080951369a648Fb03DF4F96263C";
const AUSDT: &str = "0x3Ed3B47Dd13EC9a98b44e6204A523E766B225811";
const TC_NG_TOKEN: &str = "0x1cfa5641c01406ab8ac350ded7d735ec41298372"; // a WETH/x TwoCryptoNG

// Base.
const BASE_USDC: &str = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913";
const BASE_WETH: &str = "0x4200000000000000000000000000000000000006";
const BASE_USDBC: &str = "0xd9aaEC86B65D86f6A7B5B1b0c42FFA531710b6CA";

const AERO_FACTORY: Address = address!("0x420DD381b31aEf6683db6B902084cB0FFECe40Da");

// ─── shared harness ────────────────────────────────────────────────────────────.

fn aid(chain: &str, addr: &str) -> AssetId {
    AssetId::new(&format!("{chain}:{addr}")).unwrap()
}
fn eth_asset(addr: &str) -> AssetId {
    aid("ethereum", addr)
}
fn addr(hex: &str) -> Address {
    hex.parse().expect("address")
}
fn pair(chain: &str, src: &str, dst: &str) -> Pair {
    Pair {
        source: aid(chain, src),
        destination: aid(chain, dst),
    }
}

fn eth_reader() -> (MulticallChainReader, ChainId) {
    live_reader("ethereum", "ETH_RPC_URL", ETH_RPC_DEFAULT)
}
fn base_reader() -> (MulticallChainReader, ChainId) {
    live_reader("base", "BASE_RPC_URL", BASE_RPC_DEFAULT)
}

/// The raw return bytes of a single on-chain call at `block`, routed through the
/// same Multicall reader our state reads use (so it observes the same block).
async fn onchain_return(
    reader: &MulticallChainReader,
    chain: &ChainId,
    block: u64,
    target: Address,
    calldata: Vec<u8>,
) -> Vec<u8> {
    let out = reader
        .call_batch(chain, BlockId::Number(block), vec![call(target, calldata)])
        .await
        .expect("on-chain call");
    let result = &out.results[0];
    assert!(result.success, "on-chain quote reverted");
    result.data.0.clone()
}

/// Our `Amount` as a base-unit `U256`, for comparison against the contract.
fn as_u256(amount: Amount) -> U256 {
    amount_to_u256(amount).expect("quote fits u256")
}

/// Assert our quote equals the contract's, to the wei.
fn assert_eq_wei(label: &str, ours: Amount, theirs: U256) {
    let ours = as_u256(ours);
    println!("{label}: local {ours} vs contract {theirs}");
    assert_eq!(ours, theirs, "{label}: local != on-chain");
}

/// Assert our quote agrees with the contract within `max_ppm` — for the
/// concentrated-liquidity quoters, where our bounded tick window can diverge by
/// a few ppm on larger swaps (it is 0 for in-window sizes).
fn assert_close_ppm(label: &str, ours: Amount, theirs: U256, max_ppm: u64) {
    let ours = as_u256(ours);
    let (hi, lo) = (ours.max(theirs), ours.min(theirs));
    let ppm = match theirs.is_zero() {
        true => U256::ZERO,
        false => (hi - lo) * U256::from(1_000_000) / theirs,
    };
    println!("{label}: local {ours} vs contract {theirs} ({ppm} ppm)");
    assert!(ppm <= U256::from(max_ppm), "{label}: {ppm} ppm > {max_ppm}");
}

sol! {
    // Curve: stableswap variants index coins by int128, cryptoswap by uint256.
    interface ICurveGetDyInt {
        function get_dy(int128 i, int128 j, uint256 dx) external view returns (uint256);
    }
    interface ICurveGetDyUint {
        function get_dy(uint256 i, uint256 j, uint256 dx) external view returns (uint256);
    }
    // Aerodrome pools expose their own quote directly.
    interface IAeroQuote {
        function getAmountOut(uint256 amountIn, address tokenIn) external view returns (uint256);
    }
    // Uniswap V2 pairs have no quote fn — use the Router's library math.
    interface IUniV2Router {
        function getAmountsOut(uint256 amountIn, address[] path) external view returns (uint256[] memory amounts);
    }
    // Uniswap V3 QuoterV2 (keyed by fee).
    interface IUniV3Quoter {
        struct QuoteExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint256 amountIn;
            uint24 fee;
            uint160 sqrtPriceLimitX96;
        }
        function quoteExactInputSingle(QuoteExactInputSingleParams params)
            external
            returns (uint256 amountOut, uint160 sqrtPriceX96After, uint32 initializedTicksCrossed, uint256 gasEstimate);
    }
    // Uniswap V4 Quoter (keyed by the full pool key). Nonpayable, returns via eth_call.
    interface IV4Quoter {
        struct PoolKey {
            address currency0;
            address currency1;
            uint24 fee;
            int24 tickSpacing;
            address hooks;
        }
        struct QuoteExactSingleParams {
            PoolKey poolKey;
            bool zeroForOne;
            uint128 exactAmount;
            bytes hookData;
        }
        function quoteExactInputSingle(QuoteExactSingleParams params)
            external
            returns (uint256 amountOut, uint256 gasEstimate);
    }
    // Aerodrome Slipstream Quoter — QuoterV2-shaped but keyed by tickSpacing.
    interface ISlipstreamQuoter {
        struct QuoteExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint256 amountIn;
            int24 tickSpacing;
            uint160 sqrtPriceLimitX96;
        }
        function quoteExactInputSingle(QuoteExactInputSingleParams params)
            external
            returns (uint256 amountOut, uint160 sqrtPriceX96After, uint32 initializedTicksCrossed, uint256 gasEstimate);
    }
}

// ─── Curve: one differential per variant against a real mainnet pool ────────────

/// Build our pool, quote `coins[i] -> coins[j]` for `dx`, and assert it equals
/// the pool's own `get_dy(i, j, dx)` at the same block. `int128_indices` selects
/// the stableswap (`int128`) vs cryptoswap (`uint256`) `get_dy` signature.
#[allow(clippy::too_many_arguments)]
async fn assert_curve_matches_get_dy(
    label: &str,
    pool: &str,
    variant: CurveVariant,
    coins: &[(&str, u8)],
    base_pool: Option<&str>,
    eth_variant: Option<bool>,
    i: usize,
    j: usize,
    dx: u128,
    int128_indices: bool,
) {
    let (reader, chain) = eth_reader();
    let block = reader.latest_block(&chain).await.unwrap();
    let config = CurvePoolConfig {
        address: addr(pool),
        variant,
        coins: coins.iter().map(|(a, _)| eth_asset(a)).collect(),
        decimals: coins.iter().map(|(_, d)| *d).collect(),
        base_pool: base_pool.map(addr),
        eth_variant,
    };
    let ex = CurveExchange::new("curve", chain.clone(), vec![config]);
    let keys = ex.discover(&chain, &[], &reader).await.unwrap();
    let pools = ex
        .refresh(&keys, BlockId::Number(block), &reader)
        .await
        .unwrap();
    assert_eq!(pools.len(), 1, "{label}: pool failed to build");

    let ours = pools[0]
        .quote(
            &pair("ethereum", coins[i].0, coins[j].0),
            Amount(Decimal::from(dx)),
        )
        .unwrap_or_else(|| panic!("{label}: our pool did not quote"));

    let calldata = match int128_indices {
        true => ICurveGetDyInt::get_dyCall {
            i: i as i128,
            j: j as i128,
            dx: U256::from(dx),
        }
        .abi_encode(),
        false => ICurveGetDyUint::get_dyCall {
            i: U256::from(i),
            j: U256::from(j),
            dx: U256::from(dx),
        }
        .abi_encode(),
    };
    let ret = onchain_return(&reader, &chain, block, addr(pool), calldata).await;
    let theirs = match int128_indices {
        true => ICurveGetDyInt::get_dyCall::abi_decode_returns(&ret).unwrap(),
        false => ICurveGetDyUint::get_dyCall::abi_decode_returns(&ret).unwrap(),
    };
    assert_eq_wei(&format!("{label} @ {block}"), ours, theirs);
}

const E18: u128 = 1_000_000_000_000_000_000;
const K_USDC: u128 = 1_000_000_000; // 1000 USDC / USDT (6 decimals)

#[tokio::test]
#[ignore = "live: needs an Ethereum RPC"]
async fn diff_curve_v0_susd() {
    // sUSD: plain 4-coin StableSwap V0 (A_PRECISION=1, int128 balances).
    assert_curve_matches_get_dy(
        "curve V0 sUSD DAI->USDC",
        "0xA5407eAE9Ba41422680e2e00537571bcC53efBfD",
        CurveVariant::StableSwapV0,
        &[(DAI, 18), (USDC, 6), (USDT, 6), (SUSD, 18)],
        None,
        None,
        0,
        1,
        E18,
        true,
    )
    .await;
}

#[tokio::test]
#[ignore = "live: needs an Ethereum RPC"]
async fn diff_curve_v1_3pool() {
    assert_curve_matches_get_dy(
        "curve V1 3pool DAI->USDC",
        "0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7",
        CurveVariant::StableSwapV1,
        &[(DAI, 18), (USDC, 6), (USDT, 6)],
        None,
        None,
        0,
        1,
        E18,
        true,
    )
    .await;
}

#[tokio::test]
#[ignore = "live: needs an Ethereum RPC"]
async fn diff_curve_v2_fraxusdc() {
    assert_curve_matches_get_dy(
        "curve V2 fraxusdc FRAX->USDC",
        "0xDcEF968d416a41Cdac0ED8702fAC8128A64241A2",
        CurveVariant::StableSwapV2,
        &[(FRAX, 18), (USDC, 6)],
        None,
        None,
        0,
        1,
        E18,
        true,
    )
    .await;
}

#[tokio::test]
#[ignore = "live: needs an Ethereum RPC"]
async fn diff_curve_steth() {
    assert_curve_matches_get_dy(
        "curve STETH ETH->stETH",
        "0xDC24316b9AE028F1497c275EB9192a3Ea0f67022",
        CurveVariant::StableSwapSTETH,
        &[(ETH, 18), (STETH, 18)],
        None,
        None,
        0,
        1,
        E18,
        true,
    )
    .await;
}

#[tokio::test]
#[ignore = "live: needs an Ethereum RPC"]
async fn diff_curve_alend_aave() {
    assert_curve_matches_get_dy(
        "curve ALend aave aDAI->aUSDC",
        "0xDeBF20617708857ebe4F679508E7b7863a8A8EeE",
        CurveVariant::StableSwapALend,
        &[(ADAI, 18), (AUSDC, 6), (AUSDT, 6)],
        None,
        None,
        0,
        1,
        E18,
        true,
    )
    .await;
}

#[tokio::test]
#[ignore = "live: needs an Ethereum RPC"]
async fn diff_curve_ng_crvusd() {
    assert_curve_matches_get_dy(
        "curve NG crvUSD/USDC USDC->crvUSD",
        "0x4DEcE678ceceb27446b35C672dC7d61F30bAD69E",
        CurveVariant::StableSwapNG,
        &[(USDC, 6), (CRVUSD, 18)],
        None,
        None,
        0,
        1,
        K_USDC,
        true,
    )
    .await;
}

#[tokio::test]
#[ignore = "live: needs an Ethereum RPC"]
async fn diff_curve_meta_mim() {
    assert_curve_matches_get_dy(
        "curve Meta MIM/3CRV MIM->3CRV",
        "0x5a6A4D54456819380173272A5E8E9B9904BdF41B",
        CurveVariant::StableSwapMeta,
        &[(MIM, 18), (THREE_CRV, 18)],
        Some("0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7"), // 3pool = base
        None,
        0,
        1,
        E18,
        true,
    )
    .await;
}

#[tokio::test]
#[ignore = "live: needs an Ethereum RPC"]
async fn diff_curve_twocrypto_v1() {
    // Legacy CurveCryptoSwap2ETH (WETH/CRV): gamma() but no MATH(), ETH-variant solver.
    assert_curve_matches_get_dy(
        "curve TwoCryptoV1 crveth WETH->CRV",
        "0x8301AE4fc9c624d1D396cbDAa1ed877821D7C511",
        CurveVariant::TwoCryptoV1,
        &[(WETH, 18), (CRV, 18)],
        None,
        Some(true),
        0,
        1,
        E18,
        false,
    )
    .await;
}

#[tokio::test]
#[ignore = "live: needs an Ethereum RPC"]
async fn diff_curve_twocrypto_ng() {
    // TwoCrypto-NG factory pool, MATH v2.x (WETH / x).
    assert_curve_matches_get_dy(
        "curve TwoCryptoNG WETH->token",
        "0x592878b920101946fb5915ab97961bc546f211cc",
        CurveVariant::TwoCryptoNG,
        &[(WETH, 18), (TC_NG_TOKEN, 18)],
        None,
        None,
        0,
        1,
        E18,
        false,
    )
    .await;
}

#[tokio::test]
#[ignore = "live: needs an Ethereum RPC"]
async fn diff_curve_twocrypto_stable() {
    // TwoCrypto-NG pool whose MATH is v0.1.x (stableswap math, gamma ignored).
    assert_curve_matches_get_dy(
        "curve TwoCryptoStable crvUSD->cbBTC",
        "0x83f24023d15D835a213Df24Fd309c47dab5BEB32",
        CurveVariant::TwoCryptoStable,
        &[(CRVUSD, 18), (CBBTC, 8)],
        None,
        None,
        0,
        1,
        E18,
        false,
    )
    .await;
}

#[tokio::test]
#[ignore = "live: needs an Ethereum RPC"]
async fn diff_curve_tricrypto_v1() {
    // tricrypto2 (USDT/WBTC/WETH).
    assert_curve_matches_get_dy(
        "curve TriCryptoV1 tricrypto2 USDT->WETH",
        "0xD51a44d3FaE010294C616388b506AcdA1bfAAE46",
        CurveVariant::TriCryptoV1,
        &[(USDT, 6), (WBTC, 8), (WETH, 18)],
        None,
        None,
        0,
        2,
        K_USDC,
        false,
    )
    .await;
}

#[tokio::test]
#[ignore = "live: needs an Ethereum RPC"]
async fn diff_curve_tricrypto_ng() {
    // tricryptoUSDC (USDC/WBTC/WETH).
    assert_curve_matches_get_dy(
        "curve TriCryptoNG tricryptoUSDC USDC->WETH",
        "0x7F86Bf177Dd4F3494b841a37e810A34dD56c829B",
        CurveVariant::TriCryptoNG,
        &[(USDC, 6), (WBTC, 8), (WETH, 18)],
        None,
        None,
        0,
        2,
        K_USDC,
        false,
    )
    .await;
}

// ─── Aerodrome (Base) ───────────────────────────────────────────────────────────

/// Discover a Base Aerodrome v2 pair, refresh at a pinned block, and assert
/// every pool that quotes `coins[0] -> coins[1]` matches its own `getAmountOut`
/// to the wei. Covers both the volatile (constant-product) and stable (Solidly)
/// math depending on which pools the factory returns.
async fn assert_aero_v2_matches(label: &str, coins: &[(&str, u8)], dx: u128) {
    let (reader, chain) = base_reader();
    let block = reader.latest_block(&chain).await.unwrap();
    let decimals: HashMap<AssetId, u8> = coins.iter().map(|(a, d)| (aid("base", a), *d)).collect();
    let tokens: Vec<AssetId> = coins.iter().map(|(a, _)| aid("base", a)).collect();
    let ex = AerodromeV2Exchange::new("aerodrome_v2", chain.clone(), AERO_FACTORY, decimals);
    let keys = ex.discover(&chain, &tokens, &reader).await.unwrap();
    let pools = ex
        .refresh(&keys, BlockId::Number(block), &reader)
        .await
        .unwrap();

    let p = pair("base", coins[0].0, coins[1].0);
    let mut checked = 0;
    for pool in &pools {
        let Some(ours) = pool.quote(&p, Amount(Decimal::from(dx))) else {
            continue;
        };
        let calldata = IAeroQuote::getAmountOutCall {
            amountIn: U256::from(dx),
            tokenIn: addr(coins[0].0),
        }
        .abi_encode();
        let ret = onchain_return(&reader, &chain, block, addr(pool.id().as_str()), calldata).await;
        let theirs = IAeroQuote::getAmountOutCall::abi_decode_returns(&ret).unwrap();
        assert_eq_wei(
            &format!("{label} {} @ {block}", pool.id().as_str()),
            ours,
            theirs,
        );
        checked += 1;
    }
    assert!(checked > 0, "{label}: no pool quoted the pair");
}

#[tokio::test]
#[ignore = "live: needs a Base RPC"]
async fn diff_aerodrome_volatile() {
    // USDC/WETH — the volatile (constant-product) pool.
    assert_aero_v2_matches(
        "aero volatile USDC->WETH",
        &[(BASE_USDC, 6), (BASE_WETH, 18)],
        K_USDC,
    )
    .await;
}

#[tokio::test]
#[ignore = "live: needs a Base RPC"]
async fn diff_aerodrome_stable() {
    // USDC/USDbC — a correlated pair, so the Solidly x³y+y³x stable math is
    // exercised intentionally.
    assert_aero_v2_matches(
        "aero stable USDC->USDbC",
        &[(BASE_USDC, 6), (BASE_USDBC, 6)],
        K_USDC,
    )
    .await;
}

#[tokio::test]
#[ignore = "live: needs a Base RPC"]
async fn diff_aerodrome_slipstream() {
    use alloy::primitives::aliases::{I24, U160};

    let (reader, chain) = base_reader();
    let block = reader.latest_block(&chain).await.unwrap();
    // A single tick spacing so the pool <-> quoter comparison is unambiguous.
    let tick_spacing = 100i32;
    let ex = SlipstreamExchange::new(
        "aerodrome_slipstream",
        chain.clone(),
        address!("0x5e7BB104d84c7CB9B682AaC2F3d509f5F406809A"),
        vec![tick_spacing],
    );
    let tokens = vec![aid("base", BASE_USDC), aid("base", BASE_WETH)];
    let keys = ex.discover(&chain, &tokens, &reader).await.unwrap();
    let pools = ex
        .refresh(&keys, BlockId::Number(block), &reader)
        .await
        .unwrap();

    let dx = K_USDC;
    let ours = pools[0]
        .quote(
            &pair("base", BASE_USDC, BASE_WETH),
            Amount(Decimal::from(dx)),
        )
        .unwrap();

    let params = ISlipstreamQuoter::QuoteExactInputSingleParams {
        tokenIn: addr(BASE_USDC),
        tokenOut: addr(BASE_WETH),
        amountIn: U256::from(dx),
        tickSpacing: I24::try_from(tick_spacing).unwrap(),
        sqrtPriceLimitX96: U160::ZERO,
    };
    let calldata = ISlipstreamQuoter::quoteExactInputSingleCall { params }.abi_encode();
    let ret = onchain_return(
        &reader,
        &chain,
        block,
        address!("0x254cf9E1E6e233aa1AC962CB9B05b2cfeAaE15b0"),
        calldata,
    )
    .await;
    let theirs = ISlipstreamQuoter::quoteExactInputSingleCall::abi_decode_returns(&ret)
        .unwrap()
        .amountOut;
    assert_close_ppm("slipstream USDC->WETH", ours, theirs, 100);
}

// ─── Uniswap ────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "live: needs an Ethereum RPC"]
async fn diff_uniswap_v2() {
    let (reader, chain) = eth_reader();
    let block = reader.latest_block(&chain).await.unwrap();
    let ex = UniswapV2Exchange::new(
        "uniswap_v2",
        chain.clone(),
        address!("0x5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f"),
        30,
    );
    let tokens = vec![eth_asset(USDC), eth_asset(WETH)];
    let keys = ex.discover(&chain, &tokens, &reader).await.unwrap();
    let pools = ex
        .refresh(&keys, BlockId::Number(block), &reader)
        .await
        .unwrap();

    let dx = K_USDC;
    let ours = pools[0]
        .quote(&pair("ethereum", USDC, WETH), Amount(Decimal::from(dx)))
        .unwrap();

    // The pair has no quote fn — compare against the Router's library math.
    let calldata = IUniV2Router::getAmountsOutCall {
        amountIn: U256::from(dx),
        path: vec![addr(USDC), addr(WETH)],
    }
    .abi_encode();
    let ret = onchain_return(
        &reader,
        &chain,
        block,
        address!("0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D"),
        calldata,
    )
    .await;
    let theirs = *IUniV2Router::getAmountsOutCall::abi_decode_returns(&ret)
        .unwrap()
        .last()
        .unwrap();
    assert_eq_wei(&format!("univ2 USDC->WETH @ {block}"), ours, theirs);
}

#[tokio::test]
#[ignore = "live: needs an Ethereum RPC"]
async fn diff_uniswap_v3() {
    use alloy::primitives::aliases::{U24, U160};

    let (reader, chain) = eth_reader();
    let block = reader.latest_block(&chain).await.unwrap();
    // A single fee tier so the pool <-> quoter comparison is unambiguous.
    let fee = 500u32;
    let ex = UniswapV3Exchange::new(
        "uniswap_v3",
        chain.clone(),
        address!("0x1F98431c8aD98523631AE4a59f267346ea31F984"),
        vec![fee],
    );
    let tokens = vec![eth_asset(USDC), eth_asset(WETH)];
    let keys = ex.discover(&chain, &tokens, &reader).await.unwrap();
    let pools = ex
        .refresh(&keys, BlockId::Number(block), &reader)
        .await
        .unwrap();

    let dx = K_USDC;
    let ours = pools[0]
        .quote(&pair("ethereum", USDC, WETH), Amount(Decimal::from(dx)))
        .unwrap();

    let params = IUniV3Quoter::QuoteExactInputSingleParams {
        tokenIn: addr(USDC),
        tokenOut: addr(WETH),
        amountIn: U256::from(dx),
        fee: U24::from(fee),
        sqrtPriceLimitX96: U160::ZERO,
    };
    let calldata = IUniV3Quoter::quoteExactInputSingleCall { params }.abi_encode();
    let ret = onchain_return(
        &reader,
        &chain,
        block,
        address!("0x61fFE014bA17989E743c5F6cB21bF9697530B21e"),
        calldata,
    )
    .await;
    let theirs = IUniV3Quoter::quoteExactInputSingleCall::abi_decode_returns(&ret)
        .unwrap()
        .amountOut;
    assert_close_ppm(&format!("univ3 USDC->WETH @ {block}"), ours, theirs, 100);
}

#[tokio::test]
#[ignore = "live: needs an Ethereum RPC"]
async fn diff_uniswap_v4() {
    use alloy::primitives::aliases::{I24, U24};

    let (reader, chain) = eth_reader();
    let block = reader.latest_block(&chain).await.unwrap();
    let native_eth = "0x0000000000000000000000000000000000000000";
    // ETH/USDC 0.05% in the singleton PoolManager (ETH = currency0 = address(0)).
    let config = V4PoolConfig::new(
        Address::ZERO,
        addr(USDC),
        eth_asset(native_eth),
        eth_asset(USDC),
        500,
        10,
        Address::ZERO,
        18,
        6,
    );
    let ex = UniswapV4Exchange::new(
        "uniswap_v4",
        chain.clone(),
        address!("0x000000000004444c5dc75cB358380D2e3dE08A90"),
        vec![config],
    );
    let keys = ex.discover(&chain, &[], &reader).await.unwrap();
    let pools = ex
        .refresh(&keys, BlockId::Number(block), &reader)
        .await
        .unwrap();

    let dx = 100_000_000_000_000_000u128; // 0.1 ETH
    let ours = pools[0]
        .quote(
            &pair("ethereum", native_eth, USDC),
            Amount(Decimal::from(dx)),
        )
        .unwrap();

    let params = IV4Quoter::QuoteExactSingleParams {
        poolKey: IV4Quoter::PoolKey {
            currency0: Address::ZERO,
            currency1: addr(USDC),
            fee: U24::from(500),
            tickSpacing: I24::try_from(10).unwrap(),
            hooks: Address::ZERO,
        },
        zeroForOne: true,
        exactAmount: dx,
        hookData: alloy::primitives::Bytes::new(),
    };
    let calldata = IV4Quoter::quoteExactInputSingleCall { params }.abi_encode();
    let ret = onchain_return(
        &reader,
        &chain,
        block,
        address!("0x52F0E24D1c21C8A0cB1e5a5dD6198556BD9E1203"),
        calldata,
    )
    .await;
    let theirs = IV4Quoter::quoteExactInputSingleCall::abi_decode_returns(&ret)
        .unwrap()
        .amountOut;
    assert_close_ppm(&format!("univ4 ETH->USDC @ {block}"), ours, theirs, 100);
}
