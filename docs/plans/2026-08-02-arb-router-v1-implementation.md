# Arb Router v1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a self-contained, EVM-only on-chain arbitrage **detector** that continuously finds paths where `value(dest_out) > value(source_in)` (cycles + cross-asset) by quoting locally-synced DEX pool state.

**Architecture:** Modified-hexagonal. `primitives/` (pure value types) ← `core/deps` (port traits) ← `core/application` (search engine); `adapters/` implement the ports (copied munger pool math + vendored garden-rs helpers). A per-chain `Scanner` takes a consistent `PoolSnapshot` each tick, enumerates bounded paths (asset nodes, pool edges, no-pool-reuse), quotes them locally, sizes winners, and notifies.

**Tech Stack:** Rust (edition 2024), tokio, async-trait, thiserror, rust_decimal, alloy (adapters only), arc-swap, moka, axum, config, tracing, eyre.

Spec: `docs/specs/2026-08-01-arb-router-design.md`. Munger source to copy from: `/Users/diwakarmatsaa/Desktop/catalog/munger/crates/dex/src/amm/`.

## Global Constraints

- **Edition 2024**, Rust ≥ 1.97. `name = "arb-router"`.
- **Layering (strict, enforced by module boundaries):** `primitives` depends on nothing internal; `core/deps` depends only on `primitives`; `core/application` depends on `primitives` + `core/deps`; `adapters` implement `core/deps` and may use external SDKs; **`core` must never import `adapters` or chain SDKs (alloy)**.
- **`primitives` hold no trait objects** — `Path`/`Opportunity` reference pools by `PoolId`, never `Arc<dyn Pool>`.
- **Money is never a float.** `Amount(Decimal)` and `Usd(Decimal)` wrap `rust_decimal::Decimal`. `Amount` is in **base units** (wei); pool math converts to integers internally, never to human units.
- **Errors are typed.** Every port + engine step returns a typed `thiserror` enum. No `anyhow`. **No `.unwrap()`/`.expect()` outside `#[cfg(test)]`.**
- **Self-contained:** no `garden-rs` git dependency. Copy munger's pure pool math; vendor garden helpers into `adapters/vendored` with provenance headers.
- **Quoting:** `Pool::quote` is pure, exact-in. **Graph traversal:** each `PoolId` used at most once per path; assets may repeat (incl. return to start); bounded by `max_hops` (default 16) + `beam_width` + `max_paths_per_scan`.
- **Consistency:** one block per scan tick (`ChainReader::latest_block`) pins every `refresh`; the `PoolSnapshot` records it.
- Every task ends with green tests and a commit. Conventional-commit messages.

## File Structure

```
src/
  lib.rs                       # module tree
  main.rs                      # bootstrap entry
  settings.rs                  # Settings.toml parsing
  setup.rs                     # per-chain wiring
  primitives/
    mod.rs  asset.rs  pool.rs  chain.rs  opportunity.rs
  core/
    mod.rs
    deps/    mod.rs  pool.rs  exchange.rs  pool_store.rs  valuation.rs  notifier.rs  chain_reader.rs
    application/  mod.rs  graph.rs  finder.rs  detect.rs  sizing.rs  rank.rs  validation.rs  scanner.rs  config.rs
  adapters/
    mod.rs
    exchanges/  mod.rs  uniswap_v2.rs  uniswap_v3.rs  uniswap_v4.rs  curve.rs  curve_crypto.rs  aerodrome.rs
    pool_store/ mod.rs  store.rs  sync.rs
    chain_reader/ mod.rs
    valuation/  mod.rs
    notifier/   mod.rs  log.rs  memory.rs  composite.rs
    api/        mod.rs  handlers.rs  server.rs
    vendored/   mod.rs  (provider.rs, multicall.rs, fiat.rs, retry.rs — copied garden-rs)
  test_utils.rs                # in-memory port fakes (cfg(test) or a `testing` feature)
```

---

## Phase 0 — Foundations

### Task 0.1: Cargo manifest + module skeleton

**Files:**
- Modify: `Cargo.toml`
- Create: `src/lib.rs`, `src/primitives/mod.rs`, `src/core/mod.rs`, `src/core/deps/mod.rs`, `src/core/application/mod.rs`, `src/adapters/mod.rs`

**Interfaces:**
- Produces: the crate compiles as a lib + bin; empty module tree.

- [ ] **Step 1: Write `Cargo.toml`**

```toml
[package]
name = "arb-router"
version = "0.1.0"
edition = "2024"

[lib]
path = "src/lib.rs"

[[bin]]
name = "arb-router"
path = "src/main.rs"

[dependencies]
tokio = { version = "1", features = ["full"] }
async-trait = "0.1"
thiserror = "2"
rust_decimal = { version = "1", features = ["serde"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
arc-swap = "1"
moka = { version = "0.12", features = ["future"] }
axum = "0.7"
config = "0.15"
tracing = "0.1"
tracing-subscriber = "0.3"
eyre = "0.6"
time = { version = "0.3", features = ["serde", "macros"] }
# adapters only (chain + vendored):
alloy = { version = "1", features = ["full", "providers"] }
reqwest = { version = "0.12", features = ["json"] }

[dev-dependencies]
tokio = { version = "1", features = ["full", "test-util", "macros"] }
```

- [ ] **Step 2: Write module tree** — `src/lib.rs`:

```rust
pub mod primitives;
pub mod core;
pub mod adapters;
pub mod settings;
pub mod setup;
```

Create each `mod.rs` empty (e.g. `src/core/mod.rs` = `pub mod deps;\npub mod application;`). Create empty `src/settings.rs`, `src/setup.rs` (with `// wiring`), leave `src/main.rs` as-is for now.

- [ ] **Step 3: Verify it compiles**

Run: `cargo build`
Expected: builds (warnings for empty modules OK).

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "chore: cargo manifest + hexagonal module skeleton"
```

---

## Phase 1 (M1) — Primitives + Pool quoting core

### Task 1.1: `primitives/asset.rs` — asset & money value types

**Files:**
- Create: `src/primitives/asset.rs`
- Modify: `src/primitives/mod.rs` (add `pub mod asset;`)

**Interfaces:**
- Produces:
  - `AssetId(String)` with `AssetId::new(&str) -> Result<Self, AssetIdError>` (validates `chain:token`), `fn chain(&self) -> &str`, `fn as_str(&self) -> &str`.
  - `ChainId(String)` with `ChainId::new(&str) -> Self`, `fn as_str`.
  - `Amount(Decimal)` (base units) with `fn zero()`, `Add/Sub`, `PartialOrd`.
  - `Usd(Decimal)` with `Add`, `PartialOrd`.
  - `Pair { source: AssetId, destination: AssetId }`.
  - `AssetMeta { decimals: u8, symbol: String }`.

- [ ] **Step 1: Write failing tests** (`#[cfg(test)] mod tests` at bottom of `asset.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn asset_id_parses_chain_and_token() {
        let a = AssetId::new("ethereum:usdc").unwrap();
        assert_eq!(a.chain(), "ethereum");
        assert_eq!(a.as_str(), "ethereum:usdc");
    }
    #[test]
    fn asset_id_rejects_missing_colon() {
        assert!(AssetId::new("ethereum").is_err());
    }
    #[test]
    fn amount_orders_and_adds() {
        use rust_decimal::Decimal;
        let a = Amount(Decimal::from(100));
        let b = Amount(Decimal::from(250));
        assert!(b > a);
        assert_eq!((a + b), Amount(Decimal::from(350)));
    }
}
```

- [ ] **Step 2: Run — expect FAIL** (types not defined)

Run: `cargo test --lib primitives::asset`
Expected: FAIL (unresolved `AssetId`).

- [ ] **Step 3: Implement `asset.rs`**

```rust
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::ops::{Add, Sub};

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum AssetIdError {
    #[error("asset id must be `chain:token`, got `{0}`")]
    BadFormat(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AssetId(String);
impl AssetId {
    pub fn new(s: &str) -> Result<Self, AssetIdError> {
        let mut parts = s.splitn(2, ':');
        match (parts.next(), parts.next()) {
            (Some(c), Some(t)) if !c.is_empty() && !t.is_empty() => Ok(Self(s.to_string())),
            _ => Err(AssetIdError::BadFormat(s.to_string())),
        }
    }
    pub fn chain(&self) -> &str { self.0.split(':').next().unwrap_or("") }
    pub fn as_str(&self) -> &str { &self.0 }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChainId(String);
impl ChainId {
    pub fn new(s: &str) -> Self { Self(s.to_string()) }
    pub fn as_str(&self) -> &str { &self.0 }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Amount(pub Decimal);
impl Amount { pub fn zero() -> Self { Self(Decimal::ZERO) } }
impl Add for Amount { type Output = Amount; fn add(self, o: Amount) -> Amount { Amount(self.0 + o.0) } }
impl Sub for Amount { type Output = Amount; fn sub(self, o: Amount) -> Amount { Amount(self.0 - o.0) } }

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Usd(pub Decimal);
impl Add for Usd { type Output = Usd; fn add(self, o: Usd) -> Usd { Usd(self.0 + o.0) } }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pair { pub source: AssetId, pub destination: AssetId }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetMeta { pub decimals: u8, pub symbol: String }
```
Note the `unwrap_or("")` in `chain()` is not a panic; it's inside a non-test fn but is infallible-by-construction — acceptable (no `.unwrap()`). Add `pub mod asset;` to `primitives/mod.rs`.

- [ ] **Step 4: Run — expect PASS**

Run: `cargo test --lib primitives::asset`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(primitives): AssetId, ChainId, Amount, Usd, Pair, AssetMeta"
```

### Task 1.2: `primitives/pool.rs` — pool & exchange identity

**Files:** Create `src/primitives/pool.rs`; modify `primitives/mod.rs`.

**Interfaces:**
- Produces: `PoolId(String)` (`new`, `as_str`), `ExchangeId(String)` (`new`, `as_str`), `PoolKey { exchange: ExchangeId, chain: ChainId, address: String, assets: Vec<AssetId>, fee_bps: Option<u32> }`.

- [ ] **Step 1: Failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pool_id_roundtrips() {
        let p = PoolId::new("ethereum:univ3:0xabc");
        assert_eq!(p.as_str(), "ethereum:univ3:0xabc");
    }
}
```

- [ ] **Step 2: Run — FAIL.** `cargo test --lib primitives::pool`
- [ ] **Step 3: Implement** the three types (newtypes + struct, `Clone,Debug,PartialEq,Eq,Hash` on ids; `serde` derive). Add `pub mod pool;`.
- [ ] **Step 4: Run — PASS.** `cargo test --lib primitives::pool`
- [ ] **Step 5: Commit** `feat(primitives): PoolId, ExchangeId, PoolKey`

### Task 1.3: `primitives/chain.rs` — chain read types

**Files:** Create `src/primitives/chain.rs`; modify `mod.rs`.

**Interfaces:**
- Produces: `Bytes(Vec<u8>)`; `Call { target: String, calldata: Bytes }`; `CallResult { success: bool, data: Bytes }`; `BlockId { Latest, Number(u64) }`; `BatchOutput { block: u64, results: Vec<CallResult> }`.

- [ ] **Step 1: Failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn call_result_reports_failure() {
        let r = CallResult { success: false, data: Bytes(vec![]) };
        assert!(!r.success);
    }
}
```

- [ ] **Step 2: Run — FAIL.** `cargo test --lib primitives::chain`
- [ ] **Step 3: Implement** the types (all `Clone, Debug, PartialEq`). Add `pub mod chain;`.
- [ ] **Step 4: Run — PASS.**
- [ ] **Step 5: Commit** `feat(primitives): Call, CallResult, BlockId, BatchOutput`

### Task 1.4: `primitives/opportunity.rs` — Path & Opportunity

**Files:** Create `src/primitives/opportunity.rs`; modify `mod.rs`.

**Interfaces:**
- Consumes: `AssetId`, `ChainId`, `Amount`, `Usd`, `PoolId`, `Pair`.
- Produces:
  - `Hop { pool: PoolId, pair: Pair }`.
  - `Path { start: AssetId, hops: Vec<Hop> }` with `fn destination(&self) -> &AssetId`, `fn is_cycle(&self) -> bool`, `fn canonical_key(&self) -> String` (rotate cycle to min pool id for dedup).
  - `Opportunity { chain: ChainId, path: Path, input: Amount, output: Amount, profit_usd: Option<Usd>, roi_bps: u32, detected_at: time::OffsetDateTime, worst_pool_synced_at: time::OffsetDateTime }`.

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::asset::{AssetId, Pair};
    use crate::primitives::pool::PoolId;
    fn hop(p: &str, s: &str, d: &str) -> Hop {
        Hop { pool: PoolId::new(p), pair: Pair { source: AssetId::new(s).unwrap(), destination: AssetId::new(d).unwrap() } }
    }
    #[test]
    fn detects_cycle() {
        let p = Path { start: AssetId::new("ethereum:usdc").unwrap(),
            hops: vec![hop("p1","ethereum:usdc","ethereum:weth"), hop("p2","ethereum:weth","ethereum:usdc")] };
        assert!(p.is_cycle());
        assert_eq!(p.destination().as_str(), "ethereum:usdc");
    }
    #[test]
    fn non_cycle_when_dest_differs() {
        let p = Path { start: AssetId::new("ethereum:usdc").unwrap(),
            hops: vec![hop("p1","ethereum:usdc","ethereum:weth")] };
        assert!(!p.is_cycle());
    }
}
```

- [ ] **Step 2: Run — FAIL.** `cargo test --lib primitives::opportunity`
- [ ] **Step 3: Implement.** `destination()` = last hop's `pair.destination` (or `start` if no hops); `is_cycle()` = `destination() == start`; `canonical_key()` = join hop pool ids, rotate so the lexicographically-smallest pool id is first (stable dedup of rotations). Add `pub mod opportunity;`.
- [ ] **Step 4: Run — PASS.**
- [ ] **Step 5: Commit** `feat(primitives): Path, Hop, Opportunity`

### Task 1.5: `core/deps/pool.rs` — the `Pool` trait

**Files:** Create `src/core/deps/pool.rs`; modify `core/deps/mod.rs`.

**Interfaces:**
- Consumes: `AssetId`, `Pair`, `Amount`, `PoolId`.
- Produces:
```rust
pub trait Pool: Send + Sync {
    fn id(&self) -> PoolId;
    fn assets(&self) -> &[AssetId];
    fn quote(&self, pair: &Pair, amount_in: Amount) -> Option<Amount>;
}
```

- [ ] **Step 1: Write the trait + a `FakePool` in `#[cfg(test)]`** that quotes a fixed rate, and a test asserting the trait is object-safe (`let _: Box<dyn Pool> = ...`) and `quote` returns `amount_in * rate`.
- [ ] **Step 2: Run — FAIL.** `cargo test --lib core::deps::pool`
- [ ] **Step 3: Implement** the trait (no impl body — it's a trait). Add `pub mod pool;` to `deps/mod.rs`. Keep `FakePool` in the test module.
- [ ] **Step 4: Run — PASS.**
- [ ] **Step 5: Commit** `feat(core/deps): Pool trait`

### Task 1.6: Uniswap V2 `Pool` impl (copy munger math) + golden vectors

**Files:**
- Create: `src/adapters/exchanges/uniswap_v2.rs`, `src/adapters/exchanges/mod.rs`
- Modify: `src/adapters/mod.rs` (add `pub mod exchanges;`)
- Reference to copy: `munger/crates/dex/src/amm/pools/uniswap_v2.rs`

**Interfaces:**
- Consumes: `Pool` trait, `AssetId`, `Pair`, `Amount`, `PoolId`.
- Produces: `struct UniswapV2Pool { id: PoolId, token0: AssetId, token1: AssetId, reserve0: U256, reserve1: U256, fee_bps: u32, decimals0: u8, decimals1: u8 }` implementing `Pool`.

**Notes:** The V2 formula is `out = (in*(10000-fee)*reserve_out) / (reserve_in*10000 + in*(10000-fee))`, all in base-unit integers (`alloy::primitives::U256`). Convert `Amount` (base-unit `Decimal`) → `U256` at entry, run integer math, convert result `U256` → `Amount`. Keep the conversion in a small private helper `amount_to_u256`/`u256_to_amount`.

- [ ] **Step 1: Write the golden-vector test.** Pick a known mainnet pool state (from an etherscan `getReserves` read or munger's V2 tests) and assert the output for a fixed input. Example (USDC/WETH-shaped, values illustrative — replace with a real captured triple during impl):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::asset::{AssetId, Amount, Pair};
    use rust_decimal::Decimal;
    #[test]
    fn v2_quote_matches_getamountsout() {
        // reserves captured from chain at a pinned block; expected from UniswapV2Library.getAmountOut
        let pool = UniswapV2Pool::from_reserves(
            "ethereum:univ2:0xB4e16d", "ethereum:usdc", "ethereum:weth",
            /*reserve0 usdc*/ 50_000_000_000u128.into(), /*reserve1 weth*/ 20_000_000_000_000_000_000u128.into(),
            30, 6, 18);
        let out = pool.quote(
            &Pair { source: AssetId::new("ethereum:usdc").unwrap(), destination: AssetId::new("ethereum:weth").unwrap() },
            Amount(Decimal::from(1_000_000u64)), // 1 USDC in base units
        ).unwrap();
        assert_eq!(out, Amount(Decimal::from(399_201_596_806_387u64))); // replace with the real getAmountOut value
    }
}
```

- [ ] **Step 2: Run — FAIL.** `cargo test --lib adapters::exchanges::uniswap_v2`
- [ ] **Step 3: Implement.** Port munger's `simulate_swap` constant-product math verbatim (it's `U256`). Add `from_reserves(...)` constructor, `amount_to_u256(amount, decimals)` (multiply is unnecessary — base units already; just `Decimal` → `U256` via string/mantissa), `u256_to_amount(u256, decimals)` (→ base-unit `Decimal`). Implement `Pool` (`assets()` returns `&[token0, token1]`; `quote` picks direction by `pair.source`).
- [ ] **Step 4: Run — PASS** (adjust the expected value to the real `getAmountOut` once reserves are captured).
- [ ] **Step 5: Commit** `feat(exchanges): Uniswap V2 Pool quoter with golden vector`

### Task 1.7: Uniswap V3 `Pool` impl + golden vectors

**Files:** Create `src/adapters/exchanges/uniswap_v3.rs`; modify `exchanges/mod.rs`. Copy from `munger/crates/dex/src/amm/pools/uniswap_v3.rs` (tick math, `simulate_v3_swap`).

**Interfaces:**
- Produces: `struct UniswapV3Pool { id, token0, token1, sqrt_price_x96: U256, liquidity: u128, tick: i32, ticks: Vec<TickInfo>, tick_bitmap: ..., fee: u32, decimals0, decimals1 }` implementing `Pool`. (Aerodrome Slipstream reuses this struct with a `protocol` flag — Task 5.4.)

- [ ] **Step 1: Golden-vector test** — pin a real V3 pool's `slot0` + a tick window at a block; assert `quote` matches an on-chain `QuoterV2.quoteExactInputSingle`. (Capture the triple during impl.)
- [ ] **Step 2: Run — FAIL.**
- [ ] **Step 3: Implement** — copy munger's V3 tick-crossing math verbatim (Q64.96); wire `Pool::quote`.
- [ ] **Step 4: Run — PASS.**
- [ ] **Step 5: Commit** `feat(exchanges): Uniswap V3 Pool quoter with golden vector`

> Tasks 1.8 (Uniswap V4), 1.9 (Curve stableswap), 1.10 (Curve crypto) follow the **same 5-step pattern** as 1.7: copy the corresponding `munger/crates/dex/src/amm/pools/{uniswap_v4,curve,curve_crypto_v1}.rs` math into `adapters/exchanges/{uniswap_v4,curve,curve_crypto}.rs`, write a golden-vector test against the on-chain quoter (V4 quoter / Curve `get_dy`), implement `Pool`, green, commit. Curve pools expose N assets — `assets()` returns all coins and `quote` maps `(source,destination)` to Curve `(i,j)` indices.

---

## Phase 2 (M2) — Application engine (pure, tested against fakes)

### Task 2.1: Remaining `core/deps` traits

**Files:** Create `src/core/deps/{exchange,pool_store,valuation,notifier,chain_reader}.rs`; modify `deps/mod.rs`.

**Interfaces:** (define exactly as the spec §4)
- `pool_store.rs`: `PoolStore` (`fn snapshot(&self, chain: &ChainId) -> Arc<PoolSnapshot>`), plus `PoolSnapshot`, `PoolEntry { pool: Arc<dyn Pool>, meta: PoolMeta }`, `PoolMeta { synced_block: u64, synced_at: OffsetDateTime }`. `PoolSnapshot` methods: `pools_from(&AssetId) -> &[PoolEntry]`, `get(&PoolId) -> Option<&PoolEntry>`, `assets() -> impl Iterator<Item=&AssetId>`, and a `PoolSnapshot::from_entries(block, taken_at, Vec<PoolEntry>)` constructor that builds the `pool_id → idx` and `asset → Vec<idx>` indexes.
- `valuation.rs`: `#[async_trait] Valuation { async fn price(&self, asset: &AssetId) -> Result<Usd, ValuationError>; }` + `ValuationError`.
- `notifier.rs`: `#[async_trait] Notifier { async fn notify(&self, chain: &ChainId, opps: &[Opportunity]) -> Result<(), NotifyError>; }` + `NotifyError`.
- `chain_reader.rs`: `#[async_trait] ChainReader { async fn latest_block(&self, chain: &ChainId) -> Result<u64, ChainReadError>; async fn call_batch(&self, chain: &ChainId, at: BlockId, calls: Vec<Call>) -> Result<BatchOutput, ChainReadError>; }` + `ChainReadError`.
- `exchange.rs`: `#[async_trait] Exchange { fn id(&self) -> ExchangeId; fn supports(&self, chain: &ChainId) -> bool; async fn discover(&self, chain: &ChainId, tokens: &[AssetId], reader: &dyn ChainReader) -> Result<Vec<PoolKey>, ExchangeError>; async fn refresh(&self, keys: &[PoolKey], at: BlockId, reader: &dyn ChainReader) -> Result<Vec<Box<dyn Pool>>, ExchangeError>; }` + `ExchangeError`.

- [ ] **Step 1: Write a `PoolSnapshot` unit test** — build from two `FakePool` entries, assert `pools_from(usdc)` returns the pools touching USDC and `get(id)` resolves.
- [ ] **Step 2: Run — FAIL.** `cargo test --lib core::deps::pool_store`
- [ ] **Step 3: Implement** all five files (traits + the `PoolSnapshot` struct/indexes). Errors as `thiserror` enums with at least `Internal(String)` + domain variants (`ValuationError::NotFound`, `ChainReadError::Transport`, etc.).
- [ ] **Step 4: Run — PASS.**
- [ ] **Step 5: Commit** `feat(core/deps): Exchange, PoolStore+PoolSnapshot, Valuation, Notifier, ChainReader`

### Task 2.2: In-memory port fakes (`test_utils.rs`)

**Files:** Create `src/test_utils.rs`; add `#[cfg(test)] pub mod test_utils;` to `lib.rs`.

**Interfaces:**
- Produces: `FakePool` (fixed-rate quoter), `fake_snapshot(entries) -> Arc<PoolSnapshot>`, `FakeValuation(HashMap<AssetId, Usd>)` impl `Valuation`, `RecordingNotifier(Arc<Mutex<Vec<Opportunity>>>)` impl `Notifier`.

- [ ] **Step 1: Write a test** in `test_utils` that builds a `FakeValuation`, prices USDC=1.0, asserts `price` returns it.
- [ ] **Step 2: Run — FAIL.**
- [ ] **Step 3: Implement** the fakes.
- [ ] **Step 4: Run — PASS.**
- [ ] **Step 5: Commit** `test: in-memory fakes for Pool/Valuation/Notifier + snapshot builder`

### Task 2.3: `core/application/config.rs` — `EngineConfig`

**Files:** Create `src/core/application/config.rs`; modify `application/mod.rs`.

**Interfaces:**
- Produces: `EngineConfig { max_hops: usize, beam_width: usize, max_paths_per_scan: usize, min_roi_bps: u32, min_profit_usd: Usd, start_assets: Vec<AssetId>, min_input: Amount, max_input: Amount, max_pool_staleness: Duration }` + `Default`-ish constructor used by tests.

- [ ] Steps 1–5 (TDD): test that `EngineConfig` builds with `max_hops = 16`; implement the struct; green; commit `feat(application): EngineConfig`.

### Task 2.4: `graph.rs` — build the asset graph from a snapshot

**Files:** Create `src/core/application/graph.rs`.

**Interfaces:**
- Consumes: `PoolSnapshot`, `AssetId`, `PoolId`.
- Produces: `struct Graph<'a>(&'a PoolSnapshot)` with `fn edges_from(&self, asset: &AssetId) -> impl Iterator<Item = Edge<'a>>` where `Edge { pool: PoolId, from: AssetId, to: AssetId }` (a pool contributes one edge per other-asset; N-asset pools yield N-1 edges from a given asset).

- [ ] **Step 1: Failing test** — snapshot with one USDC/WETH `FakePool`; `edges_from(usdc)` yields exactly one edge `usdc→weth` with that pool id; `edges_from(weth)` yields `weth→usdc`.
- [ ] **Step 2: Run — FAIL.** `cargo test --lib core::application::graph`
- [ ] **Step 3: Implement** — `edges_from` iterates `snapshot.pools_from(asset)`, and for each pool emits an edge to every *other* asset in `pool.assets()`.
- [ ] **Step 4: Run — PASS.**
- [ ] **Step 5: Commit** `feat(application): asset graph over PoolSnapshot`

### Task 2.5: `finder.rs` — bounded path enumeration with guards

**Files:** Create `src/core/application/finder.rs`.

**Interfaces:**
- Consumes: `Graph`, `EngineConfig`, `AssetId`, `PoolId`, `Path`, `Hop`, `Pair`.
- Produces: `fn find_paths(graph: &Graph, start: &AssetId, cfg: &EngineConfig) -> Vec<Path>` — DFS from `start`, emitting a `Path` at **every reached asset** (so cross-asset endpoints are captured, not just cycles), subject to: depth ≤ `max_hops`; a `PoolId` used at most once per path (visited-pool set); stop expanding when `max_paths_per_scan` reached (return what we have + a dropped count via `tracing::warn!`); `beam_width` caps the frontier per depth (keep, for now, the first `beam_width` — branch-and-bound value pruning arrives with quoting in 2.6 and is layered in as an optional comparator).

- [ ] **Step 1: Failing tests**
  - Triangle graph `A-B`, `B-C`, `A-C` (each a distinct pool): `find_paths(A, max_hops=3)` includes the cycle `A→B→C→A` and the 1-hop `A→C`.
  - Two pools between A and B (`p1`, `p2`): `find_paths(A, max_hops=2)` includes `A→(p1)B→(p2)A` but **not** `A→(p1)B→(p1)A` (no pool reuse).
  - `max_hops=1` yields only 1-hop paths.

```rust
#[test]
fn enumerates_cycles_and_no_pool_reuse() {
    // build a snapshot with pools p1,p2 both A/B ; assert the two properties above
}
```

- [ ] **Step 2: Run — FAIL.** `cargo test --lib core::application::finder`
- [ ] **Step 3: Implement** the recursive DFS with a `Vec<PoolId>` visited-pool guard and a `&mut usize` path counter; push a `Path` clone at each node (including intermediate assets); honor `max_hops`, `max_paths_per_scan`, `beam_width`.
- [ ] **Step 4: Run — PASS.**
- [ ] **Step 5: Commit** `feat(application): bounded path finder (no-pool-reuse, beam/budget)`

### Task 2.6: `detect.rs` — quote paths & compute profit

**Files:** Create `src/core/application/detect.rs`.

**Interfaces:**
- Consumes: `PoolSnapshot`, `Path`, `Amount`, `Usd`, `Valuation`, `AssetRegistry` (a `HashMap<AssetId, AssetMeta>` passed in), `EngineConfig`.
- Produces:
  - `fn quote_path(snapshot: &PoolSnapshot, path: &Path, input: Amount) -> Option<Amount>` — chain `pool.quote` hop by hop (resolving via `snapshot.get`), threading the running amount; `None` if any hop fails.
  - `async fn profit_usd(valuation: &dyn Valuation, registry: &AssetRegistry, asset: &AssetId, amount: Amount) -> Option<Usd>` — `amount / 10^decimals × price`.
  - `struct Priced { path: Path, input: Amount, output: Amount, profit_usd: Option<Usd> }` and `is_profitable(&Priced, cfg) -> bool` (cycle: `output > input`; cross-asset: `value(dest,output) > value(source,input)`).

- [ ] **Step 1: Failing tests**
  - `quote_path` on a 2-hop cycle with `FakePool`s (rate 1.05 then 1.0) turns 1000 → 1050.
  - `is_profitable` true for that cycle (output>input); false when rates multiply < 1.
  - cross-asset: input USDC valued vs output WETH via `FakeValuation` → profitable when USD out > USD in.
- [ ] **Step 2: Run — FAIL.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run — PASS.**
- [ ] **Step 5: Commit** `feat(application): path quoting + cycle/cross-asset profit`

### Task 2.7: `sizing.rs` — optimal input search

**Files:** Create `src/core/application/sizing.rs`.

**Interfaces:**
- Consumes: `quote_path`, `Path`, `Amount`, `EngineConfig`.
- Produces: `fn best_input(snapshot, path, cfg) -> Option<(Amount, Amount)>` — golden-section search over `[min_input, max_input]` maximizing `output(x) - x` (for cycles) or `output(x)` value (cross-asset); returns `(input, output)` at the optimum. Fixed iteration count (e.g. 40) — deterministic, no infinite loop.

- [ ] **Step 1: Failing test** — a synthetic concave profit function (via a `FakePool` whose output has diminishing returns) has its optimum found within a tolerance.
- [ ] **Step 2: Run — FAIL.**
- [ ] **Step 3: Implement** golden-section (ratio `0.618`), N fixed iterations.
- [ ] **Step 4: Run — PASS.**
- [ ] **Step 5: Commit** `feat(application): golden-section input sizing`

### Task 2.8: `validation.rs` — freshness guard

**Files:** Create `src/core/application/validation.rs`.

**Interfaces:**
- Consumes: `PoolSnapshot`, `Path`, `EngineConfig`, `OffsetDateTime`.
- Produces: `fn is_fresh(snapshot, path, now, max_staleness) -> bool` — every hop's `PoolMeta.synced_at` is within `max_staleness` of `now`; `fn worst_synced_at(snapshot, path) -> OffsetDateTime`.

- [ ] Steps 1–5 (TDD): test a path with one stale pool fails `is_fresh`; implement; green; commit `feat(application): pool freshness guard`.

### Task 2.9: `rank.rs` — USD ranking + dedup

**Files:** Create `src/core/application/rank.rs`.

**Interfaces:**
- Consumes: `Opportunity`, `Path::canonical_key`, `Usd`.
- Produces: `fn rank_and_dedup(opps: Vec<Opportunity>) -> Vec<Opportunity>` — drop duplicates by `path.canonical_key()`, keep the higher-profit of a duplicate pair, sort by `profit_usd` desc (opps without a USD value sink below those with one, tie-broken by `roi_bps`).

- [ ] Steps 1–5 (TDD): two rotations of the same cycle dedup to one; higher-USD first. Commit `feat(application): rank + dedup opportunities`.

### Task 2.10: `scanner.rs` — per-chain runner (assembled, fakes)

**Files:** Create `src/core/application/scanner.rs`.

**Interfaces:**
- Consumes: everything above + `PoolStore`, `Valuation`, `Notifier`, `AssetRegistry`, `EngineConfig`.
- Produces:
```rust
pub struct Scanner<S: PoolStore, V: Valuation, N: Notifier> {
    pub chain: ChainId, pub pool_store: Arc<S>, pub valuation: Arc<V>,
    pub notifier: Arc<N>, pub registry: AssetRegistry, pub cfg: EngineConfig,
}
impl<S,V,N> Scanner<S,V,N> {
    pub async fn scan_once(&self) -> Result<usize, ScanError>; // one tick; returns #opportunities emitted
    pub async fn run(self: Arc<Self>, interval: Duration);     // loop { scan_once; sleep }
}
```
`scan_once`: `snapshot = pool_store.snapshot(chain)` → build graph → for each `start_asset` `find_paths` → `quote_path` + `is_profitable` → `best_input` → build `Opportunity` (with `profit_usd`, `roi_bps`, freshness) → `is_fresh` filter → `rank_and_dedup` → `notifier.notify(chain, &opps)`. A tick error is logged and returns `Ok(0)`-equivalent (never panics).

- [ ] **Step 1: Failing integration test** — assemble a `Scanner` over a fake snapshot containing a real arbitrage cycle (two pools that round-trip > 1), `FakeValuation`, `RecordingNotifier`; assert `scan_once()` emits exactly the expected cycle with `output > input`.
- [ ] **Step 2: Run — FAIL.** `cargo test --lib core::application::scanner`
- [ ] **Step 3: Implement** `scan_once` (and `run`).
- [ ] **Step 4: Run — PASS.**
- [ ] **Step 5: Commit** `feat(application): Scanner tick end-to-end over fakes`

**Milestone M2 gate:** `cargo test --lib` green; the engine detects arbitrage end-to-end with zero real I/O.

---

## Phase 3 (M3) — State sync (one chain, Uniswap V3)

### Task 3.1: `adapters/vendored` — garden helpers (provider, multicall, retry)

**Files:** Create `src/adapters/vendored/{mod.rs,provider.rs,multicall.rs,retry.rs}`; modify `adapters/mod.rs`.

**Notes:** Copy the minimal provider construction + Multicall3 binding + `retry_with_backoff` from `garden-rs` (as `evm-executor` uses them) with a `// Vendored from garden-rs @ <rev>` header on each file. Expose `fn make_provider(rpc_url) -> AlloyProvider` and a `Multicall3` instance helper.

- [ ] **Step 1: Test** — `retry_with_backoff` retries a closure that fails once then succeeds (pure, no network).
- [ ] **Step 2–4:** FAIL → copy/adapt → PASS.
- [ ] **Step 5: Commit** `chore(vendored): garden-rs provider/multicall/retry helpers`

### Task 3.2: `adapters/chain_reader` — `ChainReader` impl

**Files:** Create `src/adapters/chain_reader/mod.rs`.

**Interfaces:**
- Produces: `struct MulticallChainReader { providers: HashMap<ChainId, AlloyProvider>, multicall: HashMap<ChainId, Address>, chunk_size: usize }` impl `ChainReader`.
- `latest_block`: `provider.get_block_number()`. `call_batch`: resolve `BlockId::Latest → Number(n)` once; split `calls` into `chunk_size` chunks; for each, build a Multicall3 `tryAggregate(false, calls)` call `.block(n)`; map each `Result{success,returnData}` → `CallResult`; concat in order; return `BatchOutput { block: n, results }`. Per-call revert ⇒ `success=false` (never an `Err`); transport error ⇒ `Err(ChainReadError::Transport)`.

- [ ] **Step 1: Failing test (mocked)** — unit-test the **chunking + ordering** logic with an injected fake "aggregator" fn (no live RPC): 5 calls, `chunk_size=2` → 3 chunks, results concatenated in original order; one failing sub-call surfaces as `success=false`.
- [ ] **Step 2: Run — FAIL.**
- [ ] **Step 3: Implement**; keep the aggregate call behind a small trait so the test injects a fake and prod uses Multicall3.
- [ ] **Step 4: Run — PASS.**
- [ ] **Step 5: Commit** `feat(chain_reader): block-pinned, chunked Multicall3 reader`

- [ ] **Step 6 (integration, ignored by default):** add `#[ignore]` test hitting a real RPC (`ETH_RPC_URL` env) that reads one known pool's reserves and checks the block is pinned. Run manually: `ETH_RPC_URL=... cargo test --lib -- --ignored chain_reader`.

### Task 3.3: `adapters/exchanges/uniswap_v3` — `Exchange` impl

**Files:** Modify `src/adapters/exchanges/uniswap_v3.rs` (add the `Exchange` impl alongside the `Pool` from 1.7).

**Interfaces:**
- Consumes: `Exchange` trait, `ChainReader`, `PoolKey`, `UniswapV3Pool`.
- Produces: `struct UniswapV3Exchange { id: ExchangeId, chain: ChainId, factory: Address, fee_tiers: Vec<u32> }` impl `Exchange`.
- `discover`: for each tracked-token pair × fee tier, `factory.getPool(a,b,fee)` via `reader.call_batch` (one batch), keep non-zero addresses → `PoolKey`s.
- `refresh`: for each pool, batch-read `slot0` + `liquidity` + the tick-window words around the active tick at block `at` (dependent read: first `slot0` to learn the tick, then the tick window — two `call_batch` rounds), decode into `UniswapV3Pool`, box it.

- [ ] **Step 1: Failing test** — with a `FakeChainReader` returning canned `slot0`/tick bytes for one pool, `refresh` produces one `UniswapV3Pool` whose `quote` matches the golden vector from 1.7.
- [ ] **Step 2: Run — FAIL.**
- [ ] **Step 3: Implement** discover + refresh (ABI encode/decode with alloy `sol!` bindings for `getPool`, `slot0`, `liquidity`, `ticks`).
- [ ] **Step 4: Run — PASS.**
- [ ] **Step 5: Commit** `feat(exchanges): Uniswap V3 Exchange (discover + block-pinned refresh)`

### Task 3.4: `adapters/pool_store` — `PoolStore` impl + sync worker

**Files:** Create `src/adapters/pool_store/{mod.rs,store.rs,sync.rs}`.

**Interfaces:**
- Produces:
  - `struct ArcSwapPoolStore { snapshots: HashMap<ChainId, ArcSwap<PoolSnapshot>> }` impl `PoolStore` (`snapshot` = `snapshots[chain].load_full()`).
  - `struct SyncWorker { chain, store: Arc<ArcSwapPoolStore>, exchanges: Vec<Arc<dyn Exchange>>, reader: Arc<dyn ChainReader>, tracked_tokens: Vec<AssetId>, registry: AssetRegistry, interval: Duration }` with `async fn run(self)` that each tick: `at = reader.latest_block(chain)`; for each supporting exchange `discover`(cached) + `refresh(keys, at, reader)`; build `Vec<PoolEntry>` (with `PoolMeta{ synced_block: at, synced_at: now }`); `store.snapshots[chain].store(Arc::new(PoolSnapshot::from_entries(at, now, entries)))`.

- [ ] **Step 1: Failing test** — a `SyncWorker` with a fake `Exchange` (returns one `FakePool`) and a `FakeChainReader`; after one tick, `store.snapshot(chain).pools_from(usdc)` returns the pool and `snapshot.block == fake_block`.
- [ ] **Step 2: Run — FAIL.**
- [ ] **Step 3: Implement** store + one-tick `refresh_once()` (factored out of `run` for testability).
- [ ] **Step 4: Run — PASS.**
- [ ] **Step 5: Commit** `feat(pool_store): arc-swap snapshot store + per-chain sync worker`

**Milestone M3 gate:** with a real `ETH_RPC_URL`, a manual run syncs Uniswap V3 pools for a token set and serves a consistent snapshot (verified by the ignored integration test).

---

## Phase 4 (M4) — Wiring + API

### Task 4.1: `settings.rs` — config parsing

**Files:** Create `src/settings.rs` content.

**Interfaces:**
- Produces: `Settings { scan_interval_ms, max_hops, beam_width, max_paths_per_scan, min_roi_bps, fiat_provider_url, chains: Vec<ChainSettings>, assets: Vec<AssetSettings> }`, `ChainSettings { chain_identifier, rpc_url, multicall_address, sync_interval_ms, start_assets, tracked_tokens, min_input_usd, max_input_usd, uniswap_v3_factories, ... }`, `AssetSettings { id, address, decimals, symbol, price_id }`, and `Settings::load(path) -> eyre::Result<Settings>` (via the `config` crate), plus `fn asset_registry(&self) -> AssetRegistry`.

- [ ] Steps 1–5 (TDD): parse a sample `Settings.toml` string; assert `max_hops == 16` and `asset_registry()` maps `ethereum:usdc → decimals 6`; implement; green; commit `feat: Settings.toml parsing + AssetRegistry`.

### Task 4.2: `adapters/valuation` — `Valuation` impl

**Files:** Create `src/adapters/valuation/mod.rs`; vendor a minimal fiat client into `adapters/vendored/fiat.rs`.

**Interfaces:**
- Produces: `struct FiatValuation { client, price_ids: HashMap<AssetId,String>, cache: moka::future::Cache<AssetId, Usd> }` impl `Valuation` — `price` checks the TTL cache, else fetches from the configured `fiat_provider_url`/CoinGecko by `price_id`, caches, returns. Missing id ⇒ `ValuationError::NotFound`.

- [ ] **Step 1: Failing test** — with `wiremock` (add dev-dep) stubbing the fiat endpoint, `price(usdc)` returns `Usd(1.0)` and a second call is served from cache (one HTTP hit).
- [ ] **Step 2–4:** FAIL → implement → PASS.
- [ ] **Step 5: Commit** `feat(valuation): cached fiat-backed Valuation`

### Task 4.3: `adapters/notifier` — log + memory + composite

**Files:** Create `src/adapters/notifier/{mod.rs,log.rs,memory.rs,composite.rs}`.

**Interfaces:**
- Produces:
  - `LogNotifier` impl `Notifier` (structured `tracing::info!` per opportunity; appends).
  - `MemoryNotifier { latest: RwLock<HashMap<ChainId, Vec<Opportunity>>> }` impl `Notifier` (**replaces** per chain; empty clears) + `fn get(&self, chain) -> Vec<Opportunity>` and `fn all(&self) -> Vec<Opportunity>` for the API.
  - `CompositeNotifier(Vec<Arc<dyn Notifier>>)` impl `Notifier` — calls each; a child error is logged, **never** propagated.

- [ ] **Step 1: Failing tests** — `MemoryNotifier` replace semantics (emit `[a,b]` then `[]` clears the chain); `CompositeNotifier` calls all children even if one errors.
- [ ] **Step 2–4:** FAIL → implement → PASS.
- [ ] **Step 5: Commit** `feat(notifier): log + in-memory(replace) + composite fan-out`

### Task 4.4: `adapters/api` — axum read endpoints

**Files:** Create `src/adapters/api/{mod.rs,handlers.rs,server.rs}`.

**Interfaces:**
- Consumes: `Arc<MemoryNotifier>`, per-chain status handles.
- Produces: `fn router(state: ApiState) -> axum::Router` with `GET /v1/health` → `{"status":"ok"}`; `GET /v1/opportunities?chain=&min_roi_bps=` → JSON of `MemoryNotifier` filtered; `GET /v1/status` → per-chain pool count / last sync block+time.

- [ ] **Step 1: Failing test** — `axum::http` test: `/v1/health` returns 200 `{"status":"ok"}`; `/v1/opportunities` returns the `MemoryNotifier` contents.
- [ ] **Step 2–4:** FAIL → implement → PASS.
- [ ] **Step 5: Commit** `feat(api): health/opportunities/status endpoints`

### Task 4.5: `setup.rs` + `main.rs` — per-chain wiring

**Files:** Write `src/setup.rs`, `src/main.rs`.

**Interfaces:**
- Consumes: all adapters + `Scanner` + `Settings`.
- Produces: `async fn build_and_run(settings: Settings) -> eyre::Result<()>` — build `MulticallChainReader`, the `Exchanges` registry (Uniswap V3 for now), `ArcSwapPoolStore`, `FiatValuation`, `CompositeNotifier([LogNotifier, MemoryNotifier])`; per chain spawn a `SyncWorker::run` **and** a `Scanner::run` on tokio tasks (mirroring `evm-executor`'s per-chain loop; a chain that fails to init is logged and skipped); start the axum server. `main` = load `Settings.toml`, `setup_tracing`, `build_and_run`.

- [ ] **Step 1: Failing test** — `build_and_run` with a 1-chain in-memory settings + fakes reaches "running" (health endpoint responds) without panicking. (Use a short-lived server + `tokio::time::timeout`.)
- [ ] **Step 2–4:** FAIL → implement → PASS.
- [ ] **Step 5: Commit** `feat: per-chain wiring (sync + scan + API) in setup/main`

**Milestone M4 gate:** `cargo run` with a real config starts sync + scan + API on one chain; `GET /v1/opportunities` returns live results.

---

## Phase 5 (M5) — Breadth

### Task 5.1–5.4: remaining `Exchange` impls

For each of **Uniswap V2**, **Uniswap V4**, **Curve** (+ **Curve crypto**), and **Aerodrome** (Slipstream = V3 impl with a `protocol` flag + Aerodrome factory/quoter addresses), add the `Exchange` impl next to its `Pool` (from Phase 1), following the **exact 5-step pattern of Task 3.3**: fake-reader unit test for `discover`+`refresh` → implement (factory + state decode) → green → register in the `Exchanges` registry in `setup.rs` → commit. One task per exchange.

### Task 5.5: multi-chain config + registration

**Files:** Modify `setup.rs`, `Settings.toml` sample.

- [ ] Add remaining EVM chains (Arbitrum, Optimism, Base, Polygon, BSC) to the sample config with their factory/multicall addresses; ensure `build_and_run` spawns a sync+scan pair per chain. Test: 2-chain in-memory settings spawns 2 scanners. Commit `feat: multi-chain wiring`.

**Milestone M5 gate:** all munger DEXs quote via golden vectors; all configured EVM chains scan.

---

## Self-Review (author checklist — completed)

- **Spec coverage:** every §4 port → a task (1.5, 2.1); every §5 application step → 2.4–2.10; sync §6 → 3.x; API §7 → 4.4; add-a-DEX §8 → 3.3/5.x pattern; vendoring §9/§11 → 3.1; config §10 → 4.1; error handling §11 → typed errors in every `deps` task; testing §12 → golden vectors (1.6–1.10) + fake-based unit tests + ignored integration tests; milestones §13 → phases. Detection scope (cycles + cross-asset) → 2.6. Traversal rule → 2.5. Snapshot consistency → 2.1/3.2/3.4.
- **Placeholders:** golden-vector *expected values* are captured during their task (a real on-chain read at a pinned block), not left as prose — each such step says exactly how to obtain it; not a plan placeholder.
- **Type consistency:** `Pool`, `Exchange`, `PoolStore`/`PoolSnapshot`/`PoolEntry`/`PoolMeta`, `Valuation::price`, `Notifier::notify`, `ChainReader::{latest_block,call_batch}`, `BlockId`, `BatchOutput`, `Path`/`Hop`/`Opportunity`, `AssetRegistry` used identically across tasks.
