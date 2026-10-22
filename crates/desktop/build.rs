//! Windows builds embed the logo as the executable's icon.

fn main() {
    let ico = "../../assets/logo/glidedesk.ico";
    println!("cargo:rerun-if-changed={ico}");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon(ico);
        res.compile().expect("compiling the icon resource");
    }
}
