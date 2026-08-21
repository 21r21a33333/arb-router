//! Executor adapter — builds sign-ready transactions from an opportunity using
//! `amm-rs`'s `execution::plan()`, against the detection snapshot.
//!
//! For each hop it resolves the pool from the snapshot and recovers the wrapped
//! `amm-core` pool (via [`Pool::as_any`] → [`AmmCorePool::core`]), then plans the
//! route and drives the `next_tx` loop. A single-family cycle plans to one atomic
//! transaction; a cross-router cycle plans to several, with each later span's
//! input threaded from the previous span's `min_received` (best-effort — exact
//! for the atomic case, conservative otherwise).

use std::time::{SystemTime, UNIX_EPOCH};

use alloy::primitives::Address;
use amm_core::primitives::asset::AssetId as CoreAssetId;
use amm_core::primitives::ratio::Bps;
use amm_core::slippage::Slippage;
use amm_core::traits::pool::Pool as CorePool;
use amm_rpc::execution::{
    self, ChainConfig, ExactOutPolicy, ExecutionOptions, NativeEdge, PreparedSwap, Recipient,
    Route, TradeType, resolve,
};

use crate::adapters::exchanges::amm_rpc::AmmCorePool;
use crate::adapters::exchanges::{amount_to_u256, core_asset};
use crate::core::deps::executor::{Executor, ExecutorError};
use crate::core::deps::pool_store::PoolSnapshot;
use crate::primitives::execution::{ExecutionApproval, ExecutionPlan, ExecutionTx};
use crate::primitives::opportunity::Opportunity;

/// An [`Executor`] backed by `amm-rs`. One per chain: it holds that chain's
/// sender address, `amm-rs` [`ChainConfig`], and slippage tolerance.
pub struct AmmRpcExecutor {
    sender: Address,
    chain_cfg: ChainConfig,
    slippage: Slippage,
}

impl AmmRpcExecutor {
    /// Build an executor that sends from `sender` (which also receives the arb
    /// output) with `slippage_bps` tolerance on every span's minimum output.
    pub fn new(sender: Address, chain_cfg: ChainConfig, slippage_bps: u16) -> Self {
        Self {
            sender,
            chain_cfg,
            slippage: Slippage::from_bps(Bps(slippage_bps)),
        }
    }
}

impl Executor for AmmRpcExecutor {
    fn build(
        &self,
        opp: &Opportunity,
        snapshot: &PoolSnapshot,
    ) -> Result<ExecutionPlan, ExecutorError> {
        // Resolve each hop's pool and recover its amm-core pool. The entries are
        // borrowed from the snapshot, so the `&dyn CorePool`s live as long as it.
        let mut core_pools: Vec<&dyn CorePool> = Vec::with_capacity(opp.path.hops.len());
        for hop in &opp.path.hops {
            let entry = snapshot
                .get(&hop.pool)
                .ok_or_else(|| ExecutorError::PoolNotFound(hop.pool.as_str().to_string()))?;
            let core = entry
                .pool
                .as_any()
                .downcast_ref::<AmmCorePool>()
                .map(AmmCorePool::core)
                .ok_or_else(|| ExecutorError::NotExecutable(hop.pool.as_str().to_string()))?;
            core_pools.push(core);
        }

        // The token path is one longer than the pools: [start, dest₁, …, destₙ].
        let mut path: Vec<CoreAssetId> = Vec::with_capacity(opp.path.hops.len() + 1);
        path.push(map_asset(opp.path.start.as_str())?);
        for hop in &opp.path.hops {
            path.push(map_asset(hop.pair.destination.as_str())?);
        }

        let route = Route {
            pools: core_pools,
            path,
            trade_type: TradeType::ExactIn,
        };

        // Recipient = sender: an arb cycle returns its output to the executor.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let opts = resolve(
            ExecutionOptions::new(self.slippage).with_recipient(Recipient::To(self.sender)),
            now,
            self.sender,
        );

        let amount = amount_to_u256(opp.input).ok_or(ExecutorError::Amount)?;

        let mut plan = execution::plan(
            &self.chain_cfg,
            &route,
            amount,
            &opts,
            self.sender,
            NativeEdge::None,
            ExactOutPolicy::Strict,
        )
        .map_err(|e| ExecutorError::Build(e.to_string()))?;

        let atomic = plan.is_atomic();

        // Drive every span. With no on-chain execution to observe, thread the
        // previous span's `min_received` as the next span's input — exact for a
        // single atomic span, conservative (floor-bounded) across routers.
        let mut transactions = Vec::with_capacity(plan.tx_count());
        let mut observed = None;
        while let Some(prepared) = plan
            .next_tx(observed)
            .map_err(|e| ExecutorError::Build(e.to_string()))?
        {
            transactions.push(to_execution_tx(&prepared));
            observed = Some(prepared.min_received);
        }

        Ok(ExecutionPlan {
            atomic,
            transactions,
        })
    }
}

/// Map an arb-router asset id (`chain:0x…`) to an amm-core `AssetId`.
fn map_asset(asset: &str) -> Result<CoreAssetId, ExecutorError> {
    let id = crate::primitives::asset::AssetId::new(asset)
        .map_err(|_| ExecutorError::AssetMap(asset.to_string()))?;
    core_asset(&id).ok_or_else(|| ExecutorError::AssetMap(asset.to_string()))
}

/// Translate an `amm-rs` [`PreparedSwap`] into an arb-router [`ExecutionTx`].
///
/// Addresses use lowercase `0x…` (matching the rest of arb-router's ids); the
/// approval token is rendered as its bare `0x…` address.
fn to_execution_tx(p: &PreparedSwap) -> ExecutionTx {
    ExecutionTx {
        to: format!("{:#x}", p.tx.to),
        data: p.tx.data.to_string(),
        value: p.tx.value.to_string(),
        approval: p.approval.as_ref().map(|a| ExecutionApproval {
            token: format!("{:#x}", Address::from_word(a.token.token)),
            spender: format!("{:#x}", a.spender),
            min_allowance: a.min_allowance.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use alloy::primitives::{U256, address};
    use amm_core::primitives::asset::{AssetId as CoreAssetId, ChainId as CoreChainId};
    use amm_core::primitives::pool::PoolId as CorePoolId;
    use amm_core::protocols::aerodrome::volatile::AerodromeVolatilePool;
    use amm_core::protocols::uniswap::v2::UniswapV2Pool;
    use amm_rpc::execution::chains;
    use amm_rpc::execution::config::Routers;
    use rust_decimal::Decimal;
    use time::OffsetDateTime;

    use crate::core::deps::pool::Pool as ArbPool;
    use crate::core::deps::pool_store::{PoolEntry, PoolMeta, PoolSnapshot};
    use crate::primitives::asset::{Amount, AssetId, ChainId, Pair};
    use crate::primitives::opportunity::{Hop, Opportunity, Path};
    use crate::primitives::pool::PoolId;

    // Two synthetic 18-dp tokens, address-sorted A < B.
    const A: &str = "0x1111111111111111111111111111111111111111";
    const B: &str = "0x2222222222222222222222222222222222222222";

    fn e18() -> U256 {
        U256::from(10u128).pow(U256::from(18u8))
    }
    fn core_asset_of(hex: &str) -> CoreAssetId {
        CoreAssetId::new(CoreChainId(1), hex.parse::<Address>().unwrap().into_word())
    }
    fn arb_asset_of(hex: &str) -> AssetId {
        AssetId::new(&format!("ethereum:{hex}")).unwrap()
    }

    /// Wrap a boxed amm-core pool as a snapshot entry (an `AmmCorePool`).
    fn entry(core: Box<dyn CorePool>) -> PoolEntry {
        PoolEntry {
            pool: Arc::new(AmmCorePool::new("ethereum", core)),
            meta: PoolMeta {
                synced_block: 1,
                synced_at: OffsetDateTime::UNIX_EPOCH,
            },
        }
    }

    fn v2(id: &str, ra: u128, rb: u128) -> Box<dyn CorePool> {
        Box::new(UniswapV2Pool::new(
            CorePoolId::new(id),
            [core_asset_of(A), core_asset_of(B)],
            [U256::from(ra) * e18(), U256::from(rb) * e18()],
            30,
        ))
    }
    fn aero(id: &str, ra: u128, rb: u128) -> Box<dyn CorePool> {
        Box::new(AerodromeVolatilePool::new(
            CorePoolId::new(id),
            [core_asset_of(A), core_asset_of(B)],
            [U256::from(ra) * e18(), U256::from(rb) * e18()],
            30,
        ))
    }

    /// A cycle A →(p1)→ B →(p2)→ A over the two named pools, 1 A in.
    fn cycle_opp(p1: &str, p2: &str) -> Opportunity {
        Opportunity {
            chain: ChainId::new("ethereum"),
            path: Path {
                start: arb_asset_of(A),
                hops: vec![
                    Hop {
                        pool: PoolId::new(p1),
                        pair: Pair {
                            source: arb_asset_of(A),
                            destination: arb_asset_of(B),
                        },
                    },
                    Hop {
                        pool: PoolId::new(p2),
                        pair: Pair {
                            source: arb_asset_of(B),
                            destination: arb_asset_of(A),
                        },
                    },
                ],
            },
            input: Amount(Decimal::from(1_000_000_000_000_000_000u128)), // 1 A (18 dp)
            output: Amount::zero(),
            profit_usd: None,
            roi_bps: 0,
            detected_at: OffsetDateTime::UNIX_EPOCH,
            worst_pool_synced_at: OffsetDateTime::UNIX_EPOCH,
            execution: None,
        }
    }

    fn sender() -> Address {
        address!("0x1111111111111111111111111111111111111111")
    }

    /// A cycle across two Uniswap V2 pools is one atomic Universal Router tx.
    #[test]
    fn same_family_cycle_is_one_atomic_tx() {
        let snapshot = PoolSnapshot::from_entries(
            1,
            OffsetDateTime::UNIX_EPOCH,
            vec![
                entry(v2("p1", 1_000_000, 1_000_000)),
                entry(v2("p2", 900_000, 1_100_000)),
            ],
        );
        let exec = AmmRpcExecutor::new(sender(), chains::ethereum(), 30);
        let plan = exec
            .build(&cycle_opp("p1", "p2"), &snapshot)
            .expect("build");

        assert!(plan.atomic, "all-Uniswap cycle must be atomic");
        assert_eq!(plan.transactions.len(), 1);
        assert!(plan.transactions[0].data.starts_with("0x"));
        assert!(
            plan.transactions[0].approval.is_some(),
            "ERC-20 input needs an approval"
        );
    }

    /// A cycle crossing Uniswap and Aerodrome is two sequential transactions.
    #[test]
    fn cross_router_cycle_is_two_txs() {
        let snapshot = PoolSnapshot::from_entries(
            1,
            OffsetDateTime::UNIX_EPOCH,
            vec![
                entry(v2("p1", 1_000_000, 1_000_000)),
                entry(aero("p2", 900_000, 1_100_000)),
            ],
        );
        // A config with BOTH the Uniswap and Aerodrome routers set.
        let mut routers = Routers::default();
        routers.universal = Some(Address::repeat_byte(0x11));
        routers.v2 = Some(Address::repeat_byte(0x12));
        routers.permit2 = Some(Address::repeat_byte(0x13));
        routers.aerodrome = Some(Address::repeat_byte(0x14));
        routers.aerodrome_factory = Some(Address::repeat_byte(0x15));
        let cfg = amm_rpc::execution::ChainConfig::new(CoreChainId(1), core_asset_of(A))
            .with_routers(routers);
        let exec = AmmRpcExecutor::new(sender(), cfg, 30);
        let plan = exec
            .build(&cycle_opp("p1", "p2"), &snapshot)
            .expect("build");

        assert!(!plan.atomic, "cross-router cycle is not atomic");
        assert_eq!(plan.transactions.len(), 2);
    }

    /// A pool that isn't amm-core-backed cannot be executed.
    #[test]
    fn non_amm_core_pool_is_not_executable() {
        struct Stub;
        impl ArbPool for Stub {
            fn id(&self) -> PoolId {
                PoolId::new("p1")
            }
            fn assets(&self) -> &[AssetId] {
                &[]
            }
            fn quote(&self, _pair: &Pair, _amount_in: Amount) -> Option<Amount> {
                None
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }
        let snapshot = PoolSnapshot::from_entries(
            1,
            OffsetDateTime::UNIX_EPOCH,
            vec![PoolEntry {
                pool: Arc::new(Stub),
                meta: PoolMeta {
                    synced_block: 1,
                    synced_at: OffsetDateTime::UNIX_EPOCH,
                },
            }],
        );
        let exec = AmmRpcExecutor::new(sender(), chains::ethereum(), 30);
        let err = exec.build(&cycle_opp("p1", "p1"), &snapshot).unwrap_err();
        assert!(matches!(err, ExecutorError::NotExecutable(_)));
    }

    /// A hop whose pool is absent from the snapshot is reported, not panicked.
    #[test]
    fn missing_pool_is_reported() {
        let snapshot = PoolSnapshot::from_entries(1, OffsetDateTime::UNIX_EPOCH, vec![]);
        let exec = AmmRpcExecutor::new(sender(), chains::ethereum(), 30);
        let err = exec
            .build(&cycle_opp("nope", "nope"), &snapshot)
            .unwrap_err();
        assert!(matches!(err, ExecutorError::PoolNotFound(_)));
    }
}
