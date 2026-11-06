//! Links Windows system libraries and vcpkg host libraries (libx264, vpl) when building the host on Windows.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
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
