//! The farsight server (Linux only).
//!
//! One process is one session: it creates an isolated environment (its own
//! runtime directory, D-Bus session bus and Wayland display), runs a headless
//! compositor in it, and serves clients on one port. Clients that disconnect
//! can reconnect to the same session. See `docs/design.md` §6.

use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about = "farsight server: a headless Wayland session served over QUIC")]
struct Args {
    /// UDP port to listen on.
    #[arg(short, long, default_value_t = farsight_proto::DEFAULT_PORT)]
    port: u16,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    tracing::info!(port = args.port, "farsight-server starting");
    anyhow::bail!("not implemented yet")
}
