//! Platform-independent client core: connection, codec negotiation, input
//! state sync and layout reporting. The desktop and Android apps wrap it.

/// The client core's version.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
