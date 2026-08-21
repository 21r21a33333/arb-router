//! The executor layer: turns a detected opportunity into sign-ready calldata.
//!
//! [`amm_rpc`] implements the [`Executor`](crate::core::deps::executor::Executor)
//! port by reusing `amm-rs`'s `execution::plan()` engine — the same math that
//! produced the opportunity now produces the transactions to capture it.

pub mod amm_rpc;
