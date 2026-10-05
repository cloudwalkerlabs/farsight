//! Android bindings for the farsight client, exported to Kotlin with
//! uniffi.
//!
//! The hot paths stay in Rust (`docs/design.md` §3, §8): video goes from
//! the connection to MediaCodec, which draws straight into the session
//! view's `Surface` (handed over through JNI, not uniffi); tiles are drawn
//! into the same `Surface`; audio runs on AAudio both ways. Kotlin draws
//! the UI and the cursor, turns touches into input, and looks after the
//! keyboard, the clipboard and permissions.

uniffi::setup_scaffolding!();

mod media;
mod session;

use std::path::Path;

pub use session::*;

/// Sets up logging to logcat, at info and above (QUIC and TLS only warn).
/// Safe to call more than once.
#[uniffi::export]
pub fn init_logging() {
    use tracing::Level;
    use tracing_subscriber::filter::Targets;
    use tracing_subscriber::prelude::*;
    let filter = Targets::new()
        .with_default(Level::INFO)
        .with_target("quinn", Level::WARN)
        .with_target("quinn_proto", Level::WARN)
        .with_target("quinn_udp", Level::WARN)
        .with_target("rustls", Level::WARN);
    let registry = tracing_subscriber::registry().with(filter);
    #[cfg(target_os = "android")]
    let registry = registry.with(paranoid_android::layer("farsight").with_ansi(false));
    #[cfg(not(target_os = "android"))]
    let registry = registry.with(tracing_subscriber::fmt::layer());
    let _ = registry.try_init();
}

/// The client core's version.
#[uniffi::export]
pub fn core_version() -> String {
    farsight_client::version().to_owned()
}

#[derive(Debug, uniffi::Error)]
pub enum FarsightError {
    Failed { reason: String },
}

impl std::fmt::Display for FarsightError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FarsightError::Failed { reason } => f.write_str(reason),
        }
    }
}

impl From<anyhow::Error> for FarsightError {
    fn from(err: anyhow::Error) -> Self {
        FarsightError::Failed { reason: format!("{err:#}") }
    }
}

fn key(config_dir: &str) -> anyhow::Result<farsight_net::auth::ClientKey> {
    farsight_net::auth::ClientKey::load_or_generate(&Path::new(config_dir).join("client_key"))
}

/// This device's line for a server's `authorized_keys`; the key is made
/// on first use.
#[uniffi::export]
pub fn client_key_line(config_dir: String, comment: String) -> Result<String, FarsightError> {
    Ok(key(&config_dir)?.authorized_line(&comment))
}

/// The server's name as pinned in `known_hosts`: the address with its
/// port.
#[uniffi::export]
pub fn server_name(address: String) -> String {
    farsight_client::server_name(address.trim())
}

/// Forgets the identity pinned for a server, so the next connection pins
/// what it sees: for a server whose identity was replaced on purpose.
#[uniffi::export]
pub fn forget_server(config_dir: String, address: String) -> Result<(), FarsightError> {
    let known = farsight_net::auth::KnownHosts::new(Path::new(&config_dir).join("known_hosts"));
    Ok(known.forget(&farsight_client::server_name(address.trim()))?)
}
