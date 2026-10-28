//! Windows builds embed the logo as the executable's icon, and link the system libraries the
//! static FFmpeg needs (with `FFMPEG_DIR`, ffmpeg-sys-next only links FFmpeg itself).

fn main() {
    let ico = "../../assets/logo/glidedesk.ico";
    println!("cargo:rerun-if-changed={ico}");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon(ico);
        res.compile().expect("compiling the icon resource");
        for lib in [
            "bcrypt", "ole32", "user32", "advapi32", "mfuuid", "strmiids",
        ] {
            println!("cargo:rustc-link-lib={lib}");
        }
    }
}
