//! VA-API H.264 encoding through FFmpeg (docs/design.md §2).
//!
//! Input surfaces come from an FFmpeg VA-API frame pool. Each surface is
//! mapped once to dmabufs, one per NV12 plane (R8 luma, GR88 chroma), so the
//! GLES conversion pass can render straight into it: no copy between the
//! shader and the encoder.
//!
//! [`Surfaces::new`] returns two halves: [`Surfaces`] stays with the renderer
//! on the main thread, and [`Codec`] moves to the encode thread. FFmpeg's
//! frame pool is thread-safe, and each half is used from one thread only.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::path::Path;
use std::ptr;

use anyhow::{Context, bail};
use ffmpeg_sys_next as ff;
use smithay::backend::allocator::dmabuf::{Dmabuf, DmabufFlags};
use smithay::backend::allocator::{Fourcc, Modifier};

/// One encoder input surface, as the two render targets of the conversion.
#[derive(Clone)]
pub struct Nv12Target {
    pub luma: Dmabuf,
    pub chroma: Dmabuf,
}

/// The frame pool and its mapped render targets.
pub struct Surfaces {
    pub width: i32,
    pub height: i32,
    device: *mut ff::AVBufferRef,
    frames: *mut ff::AVBufferRef,
    targets: HashMap<usize, Nv12Target>,
}

/// The encoder proper.
pub struct Codec {
    ctx: *mut ff::AVCodecContext,
    packet: *mut ff::AVPacket,
}

// SAFETY: a Codec owns its FFmpeg context outright and is only used from one
// thread at a time; the frame pool it shares with `Surfaces` is refcounted
// and thread-safe.
unsafe impl Send for Codec {}

/// A surface taken from the pool, owned until passed to [`Codec::encode`]
/// (or dropped).
pub struct Surface(*mut ff::AVFrame);

// SAFETY: as for Codec; the frame is a refcounted pool entry with one owner.
unsafe impl Send for Surface {}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: null or a frame we own.
        unsafe { ff::av_frame_free(&mut self.0) };
    }
}

fn check(ret: i32, what: &str) -> anyhow::Result<i32> {
    if ret < 0 {
        let mut buf = [0 as libc::c_char; 128];
        // SAFETY: buf is valid for its length; av_strerror NUL-terminates.
        let msg = unsafe {
            ff::av_strerror(ret, buf.as_mut_ptr(), buf.len());
            std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
        };
        bail!("{what}: {msg}");
    }
    Ok(ret)
}

const AVERROR_EAGAIN: i32 = -libc::EAGAIN;

impl Surfaces {
    /// Opens the frame pool and the encoder for one size (one video epoch).
    pub fn new(render_node: &Path, width: i32, height: i32, qp: u32) -> anyhow::Result<(Surfaces, Codec)> {
        let node = CString::new(render_node.as_os_str().as_encoded_bytes())?;
        let mut enc =
            Surfaces { width, height, device: ptr::null_mut(), frames: ptr::null_mut(), targets: HashMap::new() };
        let mut codec = Codec { ctx: ptr::null_mut(), packet: ptr::null_mut() };
        // SAFETY: plain FFmpeg setup; every pointer is checked before use and
        // owned by `enc` or `codec`, whose Drop frees it.
        unsafe {
            check(
                ff::av_hwdevice_ctx_create(
                    &mut enc.device,
                    ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                    node.as_ptr(),
                    ptr::null_mut(),
                    0,
                ),
                "VA-API device",
            )?;
            enc.frames = ff::av_hwframe_ctx_alloc(enc.device);
            if enc.frames.is_null() {
                bail!("av_hwframe_ctx_alloc");
            }
            let fc = (*enc.frames).data as *mut ff::AVHWFramesContext;
            (*fc).format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*fc).sw_format = ff::AVPixelFormat::AV_PIX_FMT_NV12;
            (*fc).width = width;
            (*fc).height = height;
            // Room for a surface being converted, one queued for the encode
            // thread and the encoder's own references.
            (*fc).initial_pool_size = 8;
            check(ff::av_hwframe_ctx_init(enc.frames), "VA-API frame pool")?;

            let name = c"h264_vaapi";
            let h264 = ff::avcodec_find_encoder_by_name(name.as_ptr());
            if h264.is_null() {
                bail!("FFmpeg has no h264_vaapi encoder");
            }
            codec.ctx = ff::avcodec_alloc_context3(h264);
            let c = &mut *codec.ctx;
            c.width = width;
            c.height = height;
            c.time_base = ff::AVRational { num: 1, den: 1_000_000 };
            c.framerate = ff::AVRational { num: 60, den: 1 };
            c.pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
            c.hw_frames_ctx = ff::av_buffer_ref(enc.frames);
            c.max_b_frames = 0;
            c.gop_size = 600;
            c.flags |= ff::AV_CODEC_FLAG_LOW_DELAY as i32;
            c.color_range = ff::AVColorRange::AVCOL_RANGE_MPEG;
            c.colorspace = ff::AVColorSpace::AVCOL_SPC_BT709;
            c.color_primaries = ff::AVColorPrimaries::AVCOL_PRI_BT709;
            c.color_trc = ff::AVColorTransferCharacteristic::AVCOL_TRC_BT709;

            let mut opts: *mut ff::AVDictionary = ptr::null_mut();
            let qp = CString::new(qp.to_string())?;
            for (k, v) in [
                (c"async_depth", c"1"),
                (c"rc_mode", c"CQP"),
                (c"profile", c"high"),
            ] {
                ff::av_dict_set(&mut opts, k.as_ptr(), v.as_ptr(), 0);
            }
            ff::av_dict_set(&mut opts, c"qp".as_ptr(), qp.as_ptr(), 0);
            let ret = ff::avcodec_open2(codec.ctx, h264, &mut opts);
            ff::av_dict_free(&mut opts);
            check(ret, "opening h264_vaapi")?;
            codec.packet = ff::av_packet_alloc();
        }
        tracing::info!(width, height, "encoder ready (h264_vaapi, CQP, async_depth 1)");
        Ok((enc, codec))
    }

    /// Take a free surface from the pool, with its render targets.
    pub fn next_surface(&mut self) -> anyhow::Result<(Surface, Nv12Target)> {
        // SAFETY: frames is an initialised VA-API frame pool.
        unsafe {
            let frame = Surface(ff::av_frame_alloc());
            check(ff::av_hwframe_get_buffer(self.frames, frame.0, 0), "get VA surface")?;
            let surface = (*frame.0).data[3] as usize;
            if let Some(t) = self.targets.get(&surface) {
                return Ok((frame, t.clone()));
            }
            let target = self.map_surface(frame.0)?;
            self.targets.insert(surface, target.clone());
            Ok((frame, target))
        }
    }

    /// Map a VA surface to DRM PRIME and wrap its two planes as dmabufs.
    unsafe fn map_surface(&self, frame: *mut ff::AVFrame) -> anyhow::Result<Nv12Target> {
        // SAFETY: the caller passes a frame from our VA-API pool.
        unsafe {
            let mut drm = ff::av_frame_alloc();
            (*drm).format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            let flags = ff::AV_HWFRAME_MAP_WRITE as i32 | ff::AV_HWFRAME_MAP_OVERWRITE as i32;
            let ret = ff::av_hwframe_map(drm, frame, flags);
            if ret < 0 {
                ff::av_frame_free(&mut drm);
                check(ret, "map VA surface to DRM PRIME")?;
            }
            let desc = &*((*drm).data[0] as *const ff::AVDRMFrameDescriptor);
            let mut planes = Vec::new();
            for layer in &desc.layers[..desc.nb_layers as usize] {
                for plane in &layer.planes[..layer.nb_planes as usize] {
                    let object = &desc.objects[plane.object_index as usize];
                    planes.push((object.fd, object.format_modifier, plane.offset, plane.pitch));
                }
            }
            tracing::info!(
                surface = (*frame).data[3] as usize,
                layers = desc.nb_layers, objects = desc.nb_objects,
                modifier = format!("{:#x}", desc.objects[0].format_modifier),
                "mapped encoder surface"
            );
            let result = (|| {
                if planes.len() != 2 {
                    bail!("expected 2 NV12 planes, got {}", planes.len());
                }
                let (w, h) = (self.width, self.height);
                let wrap = |i: usize, fourcc, size: (i32, i32)| -> anyhow::Result<Dmabuf> {
                    let (fd, modifier, offset, pitch) = planes[i];
                    let fd: OwnedFd = BorrowedFd::borrow_raw(fd).try_clone_to_owned()?;
                    let mut b = Dmabuf::builder(size, fourcc, Modifier::from(modifier), DmabufFlags::empty());
                    b.add_plane(fd, 0, offset as u32, pitch as u32);
                    b.build().context("building plane dmabuf")
                };
                Ok(Nv12Target {
                    luma: wrap(0, Fourcc::R8, (w, h))?,
                    chroma: wrap(1, Fourcc::Gr88, ((w + 1) / 2, (h + 1) / 2))?,
                })
            })();
            ff::av_frame_free(&mut drm);
            result
        }
    }

}

impl Drop for Surfaces {
    fn drop(&mut self) {
        self.targets.clear();
        // SAFETY: each pointer is null or owned by us.
        unsafe {
            ff::av_buffer_unref(&mut self.frames);
            ff::av_buffer_unref(&mut self.device);
        }
    }
}

impl Codec {
    /// Encode one converted surface into `out` (Annex B); returns whether
    /// it is a keyframe. With `force_keyframe` the encoder starts a new GOP
    /// with an IDR, SPS and PPS included.
    pub fn encode(
        &mut self,
        frame: Surface,
        pts_us: i64,
        force_keyframe: bool,
        out: &mut Vec<u8>,
    ) -> anyhow::Result<bool> {
        let mut keyframe = false;
        // SAFETY: frame came from Surfaces::next_surface and is consumed here.
        unsafe {
            (*frame.0).pts = pts_us;
            (*frame.0).pict_type = match force_keyframe {
                true => ff::AVPictureType::AV_PICTURE_TYPE_I,
                false => ff::AVPictureType::AV_PICTURE_TYPE_NONE,
            };
            let ret = ff::avcodec_send_frame(self.ctx, frame.0);
            drop(frame);
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

impl Drop for Codec {
    fn drop(&mut self) {
        // SAFETY: each pointer is null or owned by us.
        unsafe {
            ff::av_packet_free(&mut self.packet);
            ff::avcodec_free_context(&mut self.ctx);
        }
    }
}
