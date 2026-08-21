# arb-router execution wiring — Design

**Goal:** Turn a detected `Opportunity` into an `ExecutionPlan` (sign-ready
transactions) using the `amm-rs` `execution::plan()` engine, and serve it on
`/v1/opportunities`. Detection-only becomes detect-and-build. No signing, no
submission — the consumer holds the key.

**Approved decisions (2026-08-20):**
1. Build calldata for **all** opportunities: atomic (one tx) and cross-router
   (N sequential txs, best-effort — downstream spans threaded from quoted
   intermediates, flagged `atomic: false`).
2. Build against the **detection snapshot** (the block-consistent state the
   opportunity was found on), reached via a new capability accessor on the
   `Pool` port.
3. Expose by **enriching `/v1/opportunities`** with an optional `execution`
   field, computed for the ranked set each scan tick.

## Global constraints

- Hexagonal, evm-executor house style: ports in `core/deps/` (one file, own
  typed `thiserror` enum), primitives consolidated, adapters flat (one file),
  `setup.rs` composition root. No `Result<T, String>` in ports.
- Execution is **best-effort and non-fatal**: any failure → `execution: None`;
  detection/ranking/serving are never affected.
- amm-rs stays in the adapter layer only; the `core/deps` `Pool` port gains only
  a std `as_any` (no amm-rs types leak into core).
- Crate stays `cargo build`/`test`/`clippy`/`fmt` green.

## Components

- **`primitives/execution.rs`** (new): `ExecutionPlan { atomic: bool,
  transactions: Vec<ExecutionTx> }`, `ExecutionTx { to: String, data: String
  (0x-hex), value: String, approval: Option<ExecutionApproval> }`,
  `ExecutionApproval { token: String, spender: String, min_allowance: String }`.
  All `Clone + Debug + Serialize`.
- **`primitives/opportunity.rs`**: `Opportunity` gains
  `pub execution: Option<ExecutionPlan>` (default `None`).
- **`core/deps/executor.rs`** (new port): `trait Executor: Send + Sync { fn
  build(&self, opp: &Opportunity, snapshot: &PoolSnapshot) -> Result<
  ExecutionPlan, ExecutorError>; }` + `ExecutorError` (`PoolNotFound`,
  `NotAmmCore`, `AssetMap`, `Build(String)` for the amm-rs `BuildError`,
  `Amount`). Re-exported from `core/deps/mod.rs`.
- **`core/deps/pool.rs`**: add `fn as_any(&self) -> &dyn std::any::Any;` to the
  `Pool` port. Impls return `self`.
- **`adapters/exchanges/amm_rpc.rs`**: `AmmCorePool` implements `as_any` and
  gains `pub(crate) fn core(&self) -> &dyn amm_core::traits::pool::Pool`.
- **`adapters/executor/amm_rpc.rs`** (new): `AmmRpcExecutor { sender: Address,
  chain_cfg: ChainConfig, slippage: Slippage }` implements `Executor`. Reuses
  the `pub(crate)` `core_asset` / `amount_to_u256` bridge helpers.
- **`settings.rs`**: `ChainSettings` gains `executor_address: Option<String>`
  and `execution_slippage_bps: Option<u16>` (default 30).
- **`core/application/scanner.rs`**: `Scanner` gains
  `executor: Option<Box<dyn Executor>>`; after `rank_and_dedup`, enrich each
  ranked opportunity with `executor.build(opp, &snapshot).ok()`.
- **`setup.rs`**: per chain, if `executor_address` is set and an amm-rs `chains`
  preset exists for the chain, construct an `AmmRpcExecutor` and inject it.
- **`adapters/api/mod.rs`**: `OpportunityView` gains `execution:
  Option<ExecutionPlan>` (serialized directly).

## `AmmRpcExecutor::build` algorithm

1. Resolve each `hop.pool` via `snapshot.get(id)` → `Arc<dyn Pool>`; downcast
   `pool.as_any()` to `AmmCorePool` and take `.core()` → `&dyn amm_core::Pool`.
   Any miss → `ExecutorError::{PoolNotFound,NotAmmCore}`.
2. Map the arb path to amm-core `AssetId`s: `core_asset(start)` then each
   `core_asset(hop.pair.destination)`.
3. Build `amm_rpc::execution::Route { pools, path, trade_type: ExactIn }`.
4. `opts = resolve(ExecutionOptions::new(slippage).with_recipient(To(sender)),
   now, sender)` (recipient = sender: arb funds return to the executor).
5. `amount = amount_to_u256(opp.input)`.
6. `plan(&chain_cfg, &route, amount, &opts, sender, NativeEdge::None,
   ExactOutPolicy::Strict)`.
7. Drive `next_tx`: thread `observed` from each `PreparedSwap.min_received`
   (best-effort stand-in for the on-chain output — exact for a single atomic
   span; conservative for multi-tx). Collect into `Vec<ExecutionTx>`.
8. `ExecutionPlan { atomic: plan.is_atomic(), transactions }`.

## Error handling

`build` returns typed `ExecutorError`; the scanner uses `.ok()` so any failure
degrades to `execution: None`. Unconfigured executor (no `executor_address`, or
no `chains` preset for the chain) → the chain's `Scanner.executor` is `None`.

## Testing

- `pool.rs`: `as_any` downcast round-trips to `AmmCorePool` and `.core()` yields
  the wrapped amm-core pool.
- `adapters/executor/amm_rpc.rs`: over synthetic amm-core pools —
  a single-family cycle → `atomic: true`, one tx with the expected `to`/approval;
  a Curve+Uniswap cycle → `atomic: false`, two txs; a pool that isn't
  amm-core-backed → `ExecutorError::NotAmmCore`.
- `api/mod.rs`: `OpportunityView` serializes `execution` (present and `None`).
- `scanner.rs`: a ranked opportunity gets `execution` attached when the executor
  is present, `None` when absent.

## Deliberate boundary

No net-of-gas re-validation and no submission. The `ExecutionPlan` reports each
tx's min output so the consumer checks `min_out > input + gas` before signing.
Gas-aware profitability and submission are the next slice; true cross-router
atomicity is the atomic-contract thread.
