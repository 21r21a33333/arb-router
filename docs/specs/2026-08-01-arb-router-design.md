# Arb Router — Design Spec (v1)

Date: 2026-08-02 (rev 10)
Status: Complete — ready for implementation plan (pending your final read)
Repo: `catalog/arb-router`

## 1. Problem & objective

Continuously **detect** arbitrage across on-chain EVM DEXs. Framed generally:

> Find a **source asset** and a **destination asset**, connected by a path of DEX hops,
> such that **`value(destination_out) > value(source_in)`**.

- A **cycle** (`destination == source`) is the special, oracle-free case: you end with more of the
  same token than you started with — guaranteed capturable.
- A **cross-asset** path (`destination != source`) compares two different assets, so it needs a
  common numeraire (USD) via a valuation oracle.

v1 **reports both** kinds. We **detect only** — execution is a later phase.

Because pool state is synced **locally**, quoting a hop is a **pure in-memory function call**, not a
metered RPC. That removes the API rate-limit problem entirely; we quote candidate paths directly
against real, amount-dependent pool math.

### v1 scope (locked)

| Axis | Decision |
| --- | --- |
| Repo | Separate, **self-contained** repo `catalog/arb-router` — no `garden-rs` git dependency; vendor the minimal helpers. |
| Chains | **EVM only, all EVM chains** (config-driven). No Solana. |
| Mode | **Detect + report only.** Execution deferred. |
| Detection | **Cycles (oracle-free) + cross-asset (oracle-valued).** |
| DEX set | munger's existing DEXs: **Uniswap V2/V3/V4, Curve, Curve-Crypto, Aerodrome.** New DEXs deferred but trivial to add. |
| Assets | The tokens munger already tracks (config-driven per chain). |

### Non-goals (v2+)
Execution/capture, Solana, cross-chain (bridge) paths, new DEX integrations, MEV/flashloans,
persistent opportunity storage.

## 2. Architecture — modified hexagonal (ports & adapters)

One crate. Strict layering following `evm-executor`/`munger`: **all port traits in `core/deps`,
their implementations in `adapters`, all value types in a top-level `primitives`, and the use-case
logic in `core/application`.** Domain code is pure (no I/O, no chain SDKs, no `unwrap`/`expect`);
`eyre` only in bootstrap/adapters; every boundary has a typed `thiserror` error.

```
src/
  primitives/              # pure value types — depend on NOTHING internal (not even deps)
    asset.rs               #   AssetId, ChainId, Amount (base units), Usd, Pair, AssetMeta
    pool.rs                #   PoolId, PoolKey, ExchangeId
    chain.rs               #   Call, CallResult, BlockId, BatchOutput
    opportunity.rs         #   Path (hops hold PoolId, not dyn Pool), Opportunity
    mod.rs
  core/
    deps/                  # ALL port traits (+ each one's typed thiserror error)
      pool.rs              #   Pool
      exchange.rs          #   Exchange
      pool_store.rs        #   PoolStore
      valuation.rs         #   Valuation
      notifier.rs          #   Notifier
      chain_reader.rs      #   ChainReader
      mod.rs
    application/           # use-case logic (depends on primitives + deps traits ONLY)
      graph.rs  finder.rs  detect.rs  sizing.rs  rank.rs  scanner.rs  validation.rs  mod.rs
    mod.rs
  adapters/                # ALL impls of deps traits + driving/infra (one dir per port)
    exchanges/             #   Exchange impls + their Pool impls (the copied munger math)
      uniswap_v2.rs  uniswap_v3.rs  uniswap_v4.rs  curve.rs  curve_crypto.rs  aerodrome.rs  mod.rs
    pool_store/            #   PoolStore impl + sync worker
    valuation/             #   Valuation impl (vendored fiat client)
    notifier/              #   Notifier impls (log + in-memory) + composite fan-out
    chain_reader/          #   ChainReader impl (vendored provider + Multicall3)
    api/                   #   driving adapter (axum)
    vendored/              #   copied-in garden-rs helpers (provenance headers)
    mod.rs
  settings.rs  setup.rs  lib.rs  main.rs   # bootstrap — wires adapters into application
```

### The strict rulebook — what lies where
| Layer | Contains | May depend on | Must NOT |
| --- | --- | --- | --- |
| `primitives/` | value objects & pure data (`AssetId`, `Amount`, `Pair`, `PoolId`, `PoolKey`, `Call`, `CallResult`, `Usd`, `ExchangeId`, `Path`, `Opportunity`) | std, `rust_decimal` | reference any `deps` trait; do I/O |
| `core/deps/` | **every** port trait + its typed error | `primitives` | contain impls; import `adapters` |
| `core/application/` | graph, find, detect, size, rank, scanner, validation | `primitives`, `core/deps` | import `adapters` or chain SDKs |
| `adapters/` | impls of every `deps` trait, driving API, vendored infra | `primitives`, `core/deps`, external SDKs | be imported by `core` |
| bootstrap | wiring only | everything | hold business logic |

**Two rules that settle the subtle cases:**
- **Primitives depend on nothing internal.** So `Path`/`Opportunity` hold `PoolId` (a primitive),
  **not** `Arc<dyn Pool>`. The application resolves ids → pools via `PoolStore` when it needs to quote.
- **"Adapter" = "knows an external system," not "does I/O."** The copied Uniswap/Curve math is *pure*
  but encodes *Uniswap's* rules → it lives in `adapters/exchanges`. Core only knows the `Pool`
  abstraction. This is exactly why "add a DEX" is a pure adapter change.

### Dependency graph (acyclic — enforced by the module boundaries)
```
primitives  ◄─ core/deps  ◄─ core/application
     ▲             ▲
     └──────── adapters ─────────┐
                                 ▼
                             bootstrap
```
core never imports adapters; deps never imports application; application never imports adapters.

## 3. Primitives (`primitives/`)
Newtypes over primitives (idiomatic; chain-agnostic; prevents mixups). No internal deps, no I/O.

- `AssetId(String)` — chain-agnostic (`"ethereum:usdc"`); carries its chain (cross-chain is a future
  extension with no model change). `ChainId` similarly.
- `Amount(Decimal)` — **base units** (wei/sat); no human conversion in `Pool`.
- `Pair { source: AssetId, destination: AssetId }`.
- `PoolId(String)`, `PoolKey { … }` (a discovered pool's identity for refresh).
- `Call`, `CallResult` — a chain read request/response (EVM-shaped for v1, behind the type).
- `Usd(Decimal)` — a USD value.
- `Path` — ordered hops `{ pool: PoolId, pair: Pair }` from a start asset (**holds ids, not traits**).
- `Opportunity` — chain, `Path`, optimal input, output, profit (start-asset + `Usd`), roi, timestamps,
  worst-pool freshness.

## 4. Ports (`core/deps/`) — every boundary trait lives here

### `Pool` — one liquidity source
Pure, object-safe. Handles 2-asset AMMs and N-asset pools (Curve/Balancer) via `assets()` + a pair.
```rust
trait Pool: Send + Sync {
    fn id(&self) -> PoolId;
    fn assets(&self) -> &[AssetId];
    fn quote(&self, pair: &Pair, amount_in: Amount) -> Option<Amount>;  // base-unit, exact-in, pure
}
```

### `Exchange` — a DEX integration (**the extension point**)
One impl per DEX. Owns discovery + state reads and produces `Pool`s. Uses the injected `ChainReader`,
so it never owns RPC.
```rust
#[async_trait]
trait Exchange: Send + Sync {
    fn id(&self) -> ExchangeId;
    fn supports(&self, chain: &ChainId) -> bool;
    async fn discover(&self, chain: &ChainId, tokens: &[AssetId], reader: &dyn ChainReader)
        -> Result<Vec<PoolKey>, ExchangeError>;
    async fn refresh(&self, keys: &[PoolKey], at: BlockId, reader: &dyn ChainReader)
        -> Result<Vec<Box<dyn Pool>>, ExchangeError>;   // block-pinned; V3/V4 need dependent reads
}
```

### `PoolStore` — the application's read model (snapshot-based)
Returns a **consistent, immutable, point-in-time** view per scan (lock-free; the sync worker swaps
snapshots atomically via `arc-swap`). The scan reads one snapshot throughout a tick — this fixes
torn multi-hop state and avoids a read-lock + `Vec` alloc on every one of millions of lookups.
```rust
trait PoolStore: Send + Sync {
    fn snapshot(&self, chain: &ChainId) -> Arc<PoolSnapshot>;   // one consistent view per tick
}
struct PoolSnapshot { block: u64, taken_at: OffsetDateTime, /* adjacency + pool table */ }
impl PoolSnapshot {
    fn pools_from(&self, asset: &AssetId) -> &[PoolEntry];   // zero-copy borrow, no lock
    fn get(&self, id: &PoolId) -> Option<&PoolEntry>;        // resolve a Path's PoolId -> Pool
    fn assets(&self) -> impl Iterator<Item = &AssetId>;
}
struct PoolEntry { pool: Arc<dyn Pool>, meta: PoolMeta }     // Pool stays pure; freshness on meta
struct PoolMeta  { synced_block: u64, synced_at: OffsetDateTime }
```
(Trait + snapshot types live in `core/deps/pool_store.rs`.)

### `Valuation` — the oracle (USD price per asset)
Thin port over the reused pricing oracle: it returns the **USD price of one whole token**. The
application computes value **directly where needed** — `value = amount / 10^decimals × price` — using
the `AssetRegistry` (config value) for decimals.
```rust
#[async_trait]
trait Valuation: Send + Sync {
    async fn price(&self, asset: &AssetId) -> Result<Usd, ValuationError>;  // USD per 1 whole token; cached
}
```
- Backed by an adapter wrapping the reused `quote`-service oracle (`HybridPriceFetcher`/`FiatFetcher`,
  TTL-cached); only the few distinct assets in a scan are fetched.
- **Non-fatal**: a missing price never kills a scan — cross-asset paths that can't be valued are
  skipped; cycles still rank by start-asset profit. Cycles need a price only for USD ranking, never
  for the profit decision (which is oracle-free).

### `Notifier` — output
Write-only outbound port; the scanner fans out to several via a composite (log + in-memory now,
DB/webhook later). Each `notify` carries this tick's **full opportunity set for a chain** (empty
clears it): the in-memory notifier **replaces** the chain's set, a log notifier appends. Per-notifier
failures are **isolated** — a failing notifier never stops a scan. The API reads the in-memory
notifier directly (adapter-to-adapter; core never reads opportunities back).
```rust
#[async_trait]
trait Notifier: Send + Sync {
    async fn notify(&self, chain: &ChainId, opps: &[Opportunity]) -> Result<(), NotifyError>;
}
```

### `ChainReader` — batched on-chain read capability
Block-pinned, chunked batch reads. The outer `Result` is **transport failure only**; a per-call
revert is reported in `CallResult.success`, so one dead pool never fails a chain's refresh. `Call`
is byte-level (no chain SDK leaks into core; the EVM adapter parses `target` and ABI-encodes/decodes).
```rust
#[async_trait]
trait ChainReader: Send + Sync {
    /// The chain's head block — resolved once per tick to pin the whole refresh.
    async fn latest_block(&self, chain: &ChainId) -> Result<u64, ChainReadError>;
    /// Block-pinned, chunked batch reads. Per-call failures are in `CallResult`, not `Err`.
    async fn call_batch(&self, chain: &ChainId, at: BlockId, calls: Vec<Call>)
        -> Result<BatchOutput, ChainReadError>;
}
// primitives/chain.rs
struct Call        { target: String, calldata: Bytes }   // hex address + raw calldata
struct CallResult  { success: bool, data: Bytes }         // Multicall3 tryAggregate per-call result
struct BatchOutput { block: u64, results: Vec<CallResult> }
enum   BlockId     { Latest, Number(u64) }
```
Implemented over a vendored provider + Multicall3: resolves `Latest` to one block, runs every chunk
`at` that block (so a refresh is internally block-consistent), chunks internally to respect
gas/response limits, and returns the block used (→ `PoolSnapshot.block`).

## 5. Application (`core/application/`)

### Graph & traversal
- **Nodes = assets; edges = pools** (multigraph). Built from the tick's `PoolSnapshot` (one chain in v1).
- **Traversal rule**: bounded depth (`max_hops`); **each `PoolId` used at most once per path**;
  **assets may repeat, including returning to start**; value checked at every reached asset.
- **High `max_hops` is combinatorial** (branching per pool at each step). `max_hops` defaults to
  **16** for experimentation, kept tractable by: (a) the unique-pool guard, (b) **branch-and-bound**
  pruning on a partial path's running value, and (c) a configurable **`beam_width`** /
  **`max_paths_per_scan`** budget. Raising `max_hops` trades completeness/latency for reach. If a
  scan hits the budget, it logs how many paths were dropped (no silent truncation).
- Path-finding is a **concrete module** for v1; promote to a `PathFinder` trait only when a second
  strategy (e.g. Bellman-Ford) appears (YAGNI).

### Detection (per chain, per tick — all local)
1. **Take one `PoolSnapshot`** at tick start (consistent view) and **build the graph** from it.
2. **Enumerate paths** from each configured **start asset** under the traversal rule.
3. **Quote** each path by chaining `Pool::quote` (resolving `PoolId → Pool` via `snapshot.get`).
4. **Profit**: cycle → `output > input` (no oracle); cross-asset → `value(dest,out) > value(src,in)`.
5. **Size-optimise** (`sizing.rs`): golden-section over input, bounded `[min_input, max_input]`.
6. **Rank + dedup** (`rank.rs`): USD via `Valuation`; canonicalise cycles; drop below thresholds.
7. **Freshness guard** (`validation.rs`): drop opportunities whose worst pool is too stale.
8. **Notify** — fan this tick's set (per chain) out to the `Notifier`(s).

### Scanner (per-chain runner)
Generic over the singleton ports (like evm-executor's `EvmExecutor<C, E>`):
```rust
struct Scanner<S: PoolStore, V: Valuation, N: Notifier> {
    chain: ChainId, pool_store: Arc<S>, valuation: Arc<V>, notifier: Arc<N>, cfg: EngineConfig,
}
// run(): loop { build -> find -> quote -> profit -> size -> rank -> guard -> emit; sleep(scan_interval) }
```

## 6. Adapters (`adapters/`)

- **`exchanges/`** — one module per DEX, each an `Exchange` impl **plus its `Pool` impl(s)** (copied
  munger math; `Pool` impls live here, grouped by exchange). `mod.rs` builds the `Exchanges` registry
  (`HashMap<ExchangeId, Arc<dyn Exchange>>`).
- **`pool_store/`** — `PoolStore` impl (`Arc<RwLock<…>>` with `Vec<Arc<dyn Pool>>` + `pair_index`) +
  the per-chain sync worker. On-disk bootstrap snapshot per chain (path configurable).
- **`valuation/`** — `Valuation` via the vendored fiat client (CoinGecko/CMC).
- **`notifier/`** — `Notifier` impls (log + in-memory latest store) + a composite; the in-memory one
  holds the latest set the `api` adapter reads.
- **`chain_reader/`** — `ChainReader` over vendored provider + Multicall3: resolves one block per
  tick, `tryAggregate` for per-call failure isolation, internal chunking for gas/response limits.
- **`api/`** — axum driving adapter.
- **`vendored/`** — copied garden-rs helpers, provenance headers.

### Sync loop (protocol-agnostic)
```
per chain, on a timer:
  at = reader.latest_block(chain)                    // ONE block pins the whole tick
  for each Exchange in registry supporting this chain:
     keys  = exchange.discover(chain, tracked_tokens, reader)   // cached; re-run rarely
     pools = exchange.refresh(keys, at, reader)                 // block-pinned; batched via ChainReader
  pool_store.publish(snapshot @ block `at`)                     // atomic arc-swap → consistent view
```

## 7. Output (`adapters/api` + `adapters/notifier`)
- `GET /v1/health` → `{ "status": "ok" }`.
- `GET /v1/opportunities?chain=&min_roi_bps=` → current ranked opportunities.
- `GET /v1/status` → per-chain: pool count, last sync block/time, last scan time, #opportunities.
- Structured log per emitted opportunity.

## 8. Adding a new DEX (the extensibility contract)
1. **`Pool` (math)** in `adapters/exchanges` — or **reuse an existing one** if it's a fork
   (PancakeSwap → reuse Uniswap `Pool`; Balancer → new weighted `Pool`).
2. **`Exchange`** in `adapters/exchanges/<dex>.rs` — factory addresses + `discover`/`refresh`.
3. **Register** in the `Exchanges` registry (one line in `setup`).

`primitives`, `core/deps`, `core/application`, and every other adapter are untouched (Open/Closed).

## 9. Reuse & vendoring (self-contained)
- **Copy munger's pure pool math** into `adapters/exchanges` as the `Pool` impls (Uniswap V2/V3/V4,
  Curve, Curve-Crypto, Aerodrome + tick math). Pin with golden-vector tests.
- **Vendor** minimal `garden-rs` helpers into `adapters/vendored` (provider, Multicall3, fiat, retry,
  tracing) — copied, not imported, with provenance headers.
- **Do not** copy munger's sync worker / chain crate — write the lean per-chain sync ourselves.

## 10. Configuration (`Settings.toml`)
```toml
scan_interval_ms = 4000
max_hops = 16                    # experimental; pruned by beam_width / max_paths_per_scan
beam_width = 5000                # top-K partial paths kept per depth (0 = unbounded)
max_paths_per_scan = 2000000     # hard per-scan budget; overflow is logged, not silently dropped
min_roi_bps = 10
fiat_provider_url = "http://localhost:6969"

[[chains]]
chain_identifier = "ethereum"
rpc_url = "https://..."
multicall_address = "0xcA11bde05977b3631167028862bE2a173976CA11"
sync_interval_ms = 4000
start_assets   = ["ethereum:usdc", "ethereum:weth", "ethereum:usdt"]
tracked_tokens = ["ethereum:usdc", "ethereum:weth", "ethereum:usdt", "ethereum:dai", "ethereum:wbtc"]
min_input_usd  = 100
max_input_usd  = 50000
uniswap_v2_factories = ["0x..."]
uniswap_v3_factories = ["0x..."]
curve_pools          = ["0x..."]

# Asset metadata → the app builds the AssetRegistry (decimals/symbol); the valuation
# adapter builds its AssetId -> price-source-id map from `price_id`.
[[assets]]
id = "ethereum:usdc"
address = "0xA0b8..."
decimals = 6
symbol = "USDC"
price_id = "usd-coin"     # CoinGecko / oracle id
```
Fail-fast on bad config.

## 11. Error handling
- Domain: typed `thiserror` per port + per engine step; no panics/`unwrap` outside tests.
- Adapters/bootstrap: `eyre`; per-chain graceful degradation (a failing chain is logged and skipped).
- A scan tick that errors logs and continues; never crashes the runner.

## 12. Testing
- **Golden-vector tests** per copied `Pool` quoter: pinned state → known on-chain output.
- **Application unit tests** on synthetic graphs: enumeration, no-pool-reuse guard, cycle vs
  cross-asset profit, sizing convergence, dedup, freshness guard — all against **in-memory fakes** of
  the deps traits.
- **Adapter tests**: `pool_store` on recorded multicall responses / a forked node; `api` via
  wiremock-style fakes.

## 13. Milestones
1. **M1 — Quoting**: copy pool math as `Pool` impls + golden-vector tests.
2. **M2 — Application**: graph + find + detect + sizing + rank on fakes; full unit tests.
3. **M3 — Sync**: `ChainReader` + one `Exchange` (Uniswap V3) + `PoolStore`, one chain.
4. **M4 — Wiring + API**: settings/setup, `Scanner`, `notifier`, `valuation`, API; end-to-end on Ethereum.
5. **M5 — Breadth**: remaining munger DEXs + all configured EVM chains.

## 14. Decisions
- **Name**: `arb-router`. ✓
- **`max_hops`**: 16 (experimental) with beam/budget pruning. ✓
- **Ids**: single `ExchangeId`. ✓
- **Start assets**: default majors per chain, config-driven (widen to experiment). ✓
- **Ports (finalized):**
  - `Pool` — base-unit, exact-in, pure `quote(pair, amount)`.
  - `Exchange` — `discover` + block-pinned `refresh` → `Pool`s (the extension point).
  - `PoolStore` — `snapshot(chain) -> Arc<PoolSnapshot>` (consistent, lock-free, carries freshness).
  - `Valuation` — `price(asset)`; the app computes value via the `AssetRegistry`.
  - `Notifier` — write-only, per-chain replace, composite fan-out, isolated failures.
  - `ChainReader` — `latest_block` + block-pinned, chunked `call_batch` with per-call failure.
