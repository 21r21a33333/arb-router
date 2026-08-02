//! CoinGecko price poller: bulk-fetches every tracked asset on an interval and
//! writes the results into the shared [`PriceStore`].

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use rust_decimal::Decimal;

use super::PriceStore;
use crate::primitives::asset::{AssetId, Usd};

pub struct CoinGeckoFeed {
    store: Arc<PriceStore>,
    client: reqwest::Client,
    base_url: String,
    /// Asset → CoinGecko coin id (e.g. `ethereum:weth → "weth"`).
    price_ids: HashMap<AssetId, String>,
    interval: Duration,
}

impl CoinGeckoFeed {
    pub fn new(
        store: Arc<PriceStore>,
        base_url: String,
        price_ids: HashMap<AssetId, String>,
        interval: Duration,
    ) -> Self {
        Self {
            store,
            client: reqwest::Client::new(),
            base_url,
            price_ids,
            interval,
        }
    }

    /// Refresh forever, logging failures and continuing.
    pub async fn run(self) {
        loop {
            if let Err(err) = self.refresh().await {
                tracing::warn!(error = %err, "coingecko refresh failed");
            }
            tokio::time::sleep(self.interval).await;
        }
    }

    /// Fetch every tracked coin id in one request and store the prices.
    async fn refresh(&self) -> Result<(), reqwest::Error> {
        if self.price_ids.is_empty() {
            return Ok(());
        }
        let ids: BTreeSet<&str> = self.price_ids.values().map(String::as_str).collect();
        let url = format!(
            "{}/simple/price?ids={}&vs_currencies=usd",
            self.base_url,
            ids.into_iter().collect::<Vec<_>>().join(",")
        );

        let body: HashMap<String, HashMap<String, f64>> =
            self.client.get(url).send().await?.json().await?;

        for (asset, coin_id) in &self.price_ids {
            if let Some(usd) = body.get(coin_id).and_then(|quote| quote.get("usd"))
                && let Ok(price) = Decimal::try_from(*usd)
            {
                self.store.set_coingecko(asset.clone(), Usd(price));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn asset(s: &str) -> AssetId {
        AssetId::new(s).unwrap()
    }

    #[tokio::test]
    async fn refresh_bulk_populates_the_store() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/simple/price"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "usd-coin": { "usd": 1.0 },
                "weth": { "usd": 2000.0 }
            })))
            // One request covers both assets.
            .expect(1)
            .mount(&server)
            .await;

        let store = Arc::new(PriceStore::new(Duration::from_secs(60)));
        let price_ids = HashMap::from([
            (asset("ethereum:usdc"), "usd-coin".to_string()),
            (asset("ethereum:weth"), "weth".to_string()),
        ]);
        let feed = CoinGeckoFeed::new(
            store.clone(),
            server.uri(),
            price_ids,
            Duration::from_secs(60),
        );

        feed.refresh().await.unwrap();

        let now = OffsetDateTime::now_utc();
        assert_eq!(
            store.price(&asset("ethereum:usdc"), now),
            Some(Usd(Decimal::from(1)))
        );
        assert_eq!(
            store.price(&asset("ethereum:weth"), now),
            Some(Usd(Decimal::from(2000)))
        );
    }
}
