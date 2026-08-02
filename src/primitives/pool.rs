use crate::primitives::asset::{AssetId, ChainId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PoolId(String);

impl PoolId {
    pub fn new(s: &str) -> Self {
        Self(s.to_string())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExchangeId(String);

impl ExchangeId {
    pub fn new(s: &str) -> Self {
        Self(s.to_string())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolKey {
    pub exchange: ExchangeId,
    pub chain: ChainId,
    pub address: String,
    pub assets: Vec<AssetId>,
    pub fee_bps: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_id_roundtrips() {
        let p = PoolId::new("ethereum:univ3:0xabc");
        assert_eq!(p.as_str(), "ethereum:univ3:0xabc");
    }
}
