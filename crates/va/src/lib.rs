//! What a VA-API device can encode and decode, from libva's own queries
//! (`docs/design.md` §3). FFmpeg opens the codecs; it has no way to list
//! what the driver supports.

use std::ffi::{c_char, c_int, c_void};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;

use farsight_proto::codec::{Chroma, Codec, Format};

type VADisplay = *mut c_void;
type VAStatus = c_int;
type VAMessageCallback = Option<unsafe extern "C" fn(*mut c_void, *const c_char)>;

#[repr(C)]
struct VAConfigAttrib {
    kind: c_int,
    value: u32,
}

#[link(name = "va")]
unsafe extern "C" {
    fn vaInitialize(dpy: VADisplay, major: *mut c_int, minor: *mut c_int) -> VAStatus;
    fn vaTerminate(dpy: VADisplay) -> VAStatus;
    fn vaMaxNumProfiles(dpy: VADisplay) -> c_int;
    fn vaMaxNumEntrypoints(dpy: VADisplay) -> c_int;
    fn vaQueryConfigProfiles(dpy: VADisplay, profiles: *mut c_int, n: *mut c_int) -> VAStatus;
    fn vaQueryConfigEntrypoints(dpy: VADisplay, profile: c_int, entrypoints: *mut c_int, n: *mut c_int) -> VAStatus;
    fn vaGetConfigAttributes(
        dpy: VADisplay,
        profile: c_int,
        entrypoint: c_int,
        attribs: *mut VAConfigAttrib,
        n: c_int,
    ) -> VAStatus;
    fn vaSetErrorCallback(dpy: VADisplay, cb: VAMessageCallback, ctx: *mut c_void) -> VAMessageCallback;
    fn vaSetInfoCallback(dpy: VADisplay, cb: VAMessageCallback, ctx: *mut c_void) -> VAMessageCallback;
}

#[link(name = "va-drm")]
unsafe extern "C" {
    fn vaGetDisplayDRM(fd: c_int) -> VADisplay;
}

const ENTRYPOINT_VLD: c_int = 1;
const ENTRYPOINT_ENC_SLICE: c_int = 6;
const ENTRYPOINT_ENC_SLICE_LP: c_int = 8;
const ATTRIB_RT_FORMAT: c_int = 0;
const ATTRIB_MAX_WIDTH: c_int = 18;
const ATTRIB_MAX_HEIGHT: c_int = 19;
const ATTRIB_NOT_SUPPORTED: u32 = 0x8000_0000;
const RT_FORMAT_YUV420: u32 = 0x1;
const RT_FORMAT_YUV444: u32 = 0x4;

/// The 8-bit VA profiles for each format, best first. H.264 4:4:4 has no VA
/// profile.
fn profiles(format: Format) -> &'static [c_int] {
    match (format.codec, format.chroma, format.bit_depth) {
        // High, Main, Constrained Baseline.
        (Codec::H264, Chroma::Yuv420, 8) => &[7, 6, 13],
        (Codec::Hevc, Chroma::Yuv420, 8) => &[17],
        (Codec::Hevc, Chroma::Yuv444, 8) => &[26],
        (Codec::Av1, Chroma::Yuv420, 8) => &[32],
        (Codec::Av1, Chroma::Yuv444, 8) => &[33],
        _ => &[],
    }
}

pub use farsight_proto::codec::FORMATS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Encode,
    Decode,
}

/// A format the device handles, and its largest picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Support {
    pub format: Format,
    /// The VA profile, as in `VAProfile`.
    pub profile: i32,
    /// Encoding only: the low-power entrypoint (`EncSliceLP`) is the only
    /// one, so FFmpeg must be told to use it.
    pub low_power: bool,
    pub max_width: u32,
    pub max_height: u32,
}

/// What the device at `render_node` supports in `direction`. Empty if it
/// has no VA-API driver.
pub fn query(render_node: &Path, direction: Direction) -> Vec<Support> {
    let Ok(file) = File::options().read(true).write(true).open(render_node) else {
        return Vec::new();
    };
    // SAFETY: the display lives within this block and the file outlives it;
    // every buffer passed is sized as libva asks.
    unsafe {
        let dpy = vaGetDisplayDRM(file.as_raw_fd());
        if dpy.is_null() {
            return Vec::new();
        }
        // libva prints its driver's name on stderr otherwise.
        vaSetInfoCallback(dpy, None, std::ptr::null_mut());
        vaSetErrorCallback(dpy, None, std::ptr::null_mut());
        let (mut major, mut minor) = (0, 0);
        if vaInitialize(dpy, &mut major, &mut minor) != 0 {
            return Vec::new();
        }
        let out = query_display(dpy, direction);
        vaTerminate(dpy);
        out
    }
}

unsafe fn query_display(dpy: VADisplay, direction: Direction) -> Vec<Support> {
    // SAFETY: the caller passes an initialised display.
    unsafe {
        let mut n = vaMaxNumProfiles(dpy).max(0);
        let mut have = vec![0; n as usize];
        if vaQueryConfigProfiles(dpy, have.as_mut_ptr(), &mut n) != 0 {
            return Vec::new();
        }
        have.truncate(n as usize);
        let max_entry = vaMaxNumEntrypoints(dpy).max(0) as usize;
        let mut out = Vec::new();
        for format in FORMATS {
            for &profile in profiles(format).iter().filter(|p| have.contains(p)) {
                let mut entries = vec![0; max_entry];
                let mut n = 0;
                if vaQueryConfigEntrypoints(dpy, profile, entries.as_mut_ptr(), &mut n) != 0 {
                    continue;
                }
                entries.truncate(n as usize);
                let (entry, low_power) = match direction {
                    Direction::Decode if entries.contains(&ENTRYPOINT_VLD) => (ENTRYPOINT_VLD, false),
                    Direction::Encode if entries.contains(&ENTRYPOINT_ENC_SLICE) => (ENTRYPOINT_ENC_SLICE, false),
                    Direction::Encode if entries.contains(&ENTRYPOINT_ENC_SLICE_LP) => (ENTRYPOINT_ENC_SLICE_LP, true),
                    _ => continue,
                };
                let mut attribs = [ATTRIB_RT_FORMAT, ATTRIB_MAX_WIDTH, ATTRIB_MAX_HEIGHT]
                    .map(|kind| VAConfigAttrib { kind, value: 0 });
                if vaGetConfigAttributes(dpy, profile, entry, attribs.as_mut_ptr(), attribs.len() as c_int) != 0 {
                    continue;
                }
                let rt = match format.chroma {
                    Chroma::Yuv420 => RT_FORMAT_YUV420,
                    Chroma::Yuv444 => RT_FORMAT_YUV444,
                };
                if attribs[0].value == ATTRIB_NOT_SUPPORTED || attribs[0].value & rt == 0 {
                    continue;
                }
                // Drivers that don't say are assumed to manage 4K.
                let limit = |a: &VAConfigAttrib| if a.value == ATTRIB_NOT_SUPPORTED || a.value == 0 { 4096 } else { a.value };
                out.push(Support {
                    format,
                    profile,
                    low_power,
                    max_width: limit(&attribs[1]),
                    max_height: limit(&attribs[2]),
                });
                break;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Prints what this machine's first render node supports; passes
    /// without one.
    #[test]
    fn query_does_not_crash() {
        for dir in [Direction::Encode, Direction::Decode] {
            let s = query(Path::new("/dev/dri/renderD128"), dir);
            eprintln!("{dir:?}: {s:?}");
        }
        assert!(query(Path::new("/nonexistent"), Direction::Decode).is_empty());
    }
}
