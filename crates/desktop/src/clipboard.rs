//! Text on the clipboard, both ways (§8). The session's clipboard is
//! offered by MIME type and fetched when it changes, if it holds text; the
//! desktop's is read when the window gains focus and offered to the
//! session, which fetches it only when an app there pastes. Only text for
//! now: smithay-clipboard can't serve other types lazily.

use std::sync::Arc;

use farsight_client::Client;
use farsight_proto::control::ClipboardOffer;
use raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
use winit::event_loop::ActiveEventLoop;

/// Text types, best first.
const TEXT: &[&str] = &["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "TEXT", "STRING"];

pub struct Clipboard {
    inner: smithay_clipboard::Clipboard,
    /// The text last moved either way, so it isn't sent back.
    last: Option<String>,
}

impl Clipboard {
    /// The clipboard of the event loop's Wayland connection; `None`
    /// elsewhere. Made before the window: it learns of keyboard focus only
    /// from the events it sees, and can't set the selection without it.
    pub fn new(event_loop: &ActiveEventLoop) -> Option<Self> {
        let RawDisplayHandle::Wayland(h) = event_loop.display_handle().ok()?.as_raw() else { return None };
        // SAFETY: the display outlives the clipboard, which is dropped when
        // the event loop exits.
        let inner = unsafe { smithay_clipboard::Clipboard::new(h.display.as_ptr()) };
        Some(Self { inner, last: None })
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
        let Ok(text) = self.inner.load() else { return };
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
