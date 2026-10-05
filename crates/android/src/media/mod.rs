//! Video and audio: MediaCodec, the session view's `Surface` and AAudio on
//! Android; nothing elsewhere, so the workspace still builds and tests on
//! the host.

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
mod annexb;

#[cfg(target_os = "android")]
mod android;
#[cfg(target_os = "android")]
pub use android::Media;

#[cfg(not(target_os = "android"))]
mod host;
#[cfg(not(target_os = "android"))]
pub use host::Media;

/// Video and audio, for the statistics.
#[derive(Debug, Clone, Default)]
pub struct MediaStats {
    pub encoding: String,
    pub fps: f32,
    pub network_ms: f32,
    pub decode_ms: f32,
    pub total_ms: f32,
    pub mic_on: bool,
}
