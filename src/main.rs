//! Binary entry point: load `Settings.toml`, initialize tracing, and run.

use arb_router::settings::Settings;
use arb_router::setup;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let settings = Settings::load("Settings")?;
    setup::run(settings).await
}
