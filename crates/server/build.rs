//! Bindings for NVENC (`nvenc/nvEncodeAPI.h`). The library is loaded at run
//! time, so nothing is linked.

fn main() {
    println!("cargo:rerun-if-changed=nvenc/nvEncodeAPI.h");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("nvenc.rs");
    bindgen::Builder::default()
        .header("nvenc/nvEncodeAPI.h")
        .allowlist_type("NV_ENC.*|NV_ENCODE.*|_NV_ENC.*|GUID|NVENCSTATUS|PNVENCODEAPICREATEINSTANCE")
        .allowlist_var("NVENCAPI_.*")
        .derive_default(true)
        .layout_tests(false)
        .generate_comments(false)
        .prepend_enum_name(false)
        .generate()
        .expect("generating NVENC bindings")
        .write_to_file(out)
        .expect("writing NVENC bindings");
}
