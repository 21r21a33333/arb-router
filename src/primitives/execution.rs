//! The sign-ready output attached to an [`Opportunity`](super::opportunity::Opportunity).
//!
//! An [`ExecutionPlan`] is the transaction(s) a consumer signs and submits to
//! capture the opportunity. It carries no keys and performs no submission — the
//! executor builds it from the detection snapshot via `amm-rs`, and the caller
//! is responsible for re-checking net-of-gas profitability before signing.

use serde::Serialize;

/// One ERC-20 approval a transaction requires before it can pull the input.
///
/// `spender` may be a router or Permit2 depending on the protocol; the consumer
/// grants `min_allowance` of `token` to `spender` before submitting.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ExecutionApproval {
    /// The token to approve (lowercase `chain:0x…` asset id).
    pub token: String,
    /// The contract the approval is granted to.
    pub spender: String,
    /// The minimum allowance required, in the token's base units.
    pub min_allowance: String,
}

/// One sign-ready transaction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ExecutionTx {
    /// Destination contract (a `0x…` address).
    pub to: String,
    /// ABI-encoded calldata as `0x…` hex.
    pub data: String,
    /// Native value to send, in wei (decimal string).
    pub value: String,
    /// The ERC-20 approval this transaction needs, if any.
    pub approval: Option<ExecutionApproval>,
}

/// The transactions that capture an opportunity.
///
/// `atomic` is `true` when the whole route settles in a single transaction (a
/// cycle within one router family). When `false`, the route crosses routers and
/// the transactions must be submitted **in order** — later transactions are
/// built from quoted intermediates and are best-effort until the earlier ones
/// land (their slippage floors guard against drift).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ExecutionPlan {
    /// Whether the route settles in one transaction.
    pub atomic: bool,
    /// The transactions to submit, in order.
    pub transactions: Vec<ExecutionTx>,
}
