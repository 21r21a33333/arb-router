//! Curve `Exchange`: refreshes configured pools of **every** `curve-math`
//! variant (all 12) into quotable [`CurvePool`]s via `curve_adapter::build_pool`.
//!
//! Discovery is **config-driven** — Curve's registry topology is intricate, so
//! each pool's address, variant, and coins come from configuration. Refresh
//! reads the per-block state each variant needs and hands a fully-populated
//! [`RawPoolState`] to the adapter, which constructs the correct `curve_math`
//! variant. Different variants need different on-chain reads:
//!
//! | family | reads (per block) |
//! |--------|-------------------|
//! | StableSwap plain (V0/V1/V2/STETH) | `A`, `fee`, `balances` |
//! | StableSwap Meta | + base pool `get_virtual_price` (the LP coin's rate) |
//! | StableSwap ALend | + `offpeg_fee_multiplier` |
//! | StableSwap-NG | `A`, `fee`, `balances`, `offpeg_fee_multiplier`, `stored_rates` |
//! | TwoCrypto (V1/NG/Stable) | `A`, `balances`, `mid_fee`, `out_fee`, `fee_gamma`, `D`, `price_scale`, `gamma` |
//! | TriCrypto (V1/NG) | as TwoCrypto but 3 coins + indexed `price_scale(i)` |
//!
//! Only V0-era pools index `balances` by `int128`; every later pool uses
//! `uint256` (verified against mainnet). Coins/decimals come from config;
//! `precisions` are left to the adapter (computed from decimals). `amp` is the
//! pool's `A()` — for crypto pools the `A_MULTIPLIER`-scaled value the adapter
//! expects.

use alloy::primitives::{Address, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use async_trait::async_trait;
use curve_adapter::{CurveVariant, RawPoolState, build_pool};

use super::pool::CurvePool;
use crate::adapters::exchanges::{call, read_err};
use crate::core::deps::chain_reader::ChainReader;
use crate::core::deps::exchange::{Exchange, ExchangeError};
use crate::core::deps::pool::Pool;
use crate::primitives::asset::{AssetId, ChainId};
use crate::primitives::chain::{BlockId, Call, CallResult};
use crate::primitives::pool::{ExchangeId, PoolKey};

sol! {
    /// Every Curve getter that returns a single `uint256` (decoded uniformly).
    interface ICurveScalar {
        function A() external view returns (uint256);
        function future_A() external view returns (uint256);
        function fee() external view returns (uint256);
        function mid_fee() external view returns (uint256);
        function out_fee() external view returns (uint256);
        function fee_gamma() external view returns (uint256);
        function gamma() external view returns (uint256);
        function D() external view returns (uint256);
        function offpeg_fee_multiplier() external view returns (uint256);
        function get_virtual_price() external view returns (uint256);
        function price_scale() external view returns (uint256);
    }

    /// Older StableSwap pools index `balances` by `int128`.
    interface ICurveBalancesInt {
        function balances(int128 i) external view returns (uint256);
    }

    /// NG / CryptoSwap pools index `balances` by `uint256`.
    interface ICurveBalancesUint {
        function balances(uint256 i) external view returns (uint256);
    }

    /// StableSwap-NG per-token rates (oracle / ERC4626 aware).
    interface ICurveStoredRates {
        function stored_rates() external view returns (uint256[] memory);
    }

    /// TriCrypto indexes `price_scale` by coin.
    interface ICurvePriceScaleIdx {
        function price_scale(uint256 i) external view returns (uint256);
    }
}

/// The read/build strategy shared by variants with the same on-chain shape.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Family {
    /// StableSwap V0/V1/V2/STETH — `A`, `fee`, `balances(int128)`.
    Plain,
    /// StableSwap Meta — plain + base-pool `get_virtual_price` for the LP coin.
    Meta,
    /// StableSwap ALend — plain + `offpeg_fee_multiplier`.
    ALend,
    /// StableSwap-NG — `balances(uint256)` + `offpeg_fee_multiplier` + `stored_rates`.
    Ng,
    /// TwoCrypto V1/NG/Stable — 2-coin CryptoSwap.
    TwoCrypto,
    /// TriCrypto V1/NG — 3-coin CryptoSwap.
    TriCrypto,
}

impl Family {
    fn of(variant: CurveVariant) -> Self {
        match variant {
            CurveVariant::StableSwapV0
            | CurveVariant::StableSwapV1
            | CurveVariant::StableSwapV2
            | CurveVariant::StableSwapSTETH => Family::Plain,
            CurveVariant::StableSwapMeta => Family::Meta,
            CurveVariant::StableSwapALend => Family::ALend,
            CurveVariant::StableSwapNG => Family::Ng,
            CurveVariant::TwoCryptoV1
            | CurveVariant::TwoCryptoNG
            | CurveVariant::TwoCryptoStable => Family::TwoCrypto,
            CurveVariant::TriCryptoV1 | CurveVariant::TriCryptoNG => Family::TriCrypto,
        }
    }
}

/// A configured Curve pool: where it is, which variant, its coins/decimals, and
/// the extra references some variants need (Meta base pool, TwoCryptoV1 ETH flag).
#[derive(Clone)]
pub struct CurvePoolConfig {
    pub address: Address,
    pub variant: CurveVariant,
    pub coins: Vec<AssetId>,
    pub decimals: Vec<u8>,
    /// Meta pools: the base pool whose `get_virtual_price` is the LP coin's rate.
    pub base_pool: Option<Address>,
    /// `TwoCryptoV1` only: whether it's the WETH (ETH-variant) Newton solver.
    pub eth_variant: Option<bool>,
}

/// Refreshes a configured set of Curve pools on one chain.
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

    /// The ordered read calls a pool needs, by family. Fails only if a Meta pool
    /// is missing its configured base pool.
    fn calls_for(&self, config: &CurvePoolConfig) -> Result<Vec<Call>, ExchangeError> {
        let addr = config.address;
        let n = config.coins.len();
        let mut calls = Vec::new();

        let scalar = |sel: Vec<u8>| call(addr, sel);
        // Only the oldest (V0-era: Compound/sUSD/y/busd/pax/ren/sbtc) pools index
        // balances by `int128`. Every later pool — V1/V2/STETH/Meta/ALend/NG and
        // all CryptoSwap — uses `uint256`. (Verified against mainnet: `balances`
        // with the wrong index type reverts.)
        let bal = |i: usize| match config.variant == CurveVariant::StableSwapV0 {
            true => call(
                addr,
                ICurveBalancesInt::balancesCall { i: i as i128 }.abi_encode(),
            ),
            false => call(
                addr,
                ICurveBalancesUint::balancesCall { i: U256::from(i) }.abi_encode(),
            ),
        };

        match Family::of(config.variant) {
            Family::Plain => {
                calls.push(scalar(ICurveScalar::future_ACall {}.abi_encode()));
                calls.push(scalar(ICurveScalar::ACall {}.abi_encode()));
                calls.push(scalar(ICurveScalar::feeCall {}.abi_encode()));
                (0..n).for_each(|i| calls.push(bal(i)));
            }
            Family::Meta => {
                let base = config.base_pool.ok_or_else(|| {
                    ExchangeError::Decode("curve meta pool missing base_pool".into())
                })?;
                calls.push(scalar(ICurveScalar::future_ACall {}.abi_encode()));
                calls.push(scalar(ICurveScalar::ACall {}.abi_encode()));
                calls.push(scalar(ICurveScalar::feeCall {}.abi_encode()));
                (0..n).for_each(|i| calls.push(bal(i)));
                calls.push(call(
                    base,
                    ICurveScalar::get_virtual_priceCall {}.abi_encode(),
                ));
            }
            Family::ALend => {
                calls.push(scalar(ICurveScalar::future_ACall {}.abi_encode()));
                calls.push(scalar(ICurveScalar::ACall {}.abi_encode()));
                calls.push(scalar(ICurveScalar::feeCall {}.abi_encode()));
                (0..n).for_each(|i| calls.push(bal(i)));
                calls.push(scalar(
                    ICurveScalar::offpeg_fee_multiplierCall {}.abi_encode(),
                ));
            }
            Family::Ng => {
                calls.push(scalar(ICurveScalar::future_ACall {}.abi_encode()));
                calls.push(scalar(ICurveScalar::ACall {}.abi_encode()));
                calls.push(scalar(ICurveScalar::feeCall {}.abi_encode()));
                (0..n).for_each(|i| calls.push(bal(i)));
                calls.push(scalar(
                    ICurveScalar::offpeg_fee_multiplierCall {}.abi_encode(),
                ));
                calls.push(call(
                    addr,
                    ICurveStoredRates::stored_ratesCall {}.abi_encode(),
                ));
            }
            Family::TwoCrypto | Family::TriCrypto => {
                calls.push(scalar(ICurveScalar::ACall {}.abi_encode()));
                (0..n).for_each(|i| calls.push(bal(i)));
                calls.push(scalar(ICurveScalar::mid_feeCall {}.abi_encode()));
                calls.push(scalar(ICurveScalar::out_feeCall {}.abi_encode()));
                calls.push(scalar(ICurveScalar::fee_gammaCall {}.abi_encode()));
                calls.push(scalar(ICurveScalar::DCall {}.abi_encode()));
                // gamma: required for every crypto variant except TwoCryptoStable.
                if config.variant != CurveVariant::TwoCryptoStable {
                    calls.push(scalar(ICurveScalar::gammaCall {}.abi_encode()));
                }
                match Family::of(config.variant) {
                    Family::TriCrypto => {
                        // 3-coin pools expose price_scale per coin (2 elements).
                        calls.push(call(
                            addr,
                            ICurvePriceScaleIdx::price_scaleCall { i: U256::from(0) }.abi_encode(),
                        ));
                        calls.push(call(
                            addr,
                            ICurvePriceScaleIdx::price_scaleCall { i: U256::from(1) }.abi_encode(),
                        ));
                    }
                    _ => calls.push(scalar(ICurveScalar::price_scaleCall {}.abi_encode())),
                }
            }
        }
        Ok(calls)
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
        // Lay every pool's variant-specific reads out in one batch, remembering
        // which slice of results belongs to which pool.
        let mut calls = Vec::new();
        let mut plans: Vec<(&CurvePoolConfig, std::ops::Range<usize>)> = Vec::new();
        for key in keys {
            let Some(config) = self.config_for(key) else {
                continue;
            };
            let start = calls.len();
            calls.extend(self.calls_for(config)?);
            plans.push((config, start..calls.len()));
        }

        let out = reader
            .call_batch(&self.chain, at, calls)
            .await
            .map_err(read_err)?;

        let mut pools: Vec<Box<dyn Pool>> = Vec::new();
        for (config, range) in plans {
            if let Some(pool) = build_curve_pool(config, &out.results[range]) {
                pools.push(pool);
            }
            // A reverted / undecodable read drops the pool this tick.
        }
        Ok(pools)
    }
}

/// Decode one pool's result slice into a [`RawPoolState`] and build it. Returns
/// `None` if a required read reverted or the adapter rejects the state.
fn build_curve_pool(config: &CurvePoolConfig, results: &[CallResult]) -> Option<Box<dyn Pool>> {
    let n = config.coins.len();
    let mut cur = Cursor::new(results);

    let raw = match Family::of(config.variant) {
        Family::Plain => RawPoolState {
            variant: config.variant,
            amp: cur.amp()?,
            fee: Some(cur.next()?),
            balances: cur.take(n)?,
            token_decimals: config.decimals.clone(),
            ..Default::default()
        },
        Family::Meta => {
            let amp = cur.amp()?;
            let fee = cur.next()?;
            let balances = cur.take(n)?;
            let virtual_price = cur.next()?;
            // Every coin but the last prices off its decimals; the last is the
            // base-pool LP token, whose rate is the base pool's virtual price.
            let mut rates: Vec<Option<U256>> = vec![None; n];
            rates[n - 1] = Some(virtual_price);
            RawPoolState {
                variant: config.variant,
                amp,
                fee: Some(fee),
                balances,
                token_decimals: config.decimals.clone(),
                dynamic_rates: Some(rates),
                ..Default::default()
            }
        }
        Family::ALend => RawPoolState {
            variant: config.variant,
            amp: cur.amp()?,
            fee: Some(cur.next()?),
            balances: cur.take(n)?,
            token_decimals: config.decimals.clone(),
            offpeg_fee_multiplier: Some(cur.next()?),
            ..Default::default()
        },
        Family::Ng => {
            let amp = cur.amp()?;
            let fee = cur.next()?;
            let balances = cur.take(n)?;
            // Both are tolerant: crvUSD factory pools lack `offpeg`, and plain-token
            // pools have no meaningful `stored_rates` (the adapter falls back to decimals).
            let offpeg = cur.next();
            let dynamic_rates = cur
                .next_array()
                .map(|rates| rates.into_iter().map(Some).collect());
            RawPoolState {
                variant: config.variant,
                amp,
                fee: Some(fee),
                balances,
                token_decimals: config.decimals.clone(),
                offpeg_fee_multiplier: offpeg,
                dynamic_rates,
                ..Default::default()
            }
        }
        Family::TwoCrypto | Family::TriCrypto => {
            let amp = cur.next()?;
            let balances = cur.take(n)?;
            let mid_fee = cur.next()?;
            let out_fee = cur.next()?;
            let fee_gamma = cur.next()?;
            let d = cur.next()?;
            let gamma = match config.variant {
                CurveVariant::TwoCryptoStable => None,
                _ => Some(cur.next()?),
            };
            let price_scale = match Family::of(config.variant) {
                Family::TriCrypto => vec![cur.next()?, cur.next()?],
                _ => vec![cur.next()?],
            };
            RawPoolState {
                variant: config.variant,
                amp,
                balances,
                token_decimals: config.decimals.clone(),
                mid_fee: Some(mid_fee),
                out_fee: Some(out_fee),
                fee_gamma: Some(fee_gamma),
                d: Some(d),
                gamma,
                price_scale: Some(price_scale),
                eth_variant: config.eth_variant,
                ..Default::default()
            }
        }
    };

    let inner = build_pool(&raw).ok()?;
    Some(Box::new(CurvePool::new(
        &config.address.to_string(),
        config.coins.clone(),
        inner,
    )))
}

/// A sequential reader over a pool's result slice — each method advances by the
/// calls it consumes, mirroring `calls_for`'s emission order.
struct Cursor<'a> {
    results: &'a [CallResult],
    idx: usize,
}

impl<'a> Cursor<'a> {
    fn new(results: &'a [CallResult]) -> Self {
        Self { results, idx: 0 }
    }

    /// The next single-`uint256` read, or `None` if it reverted / is missing.
    fn next(&mut self) -> Option<U256> {
        let value = self.results.get(self.idx).and_then(decode_scalar);
        self.idx += 1;
        value
    }

    /// The amplification coefficient: `future_A()` then `A()`, preferring the
    /// former. `A()` truncates by `A_PRECISION` (÷100 on modern pools), losing
    /// the low digits; `future_A()` returns the exact raw value while a pool is
    /// idle (the normal state), so it's what the on-chain math actually uses.
    /// Falls back to `A()` for V0-era pools that lack `future_A()`.
    ///
    /// (An actively-ramping pool would need `interpolate_a` over the ramp
    /// endpoints + block timestamp; ramps are rare and admin-initiated.)
    fn amp(&mut self) -> Option<U256> {
        let future = self.next();
        let current = self.next();
        future.or(current)
    }

    /// The next `n` single-`uint256` reads, or `None` if any reverted.
    fn take(&mut self, n: usize) -> Option<Vec<U256>> {
        (0..n).map(|_| self.next()).collect()
    }

    /// The next `uint256[]` read (e.g. `stored_rates`), or `None`.
    fn next_array(&mut self) -> Option<Vec<U256>> {
        let value = self.results.get(self.idx).and_then(decode_array);
        self.idx += 1;
        value
    }
}

/// Decode a successful single-`uint256` result (every scalar getter shares this shape).
fn decode_scalar(result: &CallResult) -> Option<U256> {
    match result.success {
        true => ICurveScalar::ACall::abi_decode_returns(&result.data.0).ok(),
        false => None,
    }
}

/// Decode a successful `uint256[]` result.
fn decode_array(result: &CallResult) -> Option<Vec<U256>> {
    match result.success {
        true => ICurveStoredRates::stored_ratesCall::abi_decode_returns(&result.data.0).ok(),
        false => None,
    }
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

    fn asset(s: &str) -> AssetId {
        AssetId::new(s).unwrap()
    }
    const POOL: Address = address!("0x00000000000000000000000000000000000000a1");
    const BASE: Address = address!("0x00000000000000000000000000000000000000b2");
    const E18: u128 = 1_000_000_000_000_000_000;

    /// Encodes each Curve getter it's asked for from a fixed set of values,
    /// routing `balances(i)` (int128 or uint256), `price_scale(i)`, arrays, and
    /// the base-pool `get_virtual_price` correctly.
    #[derive(Clone, Default)]
    struct FakeCurve {
        a: U256,
        fee: U256,
        balances: Vec<U256>,
        offpeg: Option<U256>,
        stored_rates: Option<Vec<U256>>,
        mid_fee: U256,
        out_fee: U256,
        fee_gamma: U256,
        d: U256,
        gamma: U256,
        price_scale: Vec<U256>,
        virtual_price: U256,
    }

    fn u256(data: Vec<u8>) -> CallResult {
        CallResult {
            success: true,
            data: Bytes(data),
        }
    }
    fn reverted() -> CallResult {
        CallResult {
            success: false,
            data: Bytes(Vec::new()),
        }
    }
    fn enc(v: U256) -> Vec<u8> {
        ICurveScalar::ACall::abi_encode_returns(&v)
    }

    impl FakeCurve {
        fn answer(&self, c: &Call) -> CallResult {
            let d = &c.calldata.0;
            let sel = |s: [u8; 4]| d.starts_with(&s);
            if sel(ICurveScalar::ACall::SELECTOR) {
                u256(enc(self.a))
            } else if sel(ICurveScalar::feeCall::SELECTOR) {
                u256(enc(self.fee))
            } else if sel(ICurveBalancesInt::balancesCall::SELECTOR) {
                let i = ICurveBalancesInt::balancesCall::abi_decode(d).unwrap().i as usize;
                u256(enc(self.balances[i]))
            } else if sel(ICurveBalancesUint::balancesCall::SELECTOR) {
                let i: usize = ICurveBalancesUint::balancesCall::abi_decode(d)
                    .unwrap()
                    .i
                    .to::<u64>() as usize;
                u256(enc(self.balances[i]))
            } else if sel(ICurveScalar::offpeg_fee_multiplierCall::SELECTOR) {
                match self.offpeg {
                    Some(v) => u256(enc(v)),
                    None => reverted(),
                }
            } else if sel(ICurveStoredRates::stored_ratesCall::SELECTOR) {
                match &self.stored_rates {
                    Some(rates) => u256(ICurveStoredRates::stored_ratesCall::abi_encode_returns(
                        rates,
                    )),
                    None => reverted(),
                }
            } else if sel(ICurveScalar::mid_feeCall::SELECTOR) {
                u256(enc(self.mid_fee))
            } else if sel(ICurveScalar::out_feeCall::SELECTOR) {
                u256(enc(self.out_fee))
            } else if sel(ICurveScalar::fee_gammaCall::SELECTOR) {
                u256(enc(self.fee_gamma))
            } else if sel(ICurveScalar::DCall::SELECTOR) {
                u256(enc(self.d))
            } else if sel(ICurveScalar::gammaCall::SELECTOR) {
                u256(enc(self.gamma))
            } else if sel(ICurvePriceScaleIdx::price_scaleCall::SELECTOR) {
                let i: usize = ICurvePriceScaleIdx::price_scaleCall::abi_decode(d)
                    .unwrap()
                    .i
                    .to::<u64>() as usize;
                u256(enc(self.price_scale[i]))
            } else if sel(ICurveScalar::price_scaleCall::SELECTOR) {
                u256(enc(self.price_scale[0]))
            } else if sel(ICurveScalar::get_virtual_priceCall::SELECTOR) {
                u256(enc(self.virtual_price))
            } else {
                reverted()
            }
        }
    }

    #[async_trait]
    impl ChainReader for FakeCurve {
        async fn latest_block(&self, _chain: &ChainId) -> Result<u64, ChainReadError> {
            Ok(1)
        }
        async fn call_batch(
            &self,
            _chain: &ChainId,
            _at: BlockId,
            calls: Vec<Call>,
        ) -> Result<BatchOutput, ChainReadError> {
            Ok(BatchOutput {
                block: 1,
                results: calls.iter().map(|c| self.answer(c)).collect(),
            })
        }
    }

    /// Refresh a single-pool exchange through the fake and return the built pool.
    async fn refresh_one(config: CurvePoolConfig, reader: FakeCurve) -> Vec<Box<dyn Pool>> {
        let ex = CurveExchange::new("curve", ChainId::new("ethereum"), vec![config.clone()]);
        let keys = ex
            .discover(&ChainId::new("ethereum"), &[], &reader)
            .await
            .unwrap();
        assert_eq!(keys.len(), 1);
        ex.refresh(&keys, BlockId::Number(1), &reader)
            .await
            .unwrap()
    }

    fn stable_coins() -> (Vec<AssetId>, Vec<u8>) {
        (
            vec![
                asset("ethereum:dai"),
                asset("ethereum:usdc"),
                asset("ethereum:usdt"),
            ],
            vec![18, 6, 6],
        )
    }

    /// Balanced 3pool reserves in native units: 1M DAI / USDC / USDT.
    fn stable_balances() -> Vec<U256> {
        vec![
            U256::from(1_000_000u128 * E18),
            U256::from(1_000_000u128 * 1_000_000),
            U256::from(1_000_000u128 * 1_000_000),
        ]
    }

    fn assert_near_one_usdc(pools: &[Box<dyn Pool>]) {
        assert_eq!(pools.len(), 1, "pool must build");
        let out = pools[0]
            .quote(
                &Pair {
                    source: asset("ethereum:dai"),
                    destination: asset("ethereum:usdc"),
                },
                Amount(Decimal::from(E18)), // 1 DAI
            )
            .expect("must quote");
        assert!(out.0 > Decimal::from(980_000u64), "out {} too low", out.0);
        assert!(
            out.0 < Decimal::from(1_000_000u64),
            "out {} too high",
            out.0
        );
    }

    #[tokio::test]
    async fn plain_stableswap_v1_quotes() {
        let (coins, decimals) = stable_coins();
        let config = CurvePoolConfig {
            address: POOL,
            variant: CurveVariant::StableSwapV1,
            coins,
            decimals,
            base_pool: None,
            eth_variant: None,
        };
        let reader = FakeCurve {
            a: U256::from(2000u64),
            fee: U256::from(1_000_000u64),
            balances: stable_balances(),
            ..Default::default()
        };
        assert_near_one_usdc(&refresh_one(config, reader).await);
    }

    #[tokio::test]
    async fn stableswap_ng_reads_offpeg_and_stored_rates() {
        let (coins, decimals) = stable_coins();
        let config = CurvePoolConfig {
            address: POOL,
            variant: CurveVariant::StableSwapNG,
            coins,
            decimals,
            base_pool: None,
            eth_variant: None,
        };
        // stored_rates as 10^(36-decimals): DAI 1e18, USDC/USDT 1e30 (matches the
        // decimals fallback, so the quote stays near parity while exercising the read).
        let reader = FakeCurve {
            a: U256::from(2000u64),
            fee: U256::from(1_000_000u64),
            balances: stable_balances(),
            offpeg: Some(U256::from(10_000_000_000u64)),
            stored_rates: Some(vec![
                U256::from(E18),
                U256::from(10u128).pow(U256::from(30u64)),
                U256::from(10u128).pow(U256::from(30u64)),
            ]),
            ..Default::default()
        };
        assert_near_one_usdc(&refresh_one(config, reader).await);
    }

    #[tokio::test]
    async fn stableswap_ng_tolerates_missing_offpeg_and_rates() {
        let (coins, decimals) = stable_coins();
        let config = CurvePoolConfig {
            address: POOL,
            variant: CurveVariant::StableSwapNG,
            coins,
            decimals,
            base_pool: None,
            eth_variant: None,
        };
        // Both offpeg and stored_rates revert — a crvUSD-style plain pool.
        let reader = FakeCurve {
            a: U256::from(2000u64),
            fee: U256::from(1_000_000u64),
            balances: stable_balances(),
            offpeg: None,
            stored_rates: None,
            ..Default::default()
        };
        assert_near_one_usdc(&refresh_one(config, reader).await);
    }

    #[tokio::test]
    async fn stableswap_alend_reads_offpeg() {
        let (coins, decimals) = stable_coins();
        let config = CurvePoolConfig {
            address: POOL,
            variant: CurveVariant::StableSwapALend,
            coins,
            decimals,
            base_pool: None,
            eth_variant: None,
        };
        let reader = FakeCurve {
            a: U256::from(2000u64),
            fee: U256::from(1_000_000u64),
            balances: stable_balances(),
            offpeg: Some(U256::from(20_000_000_000u64)),
            ..Default::default()
        };
        assert_near_one_usdc(&refresh_one(config, reader).await);
    }

    #[tokio::test]
    async fn stableswap_meta_reads_base_virtual_price() {
        let config = CurvePoolConfig {
            address: POOL,
            variant: CurveVariant::StableSwapMeta,
            coins: vec![asset("ethereum:mim"), asset("ethereum:3crv")],
            decimals: vec![18, 18],
            base_pool: Some(BASE),
            eth_variant: None,
        };
        // Base pool virtual price ~1.0; balanced reserves → near parity.
        let reader = FakeCurve {
            a: U256::from(2000u64),
            fee: U256::from(1_000_000u64),
            balances: vec![
                U256::from(1_000_000u128 * E18),
                U256::from(1_000_000u128 * E18),
            ],
            virtual_price: U256::from(E18),
            ..Default::default()
        };
        let pools = refresh_one(config, reader).await;
        assert_eq!(pools.len(), 1, "meta pool must build (needs virtual_price)");
        let out = pools[0]
            .quote(
                &Pair {
                    source: asset("ethereum:mim"),
                    destination: asset("ethereum:3crv"),
                },
                Amount(Decimal::from(E18)),
            )
            .expect("meta must quote");
        assert!(out.0 > Decimal::from(980_000_000_000_000_000u64));
        assert!(out.0 < Decimal::from(E18));
    }

    #[tokio::test]
    async fn meta_without_base_pool_is_skipped() {
        // A meta config with no base pool can't be read — refresh drops it.
        let ex = CurveExchange::new(
            "curve",
            ChainId::new("ethereum"),
            vec![CurvePoolConfig {
                address: POOL,
                variant: CurveVariant::StableSwapMeta,
                coins: vec![asset("ethereum:mim"), asset("ethereum:3crv")],
                decimals: vec![18, 18],
                base_pool: None,
                eth_variant: None,
            }],
        );
        let keys = ex
            .discover(&ChainId::new("ethereum"), &[], &FakeCurve::default())
            .await
            .unwrap();
        let err = ex
            .refresh(&keys, BlockId::Number(1), &FakeCurve::default())
            .await;
        assert!(err.is_err(), "missing base_pool must surface as an error");
    }

    #[tokio::test]
    async fn twocrypto_ng_reads_full_crypto_state() {
        let config = CurvePoolConfig {
            address: POOL,
            variant: CurveVariant::TwoCryptoNG,
            coins: vec![asset("ethereum:crv"), asset("ethereum:weth")],
            decimals: vec![18, 18],
            base_pool: None,
            eth_variant: None,
        };
        // curve-adapter's own TwoCryptoNG test vector.
        let reader = FakeCurve {
            a: U256::from(540_000u64 * 10_000u64),
            balances: vec![U256::from(1_000u128 * E18), U256::from(1_000u128 * E18)],
            mid_fee: U256::from(3_000_000u64),
            out_fee: U256::from(30_000_000u64),
            fee_gamma: U256::from(500_000_000_000_000u128),
            d: U256::from(2_000u128 * E18),
            gamma: U256::from(10_000_000_000_000u128),
            price_scale: vec![U256::from(E18)],
            ..Default::default()
        };
        let pools = refresh_one(config, reader).await;
        assert_eq!(pools.len(), 1, "twocrypto pool must build from full state");
        let out = pools[0].quote(
            &Pair {
                source: asset("ethereum:crv"),
                destination: asset("ethereum:weth"),
            },
            Amount(Decimal::from(E18)),
        );
        assert!(out.is_some_and(|a| a.0 > Decimal::ZERO), "must quote > 0");
    }

    #[tokio::test]
    async fn tricrypto_ng_reads_indexed_price_scale() {
        let config = CurvePoolConfig {
            address: POOL,
            variant: CurveVariant::TriCryptoNG,
            coins: vec![
                asset("ethereum:a"),
                asset("ethereum:b"),
                asset("ethereum:c"),
            ],
            decimals: vec![18, 18, 18],
            base_pool: None,
            eth_variant: None,
        };
        // A value-balanced 3-coin pool at 1:1 prices, so the swap solver has a
        // consistent invariant to work with (real-pool quoting is checked by the
        // live tests; this asserts the 2-element indexed price_scale is read and
        // the full crypto state builds + quotes).
        let reader = FakeCurve {
            a: U256::from(1_707_629u64 * 10_000u64),
            balances: vec![
                U256::from(1_000u128 * E18),
                U256::from(1_000u128 * E18),
                U256::from(1_000u128 * E18),
            ],
            mid_fee: U256::from(3_000_000u64),
            out_fee: U256::from(30_000_000u64),
            fee_gamma: U256::from(500_000_000_000_000u128),
            d: U256::from(3_000u128 * E18),
            gamma: U256::from(11_809_167_828_997u128),
            price_scale: vec![U256::from(E18), U256::from(E18)],
            ..Default::default()
        };
        let pools = refresh_one(config, reader).await;
        assert_eq!(pools.len(), 1, "tricrypto pool must build from full state");
        let out = pools[0].quote(
            &Pair {
                source: asset("ethereum:a"),
                destination: asset("ethereum:c"),
            },
            Amount(Decimal::from(E18)),
        );
        assert!(out.is_some_and(|a| a.0 > Decimal::ZERO), "must quote > 0");
    }

    /// Every plain-StableSwap variant (incl. V0's `int128` balances path) builds
    /// and quotes near parity from a balanced pool.
    #[tokio::test]
    async fn all_plain_stableswap_variants_build() {
        for variant in [
            CurveVariant::StableSwapV0,
            CurveVariant::StableSwapV1,
            CurveVariant::StableSwapV2,
        ] {
            let (coins, decimals) = stable_coins();
            let config = CurvePoolConfig {
                address: POOL,
                variant,
                coins,
                decimals,
                base_pool: None,
                eth_variant: None,
            };
            let reader = FakeCurve {
                a: U256::from(2000u64),
                fee: U256::from(1_000_000u64),
                balances: stable_balances(),
                ..Default::default()
            };
            let pools = refresh_one(config, reader).await;
            assert_eq!(pools.len(), 1, "{variant:?} must build");
            let out = pools[0]
                .quote(
                    &Pair {
                        source: asset("ethereum:dai"),
                        destination: asset("ethereum:usdc"),
                    },
                    Amount(Decimal::from(E18)),
                )
                .unwrap_or_else(|| panic!("{variant:?} must quote"));
            assert!(out.0 > Decimal::from(950_000u64), "{variant:?}: {}", out.0);
        }
    }

    /// Every 2-coin CryptoSwap variant builds from full crypto state — including
    /// `TwoCryptoV1` (needs `eth_variant`) and `TwoCryptoStable` (no `gamma`).
    #[tokio::test]
    async fn all_twocrypto_variants_build() {
        for variant in [
            CurveVariant::TwoCryptoV1,
            CurveVariant::TwoCryptoNG,
            CurveVariant::TwoCryptoStable,
        ] {
            let config = CurvePoolConfig {
                address: POOL,
                variant,
                coins: vec![asset("ethereum:x"), asset("ethereum:y")],
                decimals: vec![18, 18],
                base_pool: None,
                eth_variant: Some(true), // used only by TwoCryptoV1
            };
            let reader = FakeCurve {
                a: U256::from(540_000u64 * 10_000u64),
                balances: vec![U256::from(1_000u128 * E18), U256::from(1_000u128 * E18)],
                mid_fee: U256::from(3_000_000u64),
                out_fee: U256::from(30_000_000u64),
                fee_gamma: U256::from(500_000_000_000_000u128),
                d: U256::from(2_000u128 * E18),
                gamma: U256::from(10_000_000_000_000u128),
                price_scale: vec![U256::from(E18)],
                ..Default::default()
            };
            let pools = refresh_one(config, reader).await;
            assert_eq!(pools.len(), 1, "{variant:?} must build from crypto state");
            let out = pools[0].quote(
                &Pair {
                    source: asset("ethereum:x"),
                    destination: asset("ethereum:y"),
                },
                Amount(Decimal::from(E18)),
            );
            assert!(
                out.is_some_and(|a| a.0 > Decimal::ZERO),
                "{variant:?} quote"
            );
        }
    }
}
