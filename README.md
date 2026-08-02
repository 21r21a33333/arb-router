# arb-router

An on-chain **arbitrage-detection router** for EVM DEXs. It maintains local,
block-consistent pool state, treats the market as a graph of assets and pools,
and finds source→destination paths where **`value(destination) > value(source)`**
— cycles that round-trip to more of the same asset, and cross-asset paths worth
more in USD than they cost.

**v1 is detect-only** (no execution): it surfaces opportunities via an API and
logs; wiring up execution is future work.

---

## How it works

```
             per block                          per tick
  RPC ──► SyncWorker ──► PoolStore ──► Scanner ──────────────► Notifier ──► API
          discover +     (lock-free    graph → find paths →              /v1/opportunities
          refresh pools   snapshot)    quote → price → rank
```

1. **Sync** — a `SyncWorker` per chain pins the latest block, discovers each
   exchange's pools and reads their state at that block, and publishes an
   immutable `PoolSnapshot` (atomic, lock-free via `arc-swap`).
2. **Detect** — a `Scanner` per chain reads the snapshot, builds an asset graph
   (assets = nodes, pools = edges), enumerates bounded paths (no-pool-reuse
   guard), quotes each hop against the local pool math, and keeps the
   value-increasing ones.
3. **Price & rank** — USD values come from a live feed (Binance WebSocket +
   CoinGecko); opportunities are deduplicated and ranked.
4. **Serve** — results are exposed over a small read API.

### Quoting

Pools are quoted from **local state** (no per-quote RPC), so exploring many
paths is cheap:

- **Uniswap V2** — constant-product.
- **Uniswap V3 / V4** — Q64.96 tick-crossing math (`uniswap_v3_math`).
- **Curve** — every StableSwap / CryptoSwap variant (`curve-math`).

### Architecture

Modified hexagonal (ports & adapters):

- `primitives/` — value types (`AssetId`, `Amount`, `Usd`, `Pool`/`Path`/`Opportunity`).
- `core/deps/` — the ports (traits): `Pool`, `Exchange`, `PoolStore`, `Valuation`,
  `Notifier`, `ChainReader`.
- `core/application/` — the pure engine (graph, path finder, detection, ranking,
  scanner), tested entirely against in-memory fakes.
- `adapters/` — implementations: alloy RPC + Multicall3, the Uniswap V3
  exchange, the arc-swap store + sync worker, the price feeds, notifiers, and
  the axum API.

The engine never imports an adapter or a chain SDK. A chain, a CEX market, or a
bridge is all just a `Pool` edge between namespaced assets (`chain:token`), so
the design generalizes to cross-chain/CEX without engine changes.

---

## Running

```bash
cp Settings.example.toml Settings.toml   # then set a real rpc_url
cargo run
```

Then:

```bash
curl localhost:8080/v1/health
curl localhost:8080/v1/opportunities        # ?chain=ethereum&min_roi_bps=50
curl localhost:8080/v1/status               # per-chain pool count / synced block
```

Configuration (chains, assets, price feeds, `max_hops`, input size) lives in
`Settings.toml` — see `Settings.example.toml`.

---

## Testing

```bash
cargo test --lib                              # ~100 tests, fully offline
cargo test --lib -- --ignored                # live tests (need ETH_RPC_URL / Binance)
```

Core logic is tested against fakes with zero network. Live integrations
(RPC reads, Binance WS) are `#[ignore]`d and opt-in.

---

## Status

| Milestone | |
|---|---|
| M1 — primitives + pool quoters | ✅ |
| M2 — detection engine | ✅ |
| M3 — on-chain state sync (Uniswap V3) | ✅ |
| M4 — wiring + read API + live pricing | ✅ |
| M5 — breadth: sync adapters for Uniswap V2 + Curve StableSwap, multi-chain | ✅ |
| Uniswap V4 + Aerodrome sync adapters | deferred |
| Execution | future |

Sync adapters live today for **Uniswap V2, Uniswap V3, and Curve StableSwap**
(quote math for V4 and Curve-Crypto also exists — they just need `Exchange`
adapters). Multiple chains each run their own sync + scan loop.

## License

Not yet licensed. Note: `curve-math` is BSL-1.1 — production/revenue use requires
the author's commercial license.
