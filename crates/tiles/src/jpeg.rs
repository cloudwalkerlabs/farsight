//! The few TurboJPEG 3 calls tiles need. The system's library, but on
//! Android, where turbojpeg-sys builds the one it bundles.

use std::ffi::{CStr, c_char, c_int, c_uchar, c_void};

use anyhow::bail;

type Handle = *mut c_void;

#[cfg(target_os = "android")]
use turbojpeg_sys as _;

#[cfg_attr(not(target_os = "android"), link(name = "turbojpeg"))]
unsafe extern "C" {
    fn tj3Init(init_type: c_int) -> Handle;
    fn tj3Destroy(handle: Handle);
    fn tj3GetErrorStr(handle: Handle) -> *mut c_char;
    fn tj3Set(handle: Handle, param: c_int, value: c_int) -> c_int;
    fn tj3Get(handle: Handle, param: c_int) -> c_int;
    fn tj3Free(buffer: *mut c_void);
    fn tj3Compress8(
        handle: Handle,
        src: *const c_uchar,
        width: c_int,
        pitch: c_int,
        height: c_int,
        pixel_format: c_int,
        jpeg: *mut *mut c_uchar,
        jpeg_size: *mut usize,
    ) -> c_int;
    fn tj3DecompressHeader(handle: Handle, jpeg: *const c_uchar, size: usize) -> c_int;
    fn tj3Decompress8(handle: Handle, jpeg: *const c_uchar, size: usize, dst: *mut c_uchar, pitch: c_int, pixel_format: c_int) -> c_int;
}

const INIT_COMPRESS: c_int = 0;
const INIT_DECOMPRESS: c_int = 1;
const PARAM_QUALITY: c_int = 3;
const PARAM_SUBSAMP: c_int = 4;
const PARAM_JPEGWIDTH: c_int = 5;
const PARAM_JPEGHEIGHT: c_int = 6;
const SAMP_444: c_int = 0;
const SAMP_420: c_int = 2;
/// R, G, B, X in memory: what the GPU reads back as ABGR8888.
const PF_RGBX: c_int = 2;

struct Tj(Handle);

// SAFETY: a handle is used by one thread at a time (each owner is Send, not
// Sync).
unsafe impl Send for Tj {}

impl Tj {
    fn new(kind: c_int) -> anyhow::Result<Self> {
        // SAFETY: plain constructor.
        let h = unsafe { tj3Init(kind) };
        if h.is_null() {
            bail!("tj3Init failed");
        }
        Ok(Self(h))
    }

    fn check(&self, ret: c_int, what: &str) -> anyhow::Result<()> {
        if ret != 0 {
            // SAFETY: the handle is valid; the string is NUL-terminated.
            let msg = unsafe { CStr::from_ptr(tj3GetErrorStr(self.0)) }.to_string_lossy();
            bail!("{what}: {msg}");
        }
        Ok(())
    }
}

impl Drop for Tj {
    fn drop(&mut self) {
        // SAFETY: we own the handle.
        unsafe { tj3Destroy(self.0) };
    }
}

pub struct Compressor(Tj);

impl Compressor {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self(Tj::new(INIT_COMPRESS)?))
    }

    /// Compresses `w`×`h` RGBX pixels with rows `stride` bytes apart,
    /// appending the JPEG to `out`.
    pub fn compress(
        &mut self,
        pixels: &[u8],
        stride: usize,
        (w, h): (usize, usize),
        quality: u8,
        chroma444: bool,
        out: &mut Vec<u8>,
    ) -> anyhow::Result<()> {
        assert!(h == 0 || pixels.len() >= (h - 1) * stride + 4 * w, "pixels too short");
        let t = &self.0;
        let mut buf: *mut c_uchar = std::ptr::null_mut();
        let mut size = 0usize;
        // SAFETY: the buffer covers every row, as asserted; TurboJPEG
        // allocates the output, which we free.
        unsafe {
            t.check(tj3Set(t.0, PARAM_QUALITY, quality as c_int), "quality")?;
            t.check(tj3Set(t.0, PARAM_SUBSAMP, if chroma444 { SAMP_444 } else { SAMP_420 }), "subsampling")?;
            let ret = tj3Compress8(t.0, pixels.as_ptr(), w as c_int, stride as c_int, h as c_int, PF_RGBX, &mut buf, &mut size);
            if ret == 0 {
                out.extend_from_slice(std::slice::from_raw_parts(buf, size));
            }
            tj3Free(buf as *mut c_void);
            t.check(ret, "compressing a JPEG")
        }
    }
}

pub struct Decompressor(Tj);

impl Decompressor {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self(Tj::new(INIT_DECOMPRESS)?))
    }

    /// Decompresses a JPEG of exactly `w`×`h` into RGBX, rows `4 * w` bytes.
    pub fn decompress(&mut self, jpeg: &[u8], (w, h): (usize, usize), out: &mut Vec<u8>) -> anyhow::Result<()> {
        let t = &self.0;
        // SAFETY: the output is sized from the header, checked to match.
        unsafe {
            t.check(tj3DecompressHeader(t.0, jpeg.as_ptr(), jpeg.len()), "reading a JPEG header")?;
            let (jw, jh) = (tj3Get(t.0, PARAM_JPEGWIDTH), tj3Get(t.0, PARAM_JPEGHEIGHT));
            if (jw as usize, jh as usize) != (w, h) {
                bail!("JPEG is {jw}x{jh}, expected {w}x{h}");
            }
            out.clear();
            out.resize(4 * w * h, 0);
            t.check(
                tj3Decompress8(t.0, jpeg.as_ptr(), jpeg.len(), out.as_mut_ptr(), (4 * w) as c_int, PF_RGBX),
                "decompressing a JPEG",
            )
        }
    }
}
