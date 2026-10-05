//! The gatekeeper daemon.

use clap::Parser;

#[derive(Parser)]
#[command(version, about = "farsight gatekeeper: authenticates clients and starts sessions")]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "[::]:7740")]
    listen: std::net::SocketAddr,
}

fn main() -> anyhow::Result<()> {
    farsight_server::init_logging();
    let args = Args::parse();
    tracing::info!(listen = %args.listen, "farsightd starting");
    anyhow::bail!("not implemented yet")
}
