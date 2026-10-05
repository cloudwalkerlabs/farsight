//! The farsight server (Linux only).
//!
//! - `farsightd`: the privileged gatekeeper. Authenticates clients, opens
//!   a logind session for the user and hands the client to that session.
//! - `farsight-session`: one per session, running as the user. A headless
//!   Wayland compositor that encodes its output and injects input.
//!
//! See `docs/design.md` §6.

/// Starts logging, filtered by `RUST_LOG` (default `info`).
pub fn init_logging() {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
}
