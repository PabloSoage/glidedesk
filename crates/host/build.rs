//! Links Windows system libraries and vcpkg host libraries (libx264, vpl) when building the host on Windows.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let root = manifest_dir.parent().and_then(|p| p.parent()).unwrap_or(&manifest_dir);
        let candidate_dirs = [
            root.join("vcpkg_installed_host").join("x64-windows-static-md").join("lib"),
            root.join("vcpkg_installed").join("x64-windows-static-md").join("lib"),
        ];
        for dir in candidate_dirs {
            if dir.exists() {
                println!("cargo:rustc-link-search=native={}", dir.display());
            }
        }
        if let Ok(ffmpeg_dir) = std::env::var("FFMPEG_DIR") {
            let lib_dir = std::path::Path::new(&ffmpeg_dir).join("lib");
            if lib_dir.exists() {
                println!("cargo:rustc-link-search=native={}", lib_dir.display());
            }
        }

        for lib in [
            "bcrypt",
            "ole32",
            "user32",
            "advapi32",
            "mfuuid",
            "strmiids",
            "dxgi",
            "d3d11",
            "vpl",
            "libx264",
        ] {
            println!("cargo:rustc-link-lib={lib}");
        }
    }
}
