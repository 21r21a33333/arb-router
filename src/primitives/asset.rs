use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::ops::{Add, Sub};

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum AssetIdError {
    #[error("asset id must be `chain:token`, got `{0}`")]
    BadFormat(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AssetId(String);
impl AssetId {
    pub fn new(s: &str) -> Result<Self, AssetIdError> {
        let mut parts = s.splitn(2, ':');
        match (parts.next(), parts.next()) {
            (Some(c), Some(t)) if !c.is_empty() && !t.is_empty() => Ok(Self(s.to_string())),
            _ => Err(AssetIdError::BadFormat(s.to_string())),
        }
    }
    pub fn chain(&self) -> &str {
        self.0.split(':').next().unwrap_or("")
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChainId(String);
impl ChainId {
    pub fn new(s: &str) -> Self {
        Self(s.to_string())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Amount(pub Decimal);
impl Amount {
    pub fn zero() -> Self {
        Self(Decimal::ZERO)
    }
}
impl Add for Amount {
    type Output = Amount;
    fn add(self, o: Amount) -> Amount {
        Amount(self.0 + o.0)
    }
}
impl Sub for Amount {
    type Output = Amount;
    fn sub(self, o: Amount) -> Amount {
        Amount(self.0 - o.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Usd(pub Decimal);
impl Add for Usd {
    type Output = Usd;
    fn add(self, o: Usd) -> Usd {
        Usd(self.0 + o.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pair {
    pub source: AssetId,
    pub destination: AssetId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetMeta {
    pub decimals: u8,
    pub symbol: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn asset_id_parses_chain_and_token() {
        let a = AssetId::new("ethereum:usdc").unwrap();
        assert_eq!(a.chain(), "ethereum");
        assert_eq!(a.as_str(), "ethereum:usdc");
    }
    #[test]
    fn asset_id_rejects_missing_colon() {
        assert!(AssetId::new("ethereum").is_err());
    }
    #[test]
    fn amount_orders_and_adds() {
        use rust_decimal::Decimal;
        let a = Amount(Decimal::from(100));
        let b = Amount(Decimal::from(250));
        assert!(b > a);
        assert_eq!((a + b), Amount(Decimal::from(350)));
    }
}
