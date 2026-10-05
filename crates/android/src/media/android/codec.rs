//! MediaCodec through the NDK, decoding straight to a `Surface`, in
//! asynchronous mode: its callbacks only hand indices to the video thread,
//! which makes every call itself.
//!
//! Low latency (`KEY_LOW_LATENCY`, API 30) is asked for, along with the
//! vendors' own keys for it, which codecs that don't know them ignore.
//! When each frame reaches the display comes from
//! `AMediaCodec_setOnFrameRenderedCallback`, which is API 33 and so is
//! looked up at run time.

use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr::NonNull;

use anyhow::{Context, bail};
use ndk::native_window::NativeWindow;
use ndk_sys as ffi;

/// What the codec reports, from its own threads.
#[derive(Debug)]
pub enum CodecEvent {
    /// An input buffer is free.
    Input(usize),
    /// A frame is decoded and waits in this output buffer.
    Output { index: usize, pts_us: i64 },
    /// A frame reached the display, at this `CLOCK_MONOTONIC` time.
    Rendered { pts_us: i64, system_ns: i64 },
    Error { fatal: bool, detail: String },
}

type Sink = Box<dyn Fn(CodecEvent) + Send + Sync>;

/// `BUFFER_FLAG_KEY_FRAME`.
pub const FLAG_KEY_FRAME: u32 = 1;
/// `BUFFER_FLAG_CODEC_CONFIG`: parameter sets, ahead of a keyframe.
pub const FLAG_CODEC_CONFIG: u32 = 2;

pub struct Codec {
    ptr: NonNull<ffi::AMediaCodec>,
    /// Owned here, and freed after the codec, which calls into it.
    sink: *mut Sink,
}

// SAFETY: AMediaCodec may be called from any thread; the video thread owns
// this.
unsafe impl Send for Codec {}

/// Format keys, as MediaFormat names them.
fn set_i32(format: *mut ffi::AMediaFormat, key: &str, value: i32) {
    let key = CString::new(key).unwrap();
    // SAFETY: a valid format and NUL-terminated key.
    unsafe { ffi::AMediaFormat_setInt32(format, key.as_ptr(), value) };
}

impl Codec {
    /// Creates the decoder `name` for `mime` at this size, drawing into
    /// `window`, and starts it.
    pub fn new(
        name: &str,
        mime: &str,
        (width, height): (u32, u32),
        window: &NativeWindow,
        sink: impl Fn(CodecEvent) + Send + Sync + 'static,
    ) -> anyhow::Result<Self> {
        let cname = CString::new(name)?;
        // SAFETY: a NUL-terminated name.
        let ptr = NonNull::new(unsafe { ffi::AMediaCodec_createCodecByName(cname.as_ptr()) })
            .with_context(|| format!("creating {name}"))?;
        let sink: *mut Sink = Box::into_raw(Box::new(Box::new(sink)));
        let codec = Codec { ptr, sink };
        // SAFETY: a fresh format, deleted below.
        let format = unsafe { ffi::AMediaFormat_new() };
        let cmime = CString::new(mime)?;
        // SAFETY: a valid format and NUL-terminated strings.
        unsafe { ffi::AMediaFormat_setString(format, c"mime".as_ptr(), cmime.as_ptr()) };
        set_i32(format, "width", width as i32);
        set_i32(format, "height", height as i32);
        set_i32(format, "low-latency", 1);
        // Realtime, and as fast as it goes.
        set_i32(format, "priority", 0);
        set_i32(format, "operating-rate", i16::MAX as i32);
        // What vendors had before KEY_LOW_LATENCY.
        set_i32(format, "vendor.qti-ext-dec-low-latency.enable", 1);
        set_i32(format, "vendor.qti-ext-dec-picture-order.enable", 1);
        set_i32(format, "vendor.rtc-ext-dec-low-latency.enable", 1);
        set_i32(format, "vendor.hisi-ext-low-latency-video-dec.video-scene-for-low-latency-req", 1);
        set_i32(format, "vendor.hisi-ext-low-latency-video-dec.video-scene-for-low-latency-rdy", -1);
        set_i32(format, "vendor.mtk.ext.dec.low-latency.enable", 1);
        // SAFETY: valid codec, format and window; the window outlives the
        // call, and the codec keeps its own reference.
        let status = unsafe {
            ffi::AMediaCodec_configure(ptr.as_ptr(), format, window.ptr().as_ptr(), std::ptr::null_mut(), 0)
        };
        // SAFETY: made above, no longer used.
        unsafe { ffi::AMediaFormat_delete(format) };
        check(status).with_context(|| format!("configuring {name} for {width}x{height}"))?;
        let callbacks = ffi::AMediaCodecOnAsyncNotifyCallback {
            onAsyncInputAvailable: Some(on_input),
            onAsyncOutputAvailable: Some(on_output),
            onAsyncFormatChanged: Some(on_format),
            onAsyncError: Some(on_error),
        };
        // SAFETY: the sink lives until after the codec is deleted.
        check(unsafe { ffi::AMediaCodec_setAsyncNotifyCallback(ptr.as_ptr(), callbacks, codec.sink.cast()) })
            .context("asynchronous mode")?;
        if let Some(set) = frame_rendered_api() {
            // SAFETY: as above.
            let _ = check(unsafe { set(ptr.as_ptr(), on_rendered, codec.sink.cast()) });
        }
        // SAFETY: configured.
        check(unsafe { ffi::AMediaCodec_start(ptr.as_ptr()) }).with_context(|| format!("starting {name}"))?;
        Ok(codec)
    }

    /// Fills input buffer `index` with `data` and queues it.
    pub fn queue(&self, index: usize, data: &[u8], pts_us: u64, flags: u32) -> anyhow::Result<()> {
        let mut size = 0;
        // SAFETY: a running codec; the buffer is ours until queued.
        let buf = unsafe { ffi::AMediaCodec_getInputBuffer(self.ptr.as_ptr(), index, &mut size) };
        if buf.is_null() {
            bail!("no input buffer {index}");
        }
        if data.len() > size {
            bail!("a frame of {} bytes, in an input buffer of {size}", data.len());
        }
        // SAFETY: `size` bytes at `buf` are ours.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), buf, data.len()) };
        // SAFETY: as above.
        check(unsafe { ffi::AMediaCodec_queueInputBuffer(self.ptr.as_ptr(), index, 0, data.len(), pts_us, flags) })
    }

    /// Sends output buffer `index` to the display, at once: with no time
    /// given, the surface would take the frame's timestamp as when to show
    /// it.
    pub fn render(&self, index: usize, now_ns: i64) -> anyhow::Result<()> {
        // SAFETY: an output buffer the codec handed us.
        check(unsafe { ffi::AMediaCodec_releaseOutputBufferAtTime(self.ptr.as_ptr(), index, now_ns) })
    }

    /// Goes on drawing into another surface.
    pub fn set_surface(&self, window: &NativeWindow) -> anyhow::Result<()> {
        // SAFETY: a running codec and a valid window.
        check(unsafe { ffi::AMediaCodec_setOutputSurface(self.ptr.as_ptr(), window.ptr().as_ptr()) })
    }
}

impl Drop for Codec {
    fn drop(&mut self) {
        // SAFETY: ours; after delete, no callback runs, and the sink can go.
        unsafe {
            ffi::AMediaCodec_stop(self.ptr.as_ptr());
            ffi::AMediaCodec_delete(self.ptr.as_ptr());
            drop(Box::from_raw(self.sink));
        }
    }
}

fn check(status: ffi::media_status_t) -> anyhow::Result<()> {
    match status.0 {
        0 => Ok(()),
        n => bail!("media status {n}"),
    }
}

/// # Safety
///
/// `data` is a codec's sink.
unsafe fn sink<'a>(data: *mut c_void) -> &'a Sink {
    // SAFETY: as the caller promises.
    unsafe { &*(data as *const Sink) }
}

unsafe extern "C" fn on_input(_codec: *mut ffi::AMediaCodec, data: *mut c_void, index: i32) {
    // SAFETY: registered with the sink.
    (unsafe { sink(data) })(CodecEvent::Input(index as usize));
}

unsafe extern "C" fn on_output(
    _codec: *mut ffi::AMediaCodec,
    data: *mut c_void,
    index: i32,
    info: *mut ffi::AMediaCodecBufferInfo,
) {
    // SAFETY: registered with the sink; the codec passes a valid info.
    let pts_us = unsafe { (*info).presentationTimeUs };
    // SAFETY: as above.
    (unsafe { sink(data) })(CodecEvent::Output { index: index as usize, pts_us });
}

unsafe extern "C" fn on_format(_codec: *mut ffi::AMediaCodec, _data: *mut c_void, _format: *mut ffi::AMediaFormat) {}

unsafe extern "C" fn on_error(
    _codec: *mut ffi::AMediaCodec,
    data: *mut c_void,
    error: ffi::media_status_t,
    action: i32,
    detail: *const c_char,
) {
    // Recoverable or transient: the codec carries on.
    const RECOVERABLE: i32 = 2;
    const TRANSIENT: i32 = 1;
    let detail = match detail.is_null() {
        true => String::new(),
        // SAFETY: a NUL-terminated string from the codec.
        false => unsafe { CStr::from_ptr(detail) }.to_string_lossy().into_owned(),
    };
    let fatal = action & (RECOVERABLE | TRANSIENT) == 0;
    // SAFETY: registered with the sink.
    (unsafe { sink(data) })(CodecEvent::Error { fatal, detail: format!("{detail} (status {})", error.0) });
}

unsafe extern "C" fn on_rendered(_codec: *mut ffi::AMediaCodec, data: *mut c_void, pts_us: i64, system_ns: i64) {
    // SAFETY: registered with the sink.
    (unsafe { sink(data) })(CodecEvent::Rendered { pts_us, system_ns });
}

type OnRendered = unsafe extern "C" fn(*mut ffi::AMediaCodec, *mut c_void, i64, i64);
type SetOnRendered = unsafe extern "C" fn(*mut ffi::AMediaCodec, OnRendered, *mut c_void) -> ffi::media_status_t;

/// `AMediaCodec_setOnFrameRenderedCallback`, where the device has it.
fn frame_rendered_api() -> Option<SetOnRendered> {
    static API: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    let f = *API.get_or_init(|| {
        // SAFETY: plain dlopen and dlsym; the library stays loaded.
        unsafe {
            let lib = libc::dlopen(c"libmediandk.so".as_ptr(), libc::RTLD_NOW);
            let f = (!lib.is_null()).then(|| libc::dlsym(lib, c"AMediaCodec_setOnFrameRenderedCallback".as_ptr()));
            f.filter(|f| !f.is_null()).map(|f| f as usize)
        }
    });
    // SAFETY: the symbol has this signature (media/NdkMediaCodec.h).
    f.map(|f| unsafe { std::mem::transmute::<usize, SetOnRendered>(f) })
}
