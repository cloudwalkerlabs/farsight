//! A single session: the headless compositor, encoder and transport.

use clap::Parser;

#[derive(Parser)]
#[command(version, about = "farsight session: headless Wayland compositor for one user session")]
struct Args {}

fn main() -> anyhow::Result<()> {
    farsight_server::init_logging();
    let Args {} = Args::parse();
    tracing::info!("farsight-session starting");
    anyhow::bail!("not implemented yet")
}
