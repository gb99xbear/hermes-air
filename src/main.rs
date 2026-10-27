// Hermes Gateway - Main Binary
use hermes_gateway_core::{Gateway, Config};
use hermes_config::ConfigLoader;
use tracing_subscriber::{EnvFilter, fmt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .json()
        .init();

    let config = ConfigLoader::load("config.toml")?;
    let gateway = Gateway::new(config).await?;
    gateway.run().await
}
