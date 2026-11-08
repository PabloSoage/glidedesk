//! Windows: FFmpeg comes linked in from vcpkg (`scripts/build-windows.ps1 --host`). The
//! static FFmpeg needs the libraries of its encoders (oneVPL for Quick Sync, x264) and some
//! system libraries; those are linked here, the encoder ones only when vcpkg built them.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    let Some(lib_dir) =
        std::env::var_os("FFMPEG_DIR").map(|d| std::path::PathBuf::from(d).join("lib"))
    else {
        return;
    };
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    for lib in ["vpl", "libx264"] {
        if lib_dir.join(format!("{lib}.lib")).exists() {
            println!("cargo:rustc-link-lib={lib}");
        }
    }
    for lib in [
        "bcrypt", "ole32", "user32", "advapi32", "mfuuid", "strmiids", "dxgi", "d3d11",
    ] {
        println!("cargo:rustc-link-lib={lib}");
    }
}
