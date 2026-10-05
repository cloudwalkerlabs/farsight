//! Compiles the stand-ins that load libva, libva-drm and libdrm at runtime
//! (`src/dlopen.c`). Whole, since the static FFmpeg's calls need them too,
//! and the linker may reach FFmpeg after this crate.

fn main() {
    println!("cargo:rerun-if-changed=src/dlopen.c");
    let mut build = cc::Build::new();
    for lib in ["libva", "libdrm"] {
        let lib = pkg_config::Config::new()
            .cargo_metadata(false)
            .probe(lib)
            .unwrap_or_else(|e| panic!("{lib}'s headers: {e}"));
        build.includes(lib.include_paths);
    }
    build
        .file("src/dlopen.c")
        .link_lib_modifier("+whole-archive")
        .compile("farsight_va_dlopen");
}
