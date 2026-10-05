//! The session view's `Surface`, handed over through JNI rather than uniffi
//! (which has no way to pass one): Kotlin calls `NativeSurface.set` when
//! the surface is created or changes, and `NativeSurface.set(null)` when it
//! is destroyed. The video thread hears of each change, and a destroyed
//! surface is let go of before `set` returns, as Android requires.

use std::sync::mpsc::{self, Sender};
use std::sync::Mutex;
use std::time::Duration;

use jni_sys::{JNIEnv, jclass, jobject};
use ndk::native_window::NativeWindow;

/// The current surface, and the video thread to tell of changes, by its
/// id. Only the newest session's video thread hears.
static SURFACE: Mutex<Option<NativeWindow>> = Mutex::new(None);
static WATCHER: Mutex<Option<(u64, Sender<super::video::Msg>)>> = Mutex::new(None);

/// How long a destroyed surface waits for the video thread to let go.
const RELEASE_WAIT: Duration = Duration::from_millis(500);

/// The surface to draw on, if there is one.
pub fn current() -> Option<NativeWindow> {
    SURFACE.lock().unwrap().clone()
}

/// The video thread that hears of changes from now on.
pub fn watch(id: u64, tx: Sender<super::video::Msg>) {
    *WATCHER.lock().unwrap() = Some((id, tx));
}

/// A video thread is done; unless a newer one took over, nobody hears.
pub fn unwatch(id: u64) {
    let mut watcher = WATCHER.lock().unwrap();
    if watcher.as_ref().is_some_and(|(w, _)| *w == id) {
        *watcher = None;
    }
}

/// `dev.fanchao.farsight.NativeSurface.set(Surface?)`.
///
/// # Safety
///
/// Called by the JVM, with a valid `env` and `surface` a `Surface` or null.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_fanchao_farsight_NativeSurface_set(
    env: *mut JNIEnv,
    _class: jclass,
    surface: jobject,
) {
    let window = match surface.is_null() {
        true => None,
        // SAFETY: as the caller promises.
        false => unsafe { NativeWindow::from_surface(env, surface) },
    };
    tracing::info!(size = ?window.as_ref().map(|w| (w.width(), w.height())), "surface");
    let gone = window.is_none();
    *SURFACE.lock().unwrap() = window;
    let Some((_, tx)) = WATCHER.lock().unwrap().clone() else { return };
    let (ack, done) = mpsc::sync_channel(1);
    if tx.send(super::video::Msg::Surface(ack)).is_ok() && gone {
        let _ = done.recv_timeout(RELEASE_WAIT);
    }
}
