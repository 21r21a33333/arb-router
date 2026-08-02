//! Low-level EVM RPC plumbing: provider construction, the Multicall3 binding,
//! and a retry helper. Thin wrappers over `alloy`, kept isolated so the rest of
//! the adapter layer works in provider-agnostic terms.

pub mod multicall;
pub mod provider;
pub mod retry;
