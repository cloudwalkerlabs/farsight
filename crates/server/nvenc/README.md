`nvEncodeAPI.h` is NVIDIA's NVENC header (MIT, see its top), from
[nv-codec-headers](https://github.com/FFmpeg/nv-codec-headers) tag
`n12.1.14.0`: API 12.1, which drivers from 530 on accept. `build.rs` runs
bindgen over it. Moving to a newer API raises the minimum driver; FFmpeg's
own NVENC needing a newer driver than Maxwell GPUs get is why farsight binds
NVENC itself.
