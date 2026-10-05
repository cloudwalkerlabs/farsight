//! An FFmpeg encoder, shared by the VA-API and software backends.

use std::ffi::{CStr, CString};
use std::ptr;

use anyhow::bail;
use ffmpeg_sys_next as ff;

use super::FrameKind;

pub fn check(ret: i32, what: &str) -> anyhow::Result<i32> {
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

const AVERROR_EAGAIN: i32 = -libc::EAGAIN;

/// An open `AVCodecContext` for one size: one video epoch.
pub struct FfEncoder {
    ctx: *mut ff::AVCodecContext,
    packet: *mut ff::AVPacket,
}

// SAFETY: the encoder owns its context outright and is used from one thread
// at a time; a hardware frame pool it shares is refcounted and thread-safe.
unsafe impl Send for FfEncoder {}

impl FfEncoder {
    /// Opens encoder `name` for `width`×`height` with the settings every
    /// backend shares (no B-frames, a long GOP, low delay, BT.709 limited
    /// range). `setup` sets the rest on the context; `opts` are the
    /// encoder's private options.
    pub fn open(
        name: &'static CStr,
        width: i32,
        height: i32,
        setup: impl FnOnce(&mut ff::AVCodecContext),
        opts: &[(&CStr, String)],
    ) -> anyhow::Result<Self> {
        // SAFETY: plain FFmpeg setup; the context is owned by `enc`, whose
        // Drop frees it on every error path.
        unsafe {
            let codec = ff::avcodec_find_encoder_by_name(name.as_ptr());
            if codec.is_null() {
                bail!("FFmpeg has no {} encoder", name.to_string_lossy());
            }
            let enc = Self { ctx: ff::avcodec_alloc_context3(codec), packet: ff::av_packet_alloc() };
            let c = &mut *enc.ctx;
            c.width = width;
            c.height = height;
            c.time_base = ff::AVRational { num: 1, den: 1_000_000 };
            c.framerate = ff::AVRational { num: 60, den: 1 };
            c.max_b_frames = 0;
            c.gop_size = 600;
            c.flags |= ff::AV_CODEC_FLAG_LOW_DELAY as i32;
            c.color_range = ff::AVColorRange::AVCOL_RANGE_MPEG;
            c.colorspace = ff::AVColorSpace::AVCOL_SPC_BT709;
            c.color_primaries = ff::AVColorPrimaries::AVCOL_PRI_BT709;
            c.color_trc = ff::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
            setup(c);

            let mut dict: *mut ff::AVDictionary = ptr::null_mut();
            for (k, v) in opts {
                let v = CString::new(v.as_str())?;
                ff::av_dict_set(&mut dict, k.as_ptr(), v.as_ptr(), 0);
            }
            let ret = ff::avcodec_open2(enc.ctx, codec, &mut dict);
            // Anything left was not recognised.
            let mut e: *const ff::AVDictionaryEntry = ptr::null();
            loop {
                e = ff::av_dict_get(dict, c"".as_ptr(), e, ff::AV_DICT_IGNORE_SUFFIX);
                if e.is_null() {
                    break;
                }
                tracing::warn!(encoder = %name.to_string_lossy(), option = %CStr::from_ptr((*e).key).to_string_lossy(), "option ignored");
            }
            ff::av_dict_free(&mut dict);
            check(ret, &format!("opening {}", name.to_string_lossy()))?;
            Ok(enc)
        }
    }

    /// Encodes `frame` (consumed) into `out`; returns whether the result is
    /// a keyframe.
    ///
    /// # Safety
    /// `frame` must be a frame this encoder accepts, owned by the caller.
    pub unsafe fn encode(
        &mut self,
        mut frame: *mut ff::AVFrame,
        pts_us: i64,
        kind: FrameKind,
        out: &mut Vec<u8>,
    ) -> anyhow::Result<bool> {
        let mut keyframe = false;
        // SAFETY: as the caller promises; the packet is ours.
        unsafe {
            (*frame).pts = pts_us;
            (*frame).pict_type = match kind {
                FrameKind::Keyframe => ff::AVPictureType::AV_PICTURE_TYPE_I,
                _ => ff::AVPictureType::AV_PICTURE_TYPE_NONE,
            };
            let ret = ff::avcodec_send_frame(self.ctx, frame);
            ff::av_frame_free(&mut frame);
            check(ret, "send frame")?;
            loop {
                let ret = ff::avcodec_receive_packet(self.ctx, self.packet);
                if ret == AVERROR_EAGAIN {
                    break;
                }
                check(ret, "receive packet")?;
                let p = &*self.packet;
                out.extend_from_slice(std::slice::from_raw_parts(p.data, p.size as usize));
                keyframe |= p.flags & ff::AV_PKT_FLAG_KEY != 0;
                ff::av_packet_unref(self.packet);
            }
        }
        Ok(keyframe)
    }
}

impl Drop for FfEncoder {
    fn drop(&mut self) {
        // SAFETY: each pointer is null or owned by us.
        unsafe {
            ff::av_packet_free(&mut self.packet);
            ff::avcodec_free_context(&mut self.ctx);
        }
    }
}
