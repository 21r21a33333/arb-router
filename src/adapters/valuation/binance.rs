//! Binance spot `bookTicker` WebSocket feed: best bid/ask → USD mid price.
//!
//! One combined stream carries every tracked symbol. Each update yields a fresh
//! mid `(bid + ask) / 2` (USDT ≈ USD), written straight to the [`PriceStore`].
//! The socket is held open in a reconnect loop with exponential backoff and a
//! receive timeout, so a dropped connection self-heals.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use rust_decimal::Decimal;
use serde::Deserialize;
use tokio_tungstenite::tungstenite::Message;

use super::PriceStore;
use crate::primitives::asset::{AssetId, Usd};

/// Upper bound on reconnect backoff.
const RECONNECT_MAX: Duration = Duration::from_secs(60);
/// Reconnect if no message arrives within this window.
const RECV_TIMEOUT: Duration = Duration::from_secs(30);

pub struct BinanceFeed {
    store: Arc<PriceStore>,
    /// WebSocket base, e.g. `"wss://stream.binance.com:9443"`.
    ws_base: String,
    /// Binance symbol (uppercase, e.g. `"ETHUSDT"`) → asset.
    symbols: HashMap<String, AssetId>,
}

impl BinanceFeed {
    pub fn new(store: Arc<PriceStore>, ws_base: String, symbols: HashMap<String, AssetId>) -> Self {
        Self {
            store,
            ws_base,
            symbols,
        }
    }

    /// Stream forever, reconnecting with exponential backoff. Returns
    /// immediately if no symbols are configured.
    pub async fn run(self) {
        if self.symbols.is_empty() {
            return;
        }
        let mut backoff = Duration::from_secs(1);
        loop {
            match self.stream().await {
                Ok(()) => tracing::warn!("binance stream ended; reconnecting"),
                Err(err) => tracing::warn!(error = %err, "binance feed error; reconnecting"),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(RECONNECT_MAX);
        }
    }

    /// Connect, read `bookTicker` updates, and write mid prices until the socket
    /// closes or stalls.
    async fn stream(&self) -> eyre::Result<()> {
        let streams = self
            .symbols
            .keys()
            .map(|symbol| format!("{}@bookTicker", symbol.to_lowercase()))
            .collect::<Vec<_>>()
            .join("/");
        let url = format!("{}/stream?streams={streams}", self.ws_base);

        let (socket, _) = tokio_tungstenite::connect_async(&url).await?;
        let (_, mut read) = socket.split();

        while let Some(message) = tokio::time::timeout(RECV_TIMEOUT, read.next()).await? {
            if let Message::Text(text) = message? {
                self.ingest(&text);
            }
        }
        Ok(())
    }

    /// Parse one `bookTicker` message and write its mid price, if the symbol is tracked.
    fn ingest(&self, text: &str) {
        if let Ok(envelope) = serde_json::from_str::<Envelope>(text)
            && let Some(asset) = self.symbols.get(&envelope.data.symbol)
            && let Some(mid) = mid_price(&envelope.data)
        {
            self.store.set_binance(asset.clone(), Usd(mid));
        }
    }
}

/// Combined-stream envelope: `{ "stream": …, "data": { … } }`.
#[derive(Deserialize)]
struct Envelope {
    data: BookTicker,
}

#[derive(Deserialize)]
struct BookTicker {
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "b")]
    bid: String,
    #[serde(rename = "a")]
    ask: String,
}

/// `(bid + ask) / 2`, or `None` if either side doesn't parse.
fn mid_price(ticker: &BookTicker) -> Option<Decimal> {
    let bid: Decimal = ticker.bid.parse().ok()?;
    let ask: Decimal = ticker.ask.parse().ok()?;
    Some((bid + ask) / Decimal::from(2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    fn asset(s: &str) -> AssetId {
        AssetId::new(s).unwrap()
    }

    fn feed(store: Arc<PriceStore>) -> BinanceFeed {
        BinanceFeed::new(
            store,
            "wss://unused".to_string(),
            HashMap::from([("ETHUSDT".to_string(), asset("ethereum:weth"))]),
        )
    }

    #[test]
    fn ingest_writes_the_mid_price() {
        let store = Arc::new(PriceStore::new(Duration::from_secs(60)));
        feed(store.clone()).ingest(
            r#"{"stream":"ethusdt@bookTicker","data":{"s":"ETHUSDT","b":"1999.5","a":"2000.5"}}"#,
        );
        assert_eq!(
            store.price(&asset("ethereum:weth"), OffsetDateTime::now_utc()),
            Some(Usd(Decimal::from(2000)))
        );
    }

    #[test]
    fn ingest_ignores_untracked_symbols() {
        let store = Arc::new(PriceStore::new(Duration::from_secs(60)));
        feed(store.clone()).ingest(
            r#"{"stream":"btcusdt@bookTicker","data":{"s":"BTCUSDT","b":"60000","a":"60002"}}"#,
        );
        assert!(
            store
                .price(&asset("ethereum:weth"), OffsetDateTime::now_utc())
                .is_none()
        );
    }

    /// Live check against Binance. Run with:
    /// `cargo test --lib -- --ignored live_binance`
    #[tokio::test]
    #[ignore = "hits live Binance WebSocket"]
    async fn live_binance_populates_a_price() {
        let store = Arc::new(PriceStore::new(Duration::from_secs(60)));
        let feed = BinanceFeed::new(
            store.clone(),
            "wss://stream.binance.com:9443".to_string(),
            HashMap::from([("ETHUSDT".to_string(), asset("ethereum:weth"))]),
        );
        tokio::spawn(feed.run());
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(
            store
                .price(&asset("ethereum:weth"), OffsetDateTime::now_utc())
                .is_some()
        );
    }
}
