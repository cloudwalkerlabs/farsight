//! Video decoding through FFmpeg: in hardware where the machine can
//! (VA-API on Linux, VideoToolbox on macOS, D3D11VA on Windows), in
//! software otherwise (§3). Decoded pictures are copied to memory for upload; the
//! zero-copy path (dmabuf → EGLImage) is later work.

use std::ffi::CStr;
use std::ptr;

use anyhow::bail;
use farsight_proto::codec::{Chroma, Codec, DecoderCaps, Format};
use ffmpeg_sys_next as ff;

const AVERROR_EAGAIN: i32 = -libc::EAGAIN;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// Y plane, then interleaved U/V at half size.
    Nv12,
    /// Y, U and V planes, U and V at half size.
    I420,
    /// Y, U and V planes at full size.
    I444,
}

impl PixelFormat {
    /// Bytes per row and rows of plane `i`, for a `w`×`h` picture.
    pub fn plane_size(self, i: usize, w: usize, h: usize) -> (usize, usize) {
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        match (self, i) {
            (_, 0) | (PixelFormat::I444, _) => (w, h),
            (PixelFormat::Nv12, _) => (2 * cw, ch),
            (PixelFormat::I420, _) => (cw, ch),
        }
    }

    pub fn planes(self) -> usize {
        if self == PixelFormat::Nv12 { 2 } else { 3 }
    }
}

/// Software decoders are offered up to this size.
const SOFTWARE_MAX: u32 = 8192;

fn software_decoder(codec: Codec) -> *const ff::AVCodec {
    // SAFETY: lookups by id and by NUL-terminated name.
    unsafe {
        match codec {
            Codec::H264 => ff::avcodec_find_decoder(ff::AVCodecID::AV_CODEC_ID_H264),
            Codec::Hevc => ff::avcodec_find_decoder(ff::AVCodecID::AV_CODEC_ID_HEVC),
            // FFmpeg's own AV1 decoder only drives hardware.
            Codec::Av1 => ff::avcodec_find_decoder_by_name(c"libdav1d".as_ptr()),
        }
    }
}

fn codec_id(codec: Codec) -> ff::AVCodecID {
    match codec {
        Codec::H264 => ff::AVCodecID::AV_CODEC_ID_H264,
        Codec::Hevc => ff::AVCodecID::AV_CODEC_ID_HEVC,
        Codec::Av1 => ff::AVCodecID::AV_CODEC_ID_AV1,
    }
}

/// The platform's hardware decoder: FFmpeg's device type, the surfaces it
/// decodes to, and its name.
#[cfg(target_os = "linux")]
const HW: Option<(ff::AVHWDeviceType, ff::AVPixelFormat, &str)> =
    Some((ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI, ff::AVPixelFormat::AV_PIX_FMT_VAAPI, "VA-API"));
#[cfg(target_os = "macos")]
const HW: Option<(ff::AVHWDeviceType, ff::AVPixelFormat, &str)> = Some((
    ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX,
    ff::AVPixelFormat::AV_PIX_FMT_VIDEOTOOLBOX,
    "VideoToolbox",
));
#[cfg(target_os = "windows")]
const HW: Option<(ff::AVHWDeviceType, ff::AVPixelFormat, &str)> =
    Some((ff::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA, ff::AVPixelFormat::AV_PIX_FMT_D3D11, "D3D11VA"));
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
const HW: Option<(ff::AVHWDeviceType, ff::AVPixelFormat, &str)> = None;

/// Whether to try `format` in hardware: 4:2:0 only, which is what the
/// renderer takes from hardware surfaces. Only VA-API is asked what it
/// decodes; elsewhere AV1, which few machines decode in hardware, stays in
/// software, since FFmpeg's own AV1 decoder can't fall back to it.
fn try_hardware(format: Format) -> bool {
    HW.is_some() && format.chroma == Chroma::Yuv420 && (cfg!(target_os = "linux") || format.codec != Codec::Av1)
}

/// What this machine can decode: in hardware where VA-API can, else in
/// software. Elsewhere than Linux the hardware isn't asked, so everything
/// is offered as software, and decoded in hardware where it turns out to
/// work.
pub fn offered(hardware: bool) -> Vec<DecoderCaps> {
    #[cfg(target_os = "linux")]
    let hw = if hardware {
        farsight_va::query(std::path::Path::new(RENDER_NODE), farsight_va::Direction::Decode)
    } else {
        Vec::new()
    };
    #[cfg(not(target_os = "linux"))]
    let hw: Vec<farsight_proto::codec::DecoderCaps> = {
        let _ = hardware;
        Vec::new()
    };
    let mut out = Vec::new();
    for format in farsight_proto::codec::FORMATS {
        if let Some(s) = hw.iter().find(|s| s.format == format && format.chroma == Chroma::Yuv420) {
            out.push(DecoderCaps {
                format,
                max_width: s.max_width,
                max_height: s.max_height,
                hardware: true,
                partial_decode: false,
            });
        } else if !software_decoder(format.codec).is_null() {
            out.push(DecoderCaps {
                format,
                max_width: SOFTWARE_MAX,
                max_height: SOFTWARE_MAX,
                hardware: false,
                partial_decode: false,
            });
        }
    }
    out
}

/// The VA-API device decoders open, as FFmpeg's default.
#[cfg(target_os = "linux")]
const RENDER_NODE: &str = "/dev/dri/renderD128";

/// One decoded picture in memory.
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub planes: Vec<Vec<u8>>,
    /// Bytes per row of each plane.
    pub strides: Vec<usize>,
}

impl Picture {
    /// MD5 of the picture's planes, packed, as FFmpeg's `framemd5` muxer
    /// hashes it: for checking decoded frames against a reference decode.
    pub fn md5(&self) -> String {
        let mut packed = Vec::new();
        for (i, (plane, stride)) in self.planes.iter().zip(&self.strides).enumerate() {
            let (bytes, rows) = self.format.plane_size(i, self.width as usize, self.height as usize);
            for row in 0..rows {
                packed.extend_from_slice(&plane[row * stride..][..bytes]);
            }
        }
        let mut digest = [0u8; 16];
        // SAFETY: `digest` holds the 16 bytes written; `packed` is read.
        unsafe { ff::av_md5_sum(digest.as_mut_ptr(), packed.as_ptr(), packed.len()) };
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// Pictures the decoder found damaged, or concealed part of, so far: a
/// reference was missing or the stream was corrupt. The core never hands
/// over a frame whose references the decoder lacks (§2), so this should
/// stay at zero. Software decoders notice; VA-API mostly doesn't.
pub static CORRUPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Errors FFmpeg logged so far. A decoder short of a reference may drop
/// the frame rather than show it damaged (HEVC does), and says so here.
pub static ERRORS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Counts FFmpeg's errors in [`ERRORS`], and logs as before.
pub fn count_errors() {
    // Only where FFmpeg's va_list is x86-64's System V one.
    #[cfg(all(target_arch = "x86_64", unix))]
    // SAFETY: the callback has the signature FFmpeg calls it with, and
    // hands the arguments on untouched.
    unsafe {
        unsafe extern "C" fn log(
            avcl: *mut libc::c_void,
            level: libc::c_int,
            fmt: *const libc::c_char,
            args: *mut ff::__va_list_tag,
        ) {
            if level <= ff::AV_LOG_ERROR {
                ERRORS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            // SAFETY: the arguments FFmpeg gave us, used once.
            unsafe { ff::av_log_default_callback(avcl, level, fmt, args) };
        }
        ff::av_log_set_callback(Some(log));
    }
}

pub struct Decoder {
    ctx: *mut ff::AVCodecContext,
    device: *mut ff::AVBufferRef,
    frame: *mut ff::AVFrame,
    sw_frame: *mut ff::AVFrame,
    packet: *mut ff::AVPacket,
    input: Vec<u8>,
    pub hardware: bool,
}

// SAFETY: the decoder owns its FFmpeg state outright and is used from one
// thread at a time.
unsafe impl Send for Decoder {}

fn check(ret: i32, what: &str) -> anyhow::Result<i32> {
    if ret < 0 {
        let mut buf = [0 as libc::c_char; 128];
        // SAFETY: buf is valid for its length; av_strerror NUL-terminates.
        let msg = unsafe {
            ff::av_strerror(ret, buf.as_mut_ptr(), buf.len());
            CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
        };
        bail!("{what}: {msg}");
    }
    Ok(ret)
}

/// Picks hardware surfaces when the decoder offers them.
unsafe extern "C" fn get_format(_ctx: *mut ff::AVCodecContext, fmts: *const ff::AVPixelFormat) -> ff::AVPixelFormat {
    // SAFETY: FFmpeg passes a list ending in AV_PIX_FMT_NONE.
    unsafe {
        let mut p = fmts;
        while *p != ff::AVPixelFormat::AV_PIX_FMT_NONE {
            if HW.is_some_and(|(_, fmt, _)| *p == fmt) {
                return *p;
            }
            p = p.add(1);
        }
        *fmts
    }
}

impl Decoder {
    /// A decoder for `format` in hardware if `hardware` and available,
    /// else in software.
    pub fn new(format: Format, hardware: bool) -> anyhow::Result<Self> {
        let mut dec = Decoder {
            ctx: ptr::null_mut(),
            device: ptr::null_mut(),
            frame: ptr::null_mut(),
            sw_frame: ptr::null_mut(),
            packet: ptr::null_mut(),
            input: Vec::new(),
            hardware: false,
        };
        // SAFETY: plain FFmpeg setup; every pointer is checked and owned by
        // `dec`, whose Drop frees it.
        unsafe {
            let hardware = hardware && try_hardware(format);
            let codec =
                if hardware { ff::avcodec_find_decoder(codec_id(format.codec)) } else { software_decoder(format.codec) };
            if codec.is_null() {
                bail!("FFmpeg has no {format} decoder");
            }
            dec.ctx = ff::avcodec_alloc_context3(codec);
            let c = &mut *dec.ctx;
            // Output each picture as soon as it is decoded; frame threads
            // would hold pictures back.
            c.flags |= ff::AV_CODEC_FLAG_LOW_DELAY as i32;
            c.thread_count = 1;
            if let Some((device, _, name)) = HW.filter(|_| hardware) {
                let ret = ff::av_hwdevice_ctx_create(&mut dec.device, device, ptr::null(), ptr::null_mut(), 0);
                match check(ret, &format!("{name} device")) {
                    Ok(_) => {
                        c.hw_device_ctx = ff::av_buffer_ref(dec.device);
                        c.get_format = Some(get_format);
                        dec.hardware = true;
                    }
                    Err(err) => tracing::warn!("{err:#}; decoding in software"),
                }
            }
            check(ff::avcodec_open2(dec.ctx, codec, ptr::null_mut()), &format!("opening the {format} decoder"))?;
            dec.frame = ff::av_frame_alloc();
            dec.sw_frame = ff::av_frame_alloc();
            dec.packet = ff::av_packet_alloc();
        }
        tracing::info!(%format, hardware = dec.hardware, "decoder ready");
        Ok(dec)
    }

    /// Decodes one whole frame (Annex B). Returns the picture it produced,
    /// if any.
    pub fn decode(&mut self, data: &[u8]) -> anyhow::Result<Option<Picture>> {
        // FFmpeg reads a little past the end of its input.
        self.input.clear();
        self.input.extend_from_slice(data);
        self.input.resize(data.len() + ff::AV_INPUT_BUFFER_PADDING_SIZE as usize, 0);
        let mut picture = None;
        // SAFETY: the packet borrows `input`, which outlives the call; the
        // frames are ours.
        unsafe {
            (*self.packet).data = self.input.as_mut_ptr();
            (*self.packet).size = data.len() as i32;
            let ret = ff::avcodec_send_packet(self.ctx, self.packet);
            (*self.packet).data = ptr::null_mut();
            (*self.packet).size = 0;
            check(ret, "decoding")?;
            loop {
                let ret = ff::avcodec_receive_frame(self.ctx, self.frame);
                if ret == AVERROR_EAGAIN {
                    break;
                }
                check(ret, "decoding")?;
                picture = Some(self.take_picture());
                ff::av_frame_unref(self.frame);
            }
        }
        picture.transpose()
    }

    unsafe fn take_picture(&mut self) -> anyhow::Result<Picture> {
        // SAFETY: frame holds a decoded picture.
        unsafe {
            let mut f = self.frame;
            if (*f).flags & ff::AV_FRAME_FLAG_CORRUPT != 0 || (*f).decode_error_flags != 0 {
                CORRUPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(pts = (*f).pts, "the decoder found a picture damaged");
            }
            if HW.is_some_and(|(_, fmt, _)| (*f).format == fmt as i32) {
                ff::av_frame_unref(self.sw_frame);
                check(ff::av_hwframe_transfer_data(self.sw_frame, f, 0), "reading the decoded surface")?;
                f = self.sw_frame;
            }
            let (w, h) = ((*f).width as usize, (*f).height as usize);
            let format = match (*f).format {
                x if x == ff::AVPixelFormat::AV_PIX_FMT_NV12 as i32 => PixelFormat::Nv12,
                x if x == ff::AVPixelFormat::AV_PIX_FMT_YUV420P as i32
                    || x == ff::AVPixelFormat::AV_PIX_FMT_YUVJ420P as i32 =>
                {
                    PixelFormat::I420
                }
                x if x == ff::AVPixelFormat::AV_PIX_FMT_YUV444P as i32
                    || x == ff::AVPixelFormat::AV_PIX_FMT_YUVJ444P as i32 =>
                {
                    PixelFormat::I444
                }
                other => bail!("unsupported decoded format {other}"),
            };
            let mut planes = Vec::new();
            let mut strides = Vec::new();
            for i in 0..format.planes() {
                let (_, rows) = format.plane_size(i, w, h);
                let stride = (*f).linesize[i] as usize;
                planes.push(std::slice::from_raw_parts((*f).data[i], stride * rows).to_vec());
                strides.push(stride);
            }
            Ok(Picture { width: w as u32, height: h as u32, format, planes, strides })
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: each pointer is null or owned by us.
        unsafe {
            ff::av_packet_free(&mut self.packet);
            ff::av_frame_free(&mut self.frame);
            ff::av_frame_free(&mut self.sw_frame);
            ff::avcodec_free_context(&mut self.ctx);
            ff::av_buffer_unref(&mut self.device);
        }
    }
}
