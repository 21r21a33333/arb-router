//! Multicall3 binding and its canonical address.
//!
//! Multicall3 batches many `eth_call`s into one round trip and, via
//! `tryAggregate(false, …)`, lets each sub-call fail independently — a reverting
//! pool read surfaces as `success = false` rather than failing the whole batch.

use alloy::primitives::{Address, address};
use alloy::sol;

/// Canonical Multicall3 deployment — the same address on every supported chain.
pub const MULTICALL3: Address = address!("0xcA11bde05977b3631167028862bE2a173976CA11");

sol! {
    #[sol(rpc)]
    interface IMulticall3 {
        struct Call {
            address target;
            bytes callData;
        }
        struct Result {
            bool success;
            bytes returnData;
        }
        function tryAggregate(bool requireSuccess, Call[] calldata calls)
            external
            payable
            returns (Result[] memory returnData);
    }
}
