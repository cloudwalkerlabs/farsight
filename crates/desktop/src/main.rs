//! The desktop client.

use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about = "farsight desktop client")]
struct Args {
    /// Server address, `host[:port]`.
    address: String,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    tracing::info!(address = %args.address, core = farsight_client::version(), "connecting");
    anyhow::bail!("not implemented yet")
}
