//! Android bindings for the farsight client, exported to Kotlin with
//! uniffi.

uniffi::setup_scaffolding!();

/// Sets up logging to logcat. Safe to call more than once.
#[uniffi::export]
pub fn init_logging() {
    use tracing_subscriber::prelude::*;
    let registry = tracing_subscriber::registry();
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
