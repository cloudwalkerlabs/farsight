//! VA-API encoding through FFmpeg (docs/design.md §2).
//!
//! Input surfaces come from an FFmpeg VA-API frame pool. Each surface is
//! mapped once to dmabufs, one per NV12 plane (R8 luma, GR88 chroma), so the
//! GLES conversion pass can render straight into it: no copy between the
//! shader and the encoder.
//!
//! [`open`] returns two halves: [`Surfaces`] stays with the renderer on the
//! main thread, and the encoder moves to the encode thread. FFmpeg's frame
//! pool is thread-safe, and each half is used from one thread only.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::path::Path;
use std::ptr;

use anyhow::{Context, bail};
use farsight_proto::codec::Codec;
use ffmpeg_sys_next as ff;
use smithay::backend::allocator::dmabuf::{Dmabuf, DmabufFlags};
use smithay::backend::allocator::{Fourcc, Modifier};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::{Bind, Frame, Renderer, Texture};
use smithay::utils::{Rectangle, Transform};

use super::ffmpeg::{FfEncoder, check};
use super::{EncoderInfo, Settings, Shaders};

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

/// A surface taken from the pool, owned until passed to the encoder
/// (or dropped).
pub struct Surface(*mut ff::AVFrame);

// SAFETY: the frame is a refcounted pool entry with one owner.
unsafe impl Send for Surface {}

impl Surface {
    /// The frame, now owned by the caller.
    pub fn into_raw(mut self) -> *mut ff::AVFrame {
        std::mem::replace(&mut self.0, ptr::null_mut())
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: null or a frame we own.
        unsafe { ff::av_frame_free(&mut self.0) };
    }
}

/// The QP range FFmpeg's VA-API encoders map an ROI's offset onto.
pub fn quant_range(codec: Codec) -> u32 {
    match codec {
        Codec::H264 | Codec::Hevc => 51,
        Codec::Av1 => 255,
    }
}

pub fn encoder_name(codec: Codec) -> &'static std::ffi::CStr {
    match codec {
        Codec::H264 => c"h264_vaapi",
        Codec::Hevc => c"hevc_vaapi",
        Codec::Av1 => c"av1_vaapi",
    }
}

/// Opens the frame pool and the encoder for one size (one video epoch).
pub fn open(
    info: &EncoderInfo,
    render_node: &Path,
    width: i32,
    height: i32,
    settings: &Settings,
) -> anyhow::Result<(Surfaces, FfEncoder)> {
    let node = CString::new(render_node.as_os_str().as_encoded_bytes())?;
    let mut enc = Surfaces { width, height, device: ptr::null_mut(), frames: ptr::null_mut(), targets: HashMap::new() };
    // SAFETY: plain FFmpeg setup; every pointer is checked before use and
    // owned by `enc`, whose Drop frees it.
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
    }
    let name = encoder_name(info.caps.format.codec);
    let profile = match info.caps.format.codec {
        Codec::H264 => "high",
        Codec::Hevc | Codec::Av1 => "main",
    };
    let frames = enc.frames;
    let mut opts = vec![
        (c"async_depth", "1".to_string()),
        (c"rc_mode", "CQP".to_string()),
        (c"profile", profile.to_string()),
        (c"qp", settings.qp.to_string()),
    ];
    if info.low_power {
        opts.push((c"low_power", "1".to_string()));
    }
    let codec = FfEncoder::open(
        name,
        width,
        height,
        |c| {
            c.pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
            // SAFETY: frames is a valid frame pool; the context takes a
            // reference of its own.
            c.hw_frames_ctx = unsafe { ff::av_buffer_ref(frames) };
        },
        &opts,
    )?;
    tracing::info!(width, height, encoder = %name.to_string_lossy(), qp = settings.qp, "encoder ready (CQP, async_depth 1)");
    Ok((enc, codec))
}

impl Surfaces {
    /// Converts `texture` into a free surface. The returned sync point
    /// signals when the GPU has written it.
    pub fn convert(
        &mut self,
        renderer: &mut GlesRenderer,
        shaders: &Shaders,
        texture: &GlesTexture,
    ) -> anyhow::Result<(Surface, SyncPoint)> {
        let size = texture.size();
        let (w, h) = (size.w, size.h);
        let (surface, mut target) = self.next_surface()?;
        let src = Rectangle::from_size(size.to_f64());
        let mut sync = SyncPoint::signaled();
        for (dmabuf, prog, (tw, th)) in [
            (&mut target.luma, &shaders.luma, (w, h)),
            (&mut target.chroma, &shaders.chroma, ((w + 1) / 2, (h + 1) / 2)),
        ] {
            let mut fb = renderer.bind(dmabuf)?;
            let dst = Rectangle::from_size((tw, th).into());
            let mut frame = renderer.render(&mut fb, (tw, th).into(), Transform::Normal)?;
            frame.render_texture_from_to(texture, src, dst, &[dst], &[dst], Transform::Normal, 1.0, Some(prog), &[])?;
            sync = frame.finish()?;
        }
        Ok((surface, sync))
    }

    /// Take a free surface from the pool, with its render targets.
    fn next_surface(&mut self) -> anyhow::Result<(Surface, Nv12Target)> {
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

