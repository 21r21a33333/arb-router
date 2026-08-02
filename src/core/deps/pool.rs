use crate::primitives::asset::{Amount, AssetId, Pair};
use crate::primitives::pool::PoolId;

pub trait Pool: Send + Sync {
    fn id(&self) -> PoolId;
    fn assets(&self) -> &[AssetId];
    fn quote(&self, pair: &Pair, amount_in: Amount) -> Option<Amount>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;

    struct FakePool {
        id: PoolId,
        assets: Vec<AssetId>,
        rate: Decimal,
    }

    impl Pool for FakePool {
        fn id(&self) -> PoolId {
            self.id.clone()
        }

        fn assets(&self) -> &[AssetId] {
            &self.assets
        }

        fn quote(&self, _pair: &Pair, amount_in: Amount) -> Option<Amount> {
            Some(Amount(amount_in.0 * self.rate))
        }
    }

    #[test]
    fn pool_is_object_safe() {
        let fake = FakePool {
            id: PoolId::new("ethereum:univ3:0xabc"),
            assets: vec![
                AssetId::new("ethereum:usdc").unwrap(),
                AssetId::new("ethereum:weth").unwrap(),
            ],
            rate: Decimal::from(2),
        };
        let _: Box<dyn Pool> = Box::new(fake);
    }

    #[test]
    fn quote_returns_amount_in_times_rate() {
        let fake = FakePool {
            id: PoolId::new("ethereum:univ3:0xabc"),
            assets: vec![
                AssetId::new("ethereum:usdc").unwrap(),
                AssetId::new("ethereum:weth").unwrap(),
            ],
            rate: Decimal::from(3),
        };
        let pair = Pair {
            source: AssetId::new("ethereum:usdc").unwrap(),
            destination: AssetId::new("ethereum:weth").unwrap(),
        };
        let amount_in = Amount(Decimal::from(100));
        let result = fake.quote(&pair, amount_in);
        assert_eq!(result, Some(Amount(Decimal::from(300))));
    }
}
