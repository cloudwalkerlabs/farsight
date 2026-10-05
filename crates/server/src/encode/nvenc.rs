//! NVENC, bound directly (docs/design.md §2): FFmpeg's NVENC wants a newer
//! API than older GPUs' drivers offer, so farsight speaks API 12.1, which
//! drivers from 530 on accept. Both `libcuda` and `libnvidia-encode` are
//! loaded at run time; without them there is simply no NVENC.
//!
//! The host renders on its own render node, which is usually not the NVIDIA
//! GPU, so pictures reach NVENC through memory: the conversion is read back
//! (`readback`) and copied into NVENC's input buffers. Importing the nested
//! buffer into CUDA when both are on the NVIDIA GPU is later work.

mod sys {
    #![allow(warnings, unsafe_op_in_unsafe_fn, clippy::all)]
    include!(concat!(env!("OUT_DIR"), "/nvenc.rs"));
}

use std::ffi::c_void;
use std::ptr;
use std::sync::OnceLock;

use anyhow::{Context, bail};
use farsight_proto::codec::{Chroma, Codec, EncoderCaps, Format};
use sys::*;

use super::{FrameKind, MemFrame, PlaneLayout, Settings};

const fn struct_version(ver: u32) -> u32 {
    NVENCAPI_MAJOR_VERSION | (NVENCAPI_MINOR_VERSION << 24) | (ver << 16) | (0x7 << 28)
}
const API_VERSION: u32 = NVENCAPI_MAJOR_VERSION | (NVENCAPI_MINOR_VERSION << 24);
const FUNCTION_LIST_VER: u32 = struct_version(2);
const OPEN_SESSION_VER: u32 = struct_version(1);
const CAPS_PARAM_VER: u32 = struct_version(1);
const CREATE_INPUT_VER: u32 = struct_version(1);
const CREATE_BITSTREAM_VER: u32 = struct_version(1);
const CONFIG_VER: u32 = struct_version(8) | 1 << 31;
const INITIALIZE_VER: u32 = struct_version(6) | 1 << 31;
const PRESET_CONFIG_VER: u32 = struct_version(4) | 1 << 31;
const PIC_PARAMS_VER: u32 = struct_version(6) | 1 << 31;
const LOCK_BITSTREAM_VER: u32 = struct_version(1) | 1 << 31;
const LOCK_INPUT_VER: u32 = struct_version(1);
const RECONFIGURE_VER: u32 = struct_version(1) | 1 << 31;
const INFINITE_GOP: u32 = 0xffff_ffff;

/// Frames kept for reference (each frame references only the newest): how
/// far back reference frame invalidation can reach.
const DPB: u32 = 8;

const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> GUID {
    GUID { Data1: d1, Data2: d2, Data3: d3, Data4: d4 }
}
const CODEC_H264: GUID = guid(0x6bc82762, 0x4e63, 0x4ca4, [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf]);
const CODEC_HEVC: GUID = guid(0x790cdc88, 0x4522, 0x4d7b, [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03]);
const CODEC_AV1: GUID = guid(0x0a352289, 0x0aa7, 0x4759, [0x86, 0x2d, 0x5d, 0x15, 0xcd, 0x16, 0xd2, 0x54]);
const H264_HIGH: GUID = guid(0xe7cbc309, 0x4f7a, 0x4b89, [0xaf, 0x2a, 0xd5, 0x37, 0xc9, 0x2b, 0xe3, 0x10]);
const H264_HIGH_444: GUID = guid(0x7ac663cb, 0xa598, 0x4960, [0xb8, 0x44, 0x33, 0x9b, 0x26, 0x1a, 0x7d, 0x52]);
const HEVC_MAIN: GUID = guid(0xb514c39a, 0xb55b, 0x40fa, [0x87, 0x8f, 0xf1, 0x25, 0x3b, 0x4d, 0xfd, 0xec]);
const HEVC_FREXT: GUID = guid(0x51ec32b5, 0x1b4c, 0x453c, [0x9c, 0xbd, 0xb6, 0x16, 0xbd, 0x62, 0x13, 0x41]);
const AV1_MAIN: GUID = guid(0x5f2a39f5, 0xf14e, 0x4f95, [0x9a, 0x9e, 0xb7, 0x6d, 0x56, 0x8f, 0xcf, 0x97]);
const PRESET_P1: GUID = guid(0xfc0a8d3e, 0x45f8, 0x4cf8, [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf]);

fn same(a: &GUID, b: &GUID) -> bool {
    (a.Data1, a.Data2, a.Data3, a.Data4) == (b.Data1, b.Data2, b.Data3, b.Data4)
}

fn codec_guid(codec: Codec) -> GUID {
    match codec {
        Codec::H264 => CODEC_H264,
        Codec::Hevc => CODEC_HEVC,
        Codec::Av1 => CODEC_AV1,
    }
}

type CUresult = i32;
type CUdevice = i32;
type CUcontext = *mut c_void;

/// The CUDA driver and NVENC, loaded once.
struct Api {
    cu_ctx_create: unsafe extern "C" fn(*mut CUcontext, u32, CUdevice) -> CUresult,
    cu_ctx_destroy: unsafe extern "C" fn(CUcontext) -> CUresult,
    cu_ctx_push: unsafe extern "C" fn(CUcontext) -> CUresult,
    cu_ctx_pop: unsafe extern "C" fn(*mut CUcontext) -> CUresult,
    nvenc: NV_ENCODE_API_FUNCTION_LIST,
    device: CUdevice,
    _libs: (libloading::Library, libloading::Library),
}

// SAFETY: the function table is immutable after loading; CUDA and NVENC
// calls are thread-safe as long as each session is used by one thread.
unsafe impl Send for Api {}
// SAFETY: as above.
unsafe impl Sync for Api {}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| match load() {
        Ok(api) => Some(api),
        Err(err) => {
            tracing::info!("no NVENC: {err:#}");
            None
        }
    })
    .as_ref()
}

fn load() -> anyhow::Result<Api> {
    // SAFETY: loading the vendor libraries and looking up symbols with the
    // signatures their headers declare.
    unsafe {
        let cuda = libloading::Library::new("libcuda.so.1").context("loading libcuda")?;
        let enc = libloading::Library::new("libnvidia-encode.so.1").context("loading libnvidia-encode")?;
        let cu_init: libloading::Symbol<unsafe extern "C" fn(u32) -> CUresult> = cuda.get(b"cuInit")?;
        let cu_device_get: libloading::Symbol<unsafe extern "C" fn(*mut CUdevice, i32) -> CUresult> =
            cuda.get(b"cuDeviceGet")?;
        let r = cu_init(0);
        if r != 0 {
            bail!("cuInit: {r}");
        }
        let mut device = 0;
        let r = cu_device_get(&mut device, 0);
        if r != 0 {
            bail!("cuDeviceGet: {r}");
        }
        let max_version: libloading::Symbol<unsafe extern "C" fn(*mut u32) -> NVENCSTATUS> =
            enc.get(b"NvEncodeAPIGetMaxSupportedVersion")?;
        let mut max = 0;
        check(max_version(&mut max), "NvEncodeAPIGetMaxSupportedVersion")?;
        let ours = (NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION;
        if max < ours {
            bail!("the driver offers NVENC API {}.{}; farsight needs {}.{}", max >> 4, max & 0xf, ours >> 4, ours & 0xf);
        }
        let create: libloading::Symbol<unsafe extern "C" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS> =
            enc.get(b"NvEncodeAPICreateInstance")?;
        let mut nvenc = NV_ENCODE_API_FUNCTION_LIST { version: FUNCTION_LIST_VER, ..Default::default() };
        check(create(&mut nvenc), "NvEncodeAPICreateInstance")?;
        Ok(Api {
            cu_ctx_create: *cuda.get(b"cuCtxCreate_v2")?,
            cu_ctx_destroy: *cuda.get(b"cuCtxDestroy_v2")?,
            cu_ctx_push: *cuda.get(b"cuCtxPushCurrent_v2")?,
            cu_ctx_pop: *cuda.get(b"cuCtxPopCurrent_v2")?,
            nvenc,
            device,
            _libs: (cuda, enc),
        })
    }
}

fn check(status: NVENCSTATUS, what: &str) -> anyhow::Result<()> {
    if status != NV_ENC_SUCCESS {
        bail!("{what}: NVENCSTATUS {status}");
    }
    Ok(())
}

/// Calls an NVENC entry point from the function list.
macro_rules! nv {
    ($api:expr, $f:ident($($arg:expr),*)) => {
        ($api.nvenc.$f.expect(stringify!($f)))($($arg),*)
    };
}

/// A CUDA context and an NVENC session on it.
struct Session {
    api: &'static Api,
    ctx: CUcontext,
    encoder: *mut c_void,
}

impl Session {
    fn open() -> anyhow::Result<Self> {
        let api = api().context("NVENC is not available")?;
        let mut s = Session { api, ctx: ptr::null_mut(), encoder: ptr::null_mut() };
        // SAFETY: plain CUDA and NVENC setup; Drop cleans up what was made.
        unsafe {
            let r = (api.cu_ctx_create)(&mut s.ctx, 0, api.device);
            if r != 0 {
                bail!("cuCtxCreate: {r}");
            }
            let mut params = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
                version: OPEN_SESSION_VER,
                deviceType: NV_ENC_DEVICE_TYPE_CUDA,
                device: s.ctx,
                apiVersion: API_VERSION,
                ..Default::default()
            };
            check(nv!(api, nvEncOpenEncodeSessionEx(&mut params, &mut s.encoder)), "opening an NVENC session")?;
            // Calls come from whichever thread owns the session; each one
            // pushes the context first.
            let mut popped = ptr::null_mut();
            (api.cu_ctx_pop)(&mut popped);
        }
        Ok(s)
    }

    /// Runs `f` with this session's CUDA context current.
    fn with<T>(&self, f: impl FnOnce() -> T) -> T {
        // SAFETY: the context is ours and alive.
        unsafe { (self.api.cu_ctx_push)(self.ctx) };
        let r = f();
        let mut popped = ptr::null_mut();
        // SAFETY: pops what we pushed.
        unsafe { (self.api.cu_ctx_pop)(&mut popped) };
        r
    }

    fn caps(&self, codec: &GUID, cap: NV_ENC_CAPS) -> i32 {
        let mut param = NV_ENC_CAPS_PARAM { version: CAPS_PARAM_VER, capsToQuery: cap, ..Default::default() };
        let mut value = 0;
        // SAFETY: a query on our open session.
        let status = self.with(|| unsafe { nv!(self.api, nvEncGetEncodeCaps(self.encoder, *codec, &mut param, &mut value)) });
        if status == NV_ENC_SUCCESS { value } else { 0 }
    }

    fn codecs(&self) -> Vec<GUID> {
        let mut n = 0;
        // SAFETY: queries on our open session, into buffers of the size given.
        self.with(|| unsafe {
            if nv!(self.api, nvEncGetEncodeGUIDCount(self.encoder, &mut n)) != NV_ENC_SUCCESS {
                return Vec::new();
            }
            let mut guids = vec![GUID::default(); n as usize];
            let mut got = 0;
            if nv!(self.api, nvEncGetEncodeGUIDs(self.encoder, guids.as_mut_ptr(), n, &mut got)) != NV_ENC_SUCCESS {
                return Vec::new();
            }
            guids.truncate(got as usize);
            guids
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: each is null or ours.
        unsafe {
            if !self.encoder.is_null() {
                let encoder = self.encoder;
                self.with(|| nv!(self.api, nvEncDestroyEncoder(encoder)));
            }
            if !self.ctx.is_null() {
                (self.api.cu_ctx_destroy)(self.ctx);
            }
        }
    }
}

/// What NVENC can encode on this machine.
pub fn probe() -> Vec<EncoderCaps> {
    let session = match Session::open() {
        Ok(s) => s,
        Err(err) => {
            tracing::info!("no NVENC: {err:#}");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for g in session.codecs() {
        let codec = [Codec::H264, Codec::Hevc, Codec::Av1].into_iter().find(|c| same(&codec_guid(*c), &g));
        let Some(codec) = codec else { continue };
        let max_width = session.caps(&g, NV_ENC_CAPS_WIDTH_MAX).max(0) as u32;
        let max_height = session.caps(&g, NV_ENC_CAPS_HEIGHT_MAX).max(0) as u32;
        let mut chromas = vec![Chroma::Yuv420];
        if codec != Codec::Av1 && session.caps(&g, NV_ENC_CAPS_SUPPORT_YUV444_ENCODE) != 0 {
            chromas.push(Chroma::Yuv444);
        }
        for chroma in chromas {
            out.push(EncoderCaps {
                format: Format { codec, chroma, bit_depth: 8 },
                max_width,
                max_height,
                hardware: true,
            });
        }
    }
    out
}

pub fn plane_layout(format: Format) -> PlaneLayout {
    match format.chroma {
        Chroma::Yuv420 => PlaneLayout::Nv12,
        Chroma::Yuv444 => PlaneLayout::Yuv444,
    }
}

pub struct Nvenc {
    session: Session,
    input: NV_ENC_INPUT_PTR,
    output: NV_ENC_OUTPUT_PTR,
    buffer_format: NV_ENC_BUFFER_FORMAT,
    width: u32,
    height: u32,
    /// `init` points at it.
    config: Box<NV_ENC_CONFIG>,
    /// Reference frame invalidation works: the driver has it, and more
    /// than one reference frame.
    rfi: bool,
    /// The ordinary and refinement QPs, on this codec's scale, the most it
    /// takes, and the one set now.
    qp: u32,
    refine_qp: u32,
    max_qp: u32,
    current_qp: u32,
    init: Box<NV_ENC_INITIALIZE_PARAMS>,
}

// SAFETY: the session and its buffers are used by one thread at a time,
// with the CUDA context pushed around each call.
unsafe impl Send for Nvenc {}

impl Nvenc {
    pub fn open(format: Format, width: i32, height: i32, settings: &Settings) -> anyhow::Result<Self> {
        let session = Session::open()?;
        let api = session.api;
        let codec = codec_guid(format.codec);
        let (width, height) = (width as u32, height as u32);
        let buffer_format = match format.chroma {
            Chroma::Yuv420 => NV_ENC_BUFFER_FORMAT_NV12,
            Chroma::Yuv444 => NV_ENC_BUFFER_FORMAT_YUV444,
        };
        let mut preset = NV_ENC_PRESET_CONFIG {
            version: PRESET_CONFIG_VER,
            presetCfg: NV_ENC_CONFIG { version: CONFIG_VER, ..Default::default() },
            ..Default::default()
        };
        let encoder = session.encoder;
        // SAFETY: a query on our session.
        session.with(|| unsafe {
            check(
                nv!(api, nvEncGetEncodePresetConfigEx(encoder, codec, PRESET_P1, NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY, &mut preset)),
                "reading the P1 preset",
            )
        })?;
        let mut config = Box::new(preset.presetCfg);
        config.version = CONFIG_VER;
        config.gopLength = INFINITE_GOP;
        config.frameIntervalP = 1; // no B-frames
        config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CONSTQP;
        let qp = match format.codec {
            Codec::Av1 => settings.qp * 255 / 51,
            _ => settings.qp,
        };
        config.rcParams.constQP = NV_ENC_QP { qpInterP: qp, qpInterB: qp, qpIntra: qp };
        let chroma_idc = if format.chroma == Chroma::Yuv444 { 3 } else { 1 };
        let invalidation = session.caps(&codec, NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION) != 0;
        let multiple_refs = session.caps(&codec, NV_ENC_CAPS_SUPPORT_MULTIPLE_REF_FRAMES) != 0;
        tracing::debug!(
            invalidation,
            multiple_refs,
            ltr = session.caps(&codec, NV_ENC_CAPS_NUM_MAX_LTR_FRAMES),
            intra_refresh = session.caps(&codec, NV_ENC_CAPS_SUPPORT_INTRA_REFRESH),
            "NVENC loss recovery"
        );
        // A frame references only the one before it, so a GPU without
        // multiple references per frame (Maxwell) still keeps a DPB to fall
        // back on.
        let rfi = format.codec != Codec::Av1 && invalidation;
        // SAFETY: each union member is the one for this codec.
        unsafe {
            match format.codec {
                Codec::H264 => {
                    let h = &mut config.encodeCodecConfig.h264Config;
                    h.idrPeriod = INFINITE_GOP;
                    h.set_repeatSPSPPS(1);
                    h.chromaFormatIDC = chroma_idc;
                    if rfi {
                        h.maxNumRefFrames = DPB;
                        h.numRefL0 = NV_ENC_NUM_REF_FRAMES_1;
                    }
                    config.profileGUID = if chroma_idc == 3 { H264_HIGH_444 } else { H264_HIGH };
                }
                Codec::Hevc => {
                    let h = &mut config.encodeCodecConfig.hevcConfig;
                    h.idrPeriod = INFINITE_GOP;
                    h.set_repeatSPSPPS(1);
                    h.set_chromaFormatIDC(chroma_idc);
                    if rfi {
                        h.maxNumRefFramesInDPB = DPB;
                        h.numRefL0 = NV_ENC_NUM_REF_FRAMES_1;
                    }
                    config.profileGUID = if chroma_idc == 3 { HEVC_FREXT } else { HEVC_MAIN };
                }
                Codec::Av1 => {
                    let a = &mut config.encodeCodecConfig.av1Config;
                    a.idrPeriod = INFINITE_GOP;
                    a.set_repeatSeqHdr(1);
                    config.profileGUID = AV1_MAIN;
                }
            }
        }
        let mut init = Box::new(NV_ENC_INITIALIZE_PARAMS {
            version: INITIALIZE_VER,
            encodeGUID: codec,
            presetGUID: PRESET_P1,
            encodeWidth: width,
            encodeHeight: height,
            darWidth: width,
            darHeight: height,
            frameRateNum: 60,
            frameRateDen: 1,
            enablePTD: 1,
            maxEncodeWidth: width,
            maxEncodeHeight: height,
            tuningInfo: NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
            ..Default::default()
        });
        init.encodeConfig = &mut *config;
        let mut enc = Nvenc {
            session,
            input: ptr::null_mut(),
            output: ptr::null_mut(),
            buffer_format,
            width,
            height,
            config,
            rfi,
            qp,
            max_qp: if format.codec == Codec::Av1 { 255 } else { 51 },
            current_qp: qp,
            refine_qp: match format.codec {
                Codec::Av1 => settings.refine_qp * 255 / 51,
                _ => settings.refine_qp,
            }
            .min(qp),
            init,
        };
        let init_ptr: *mut NV_ENC_INITIALIZE_PARAMS = &mut *enc.init;
        // SAFETY: setup on our session; the buffers are destroyed in Drop.
        enc.session.with(|| unsafe {
            check(nv!(api, nvEncInitializeEncoder(encoder, init_ptr)), "initialising NVENC")?;
            let mut input = NV_ENC_CREATE_INPUT_BUFFER {
                version: CREATE_INPUT_VER,
                width,
                height,
                bufferFmt: buffer_format,
                ..Default::default()
            };
            check(nv!(api, nvEncCreateInputBuffer(encoder, &mut input)), "creating an input buffer")?;
            enc.input = input.inputBuffer;
            let mut output = NV_ENC_CREATE_BITSTREAM_BUFFER { version: CREATE_BITSTREAM_VER, ..Default::default() };
            check(nv!(api, nvEncCreateBitstreamBuffer(encoder, &mut output)), "creating a bitstream buffer")?;
            enc.output = output.bitstreamBuffer;
            anyhow::Ok(())
        })?;
        tracing::info!(width, height, %format, qp, rfi, "encoder ready (NVENC, P1 ultra-low-latency, CQP)");
        Ok(enc)
    }

    /// Changes the constant QP from the next frame on, without an IDR.
    fn set_qp(&mut self, qp: u32) -> anyhow::Result<()> {
        self.config.rcParams.constQP = NV_ENC_QP { qpInterP: qp, qpInterB: qp, qpIntra: qp };
        let mut params = NV_ENC_RECONFIGURE_PARAMS {
            version: RECONFIGURE_VER,
            reInitEncodeParams: *self.init,
            ..Default::default()
        };
        params.reInitEncodeParams.encodeConfig = &mut *self.config;
        let (api, encoder) = (self.session.api, self.session.encoder);
        // SAFETY: a reconfigure of our session with its own parameters.
        self.session.with(|| unsafe { check(nv!(api, nvEncReconfigureEncoder(encoder, &mut params)), "changing the QP") })
    }

    /// Encodes `frame`, frame `number`, `qp_offset` above its QP (on
    /// H.264's scale). The number is NVENC's timestamp for the frame, which
    /// [`Nvenc::invalidate`] names it by.
    pub fn encode(
        &mut self,
        frame: MemFrame,
        number: u32,
        kind: FrameKind,
        qp_offset: u32,
        out: &mut Vec<u8>,
    ) -> anyhow::Result<bool> {
        let base = if kind == FrameKind::Refine { self.refine_qp } else { self.qp };
        let qp = (base + qp_offset * self.max_qp / 51).min(self.max_qp);
        if qp != self.current_qp {
            self.set_qp(qp)?;
            self.current_qp = qp;
        }
        self.encode_frame(frame, number as u64, kind, out)
    }

    /// Reference frame invalidation: frames `from..=to` are no longer
    /// referenced, so the next frame is predicted from `from - 1`, if it is
    /// still in the DPB.
    pub fn invalidate(&mut self, from: u32, to: u32) -> bool {
        let span = to.wrapping_sub(from).wrapping_add(1);
        if !self.rfi || span >= DPB {
            return false;
        }
        let (api, encoder) = (self.session.api, self.session.encoder);
        // SAFETY: invalidating frames of our own session by the timestamps
        // they were encoded with.
        self.session.with(|| unsafe {
            (0..span).all(|i| {
                let ts = from.wrapping_add(i) as u64;
                nv!(api, nvEncInvalidateRefFrames(encoder, ts)) == NV_ENC_SUCCESS
            })
        })
    }

    fn encode_frame(&mut self, frame: MemFrame, timestamp: u64, kind: FrameKind, out: &mut Vec<u8>) -> anyhow::Result<bool> {
        let api = self.session.api;
        let encoder = self.session.encoder;
        let (input, output) = (self.input, self.output);
        let (width, height, buffer_format) = (self.width, self.height, self.buffer_format);
        // SAFETY: the input buffer is locked while written, each row within
        // its pitch; the bitstream is locked while read.
        self.session.with(|| unsafe {
            let mut lock = NV_ENC_LOCK_INPUT_BUFFER { version: LOCK_INPUT_VER, inputBuffer: input, ..Default::default() };
            check(nv!(api, nvEncLockInputBuffer(encoder, &mut lock)), "locking the input buffer")?;
            let pitch = lock.pitch as usize;
            let base = lock.bufferDataPtr as *mut u8;
            let mut offset = 0;
            for (i, plane) in frame.planes.iter().enumerate() {
                let (bytes, rows) = frame.layout.plane_size(i, width as usize, height as usize);
                for row in 0..rows {
                    let src = &plane[row * frame.strides[i]..][..bytes];
                    ptr::copy_nonoverlapping(src.as_ptr(), base.add(offset + row * pitch), bytes);
                }
                offset += rows * pitch;
            }
            check(nv!(api, nvEncUnlockInputBuffer(encoder, input)), "unlocking the input buffer")?;

            let mut pic = NV_ENC_PIC_PARAMS {
                version: PIC_PARAMS_VER,
                inputWidth: width,
                inputHeight: height,
                inputPitch: pitch as u32,
                inputBuffer: input,
                outputBitstream: output,
                bufferFmt: buffer_format,
                pictureStruct: NV_ENC_PIC_STRUCT_FRAME,
                inputTimeStamp: timestamp,
                ..Default::default()
            };
            if kind == FrameKind::Keyframe {
                pic.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
            }
            check(nv!(api, nvEncEncodePicture(encoder, &mut pic)), "encoding")?;
            let mut lock = NV_ENC_LOCK_BITSTREAM { version: LOCK_BITSTREAM_VER, outputBitstream: output, ..Default::default() };
            check(nv!(api, nvEncLockBitstream(encoder, &mut lock)), "locking the bitstream")?;
            out.extend_from_slice(std::slice::from_raw_parts(
                lock.bitstreamBufferPtr as *const u8,
                lock.bitstreamSizeInBytes as usize,
            ));
            let keyframe = lock.pictureType == NV_ENC_PIC_TYPE_IDR || lock.pictureType == NV_ENC_PIC_TYPE_I;
            check(nv!(api, nvEncUnlockBitstream(encoder, output)), "unlocking the bitstream")?;
            Ok(keyframe)
        })
    }
}

impl Drop for Nvenc {
    fn drop(&mut self) {
        let api = self.session.api;
        let encoder = self.session.encoder;
        let (input, output) = (self.input, self.output);
        // SAFETY: the buffers are ours; the session is destroyed after.
        self.session.with(|| unsafe {
            if !input.is_null() {
                nv!(api, nvEncDestroyInputBuffer(encoder, input));
            }
            if !output.is_null() {
                nv!(api, nvEncDestroyBitstreamBuffer(encoder, output));
            }
        });
    }
}
