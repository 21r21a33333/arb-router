//! Read-only HTTP provider construction.

use alloy::providers::RootProvider;
use alloy::transports::http::reqwest::Url;

/// The concrete provider type used across the adapter layer: a plain
/// Ethereum-network HTTP root provider, sufficient for reads (block height,
/// `eth_call`). Cheap to clone — reference-counted internally.
pub type EthProvider = RootProvider;

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("invalid rpc url `{0}`")]
    BadUrl(String),
}

/// Build a read-only HTTP provider for `rpc_url`.
pub fn make_provider(rpc_url: &str) -> Result<EthProvider, RpcError> {
    let url: Url = rpc_url
        .parse()
        .map_err(|_| RpcError::BadUrl(rpc_url.to_string()))?;
    Ok(RootProvider::new_http(url))
}
