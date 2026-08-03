//! Binary entry point: load `Settings.toml`, initialize tracing, and run.
//!
//! Modes:
//! - default (`arb-router`): one-shot — sync + scan every chain once, print the
//!   detected arbitrage opportunities, and exit.
//! - `arb-router serve`: long-running — keep syncing/scanning and serve the read API.

use arb_router::settings::Settings;
use arb_router::setup;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let settings = Settings::load("Settings")?;
    match std::env::args().nth(1).as_deref() {
        Some("serve") => setup::run(settings).await,
        _ => setup::run_once(settings).await,
    }
}
