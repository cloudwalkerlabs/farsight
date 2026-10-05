//! H.264 decoding through FFmpeg: VA-API when the machine has it, software
//! otherwise (§3). Decoded pictures are copied to memory for upload; the
//! zero-copy path (dmabuf → EGLImage) is later work.

use std::ffi::CStr;
use std::ptr;

use anyhow::bail;
use ffmpeg_sys_next as ff;

const AVERROR_EAGAIN: i32 = -libc::EAGAIN;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Y plane, then interleaved U/V at half size.
    Nv12,
    /// Y, U and V planes, U and V at half size.
    I420,
}

/// One decoded picture in memory.
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub format: Format,
    pub planes: Vec<Vec<u8>>,
    /// Bytes per row of each plane.
    pub strides: Vec<usize>,
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

/// Picks VA-API surfaces when the decoder offers them.
unsafe extern "C" fn get_format(_ctx: *mut ff::AVCodecContext, fmts: *const ff::AVPixelFormat) -> ff::AVPixelFormat {
    // SAFETY: FFmpeg passes a list ending in AV_PIX_FMT_NONE.
    unsafe {
        let mut p = fmts;
        while *p != ff::AVPixelFormat::AV_PIX_FMT_NONE {
            if *p == ff::AVPixelFormat::AV_PIX_FMT_VAAPI {
                return *p;
            }
            p = p.add(1);
        }
        *fmts
    }
}

impl Decoder {
    /// A decoder using VA-API if `hardware` and available, else software.
    pub fn new(hardware: bool) -> anyhow::Result<Self> {
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
            let codec = ff::avcodec_find_decoder(ff::AVCodecID::AV_CODEC_ID_H264);
            if codec.is_null() {
                bail!("FFmpeg has no H.264 decoder");
            }
            dec.ctx = ff::avcodec_alloc_context3(codec);
            let c = &mut *dec.ctx;
            // Output each picture as soon as it is decoded; frame threads
            // would hold pictures back.
            c.flags |= ff::AV_CODEC_FLAG_LOW_DELAY as i32;
            c.thread_count = 1;
            if hardware {
                let ret = ff::av_hwdevice_ctx_create(
                    &mut dec.device,
                    ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                    ptr::null(),
                    ptr::null_mut(),
                    0,
                );
                match check(ret, "VA-API device") {
                    Ok(_) => {
                        c.hw_device_ctx = ff::av_buffer_ref(dec.device);
                        c.get_format = Some(get_format);
                        dec.hardware = true;
                    }
                    Err(err) => tracing::warn!("{err:#}; decoding in software"),
                }
            }
            check(ff::avcodec_open2(dec.ctx, codec, ptr::null_mut()), "opening the H.264 decoder")?;
            dec.frame = ff::av_frame_alloc();
            dec.sw_frame = ff::av_frame_alloc();
            dec.packet = ff::av_packet_alloc();
        }
        tracing::info!(hardware = dec.hardware, "decoder ready");
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
            if (*f).format == ff::AVPixelFormat::AV_PIX_FMT_VAAPI as i32 {
                ff::av_frame_unref(self.sw_frame);
                check(ff::av_hwframe_transfer_data(self.sw_frame, f, 0), "reading the decoded surface")?;
                f = self.sw_frame;
            }
            let (w, h) = ((*f).width as usize, (*f).height as usize);
            let format = match (*f).format {
                x if x == ff::AVPixelFormat::AV_PIX_FMT_NV12 as i32 => Format::Nv12,
                x if x == ff::AVPixelFormat::AV_PIX_FMT_YUV420P as i32
                    || x == ff::AVPixelFormat::AV_PIX_FMT_YUVJ420P as i32 =>
                {
                    Format::I420
                }
                other => bail!("unsupported decoded format {other}"),
            };
            let rows = match format {
                Format::Nv12 => vec![h, h.div_ceil(2)],
                Format::I420 => vec![h, h.div_ceil(2), h.div_ceil(2)],
            };
            let mut planes = Vec::new();
            let mut strides = Vec::new();
            for (i, rows) in rows.into_iter().enumerate() {
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
