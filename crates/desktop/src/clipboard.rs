//! Text on the clipboard, both ways (§8). The session's clipboard is
//! offered by MIME type and fetched when it changes, if it holds text; the
//! desktop's is read when the window gains focus and offered to the
//! session, which fetches it only when an app there pastes. Only text for
//! now: smithay-clipboard can't serve other types lazily. Elsewhere than
//! Linux, arboard is the clipboard.

use std::sync::Arc;

use farsight_client::Client;
use farsight_proto::control::ClipboardOffer;
use winit::event_loop::ActiveEventLoop;

/// Text types, best first.
const TEXT: &[&str] = &["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "TEXT", "STRING"];

pub struct Clipboard {
    inner: Inner,
    /// The text last moved either way, so it isn't sent back.
    last: Option<String>,
}

impl Clipboard {
    /// The clipboard of the event loop's Wayland connection; `None`
    /// elsewhere. Made before the window: it learns of keyboard focus only
    /// from the events it sees, and can't set the selection without it.
    pub fn new(event_loop: &ActiveEventLoop) -> Option<Self> {
        Some(Self { inner: Inner::new(event_loop)?, last: None })
    }

    /// The text type to fetch from an offer, if it has one.
    pub fn text_mime(offer: &ClipboardOffer) -> Option<&str> {
        TEXT.iter().copied().find(|t| offer.mimes.iter().any(|m| m == t))
    }

    /// Text from the session.
    pub fn set(&mut self, text: String) {
        self.inner.store(text.clone());
        self.last = Some(text);
    }

    /// Offers the desktop's text to the session, if it changed.
    pub fn offer_local(&mut self, client: &Client) {
        let Some(text) = self.inner.load() else { return };
        if self.last.as_ref() == Some(&text) || text.is_empty() {
            return;
        }
        tracing::debug!(bytes = text.len(), "offering the clipboard");
        self.last = Some(text.clone());
        let text = Arc::new(text);
        client.offer_clipboard(
            TEXT.iter().map(|t| t.to_string()).collect(),
            Arc::new(move |mime| TEXT.contains(&mime).then(|| text.as_bytes().to_vec())),
        );
    }
}

#[cfg(target_os = "linux")]
struct Inner(smithay_clipboard::Clipboard);

#[cfg(target_os = "linux")]
impl Inner {
    fn new(event_loop: &ActiveEventLoop) -> Option<Self> {
        use raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
        let RawDisplayHandle::Wayland(h) = event_loop.display_handle().ok()?.as_raw() else { return None };
        // SAFETY: the display outlives the clipboard, which is dropped when
        // the event loop exits.
        Some(Self(unsafe { smithay_clipboard::Clipboard::new(h.display.as_ptr()) }))
    }

    fn store(&mut self, text: String) {
        self.0.store(text);
    }

    fn load(&mut self) -> Option<String> {
        self.0.load().ok()
    }
}

#[cfg(not(target_os = "linux"))]
struct Inner(arboard::Clipboard);

#[cfg(not(target_os = "linux"))]
impl Inner {
    fn new(_event_loop: &ActiveEventLoop) -> Option<Self> {
        arboard::Clipboard::new().map_err(|err| tracing::warn!(%err, "no clipboard")).ok().map(Self)
    }

    fn store(&mut self, text: String) {
        if let Err(err) = self.0.set_text(text) {
            tracing::warn!(%err, "setting the clipboard");
        }
    }

    fn load(&mut self) -> Option<String> {
        self.0.get_text().ok()
    }
}
