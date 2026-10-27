// Hermes Gateway - Main Binary
use hermes_config::ConfigLoader;
use tracing_subscriber::{EnvFilter, fmt};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "hermes-gateway", version, about = "Hermes Gateway - Rust Rewrite")]
struct Args {
    #[arg(short, long, default_value = "config.toml")]
    config: String,
    
    #[arg(long)]
    validate: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    
    fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .json()
        .init();

    let config = ConfigLoader::load(&args.config)?;
    
    if args.validate {
        println!("Configuration valid!");
        println!("Profiles: {}", config.profiles.len());
        println!("Platforms: {}", config.platforms.len());
        return Ok(());
    }

    let gateway = hermes_gateway_core::Gateway::new(config).await?;
    gateway.run().await
}