// Generates bindings for the vendored nvEncodeAPI.h (MIT, from FFmpeg's nv-codec-headers
// n13.0.19.0). The library itself, libnvidia-encode.so.1, is loaded at run time.
fn main() {
    println!("cargo:rerun-if-changed=include/nvEncodeAPI.h");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    bindgen::Builder::default()
        .header("include/nvEncodeAPI.h")
        .allowlist_type("NV_ENC.*|NV_ENCODE.*|NVENCSTATUS|GUID|_NV_ENC.*|PNVENCODE.*|NV_ENC_.*")
        .allowlist_var("NVENC.*|NV_ENC.*|NV_MAX.*")
        .allowlist_function("NvEncodeAPI.*")
        .blocklist_item("NV_ENC_.*_GUID|NVENCAPI_VERSION")
        .formatter(bindgen::Formatter::Prettyplease)
        .default_enum_style(bindgen::EnumVariation::Consts)
        .prepend_enum_name(false)
        .derive_default(true)
        .layout_tests(false)
        .generate()
        .expect("generating NVENC bindings")
        .write_to_file(out.join("bindings.rs"))
        .expect("writing NVENC bindings");
}
