//! Port for turning a detected opportunity into sign-ready transactions.
//!
//! An implementation resolves the opportunity's pools from the block-consistent
//! [`PoolSnapshot`] it was detected on and builds calldata for each hop. It does
//! not sign or submit — it hands back an [`ExecutionPlan`] the consumer acts on.

use crate::core::deps::pool_store::PoolSnapshot;
use crate::primitives::execution::ExecutionPlan;
use crate::primitives::opportunity::Opportunity;

#[derive(Debug, thiserror::Error)]
pub enum ExecutorError {
    /// A hop's pool id was not in the snapshot.
    #[error("pool `{0}` not found in snapshot")]
    PoolNotFound(String),
    /// A pool in the route is not backed by an `amm-core` pool, so no calldata
    /// encoder is available for it.
    #[error("pool `{0}` is not executable (not amm-core backed)")]
    NotExecutable(String),
    /// An asset id on the path did not map to an on-chain token.
    #[error("could not map asset `{0}` to a token")]
    AssetMap(String),
    /// The input amount could not be represented on-chain.
    #[error("invalid input amount")]
    Amount,
    /// The `amm-rs` build layer rejected the route.
    #[error("build failed: {0}")]
    Build(String),
}

/// Builds an [`ExecutionPlan`] for a detected opportunity against the snapshot
/// it was found on.
pub trait Executor: Send + Sync {
    /// Build sign-ready transactions for `opp` using `snapshot` for pool state.
    fn build(
        &self,
        opp: &Opportunity,
        snapshot: &PoolSnapshot,
    ) -> Result<ExecutionPlan, ExecutorError>;
}
