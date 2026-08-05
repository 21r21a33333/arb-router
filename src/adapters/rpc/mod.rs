//! Low-level EVM RPC plumbing: provider construction and a retry helper. Thin
//! wrappers over `alloy`, kept isolated so the rest of the adapter layer works
//! in provider-agnostic terms.

pub mod provider;
pub mod retry;
