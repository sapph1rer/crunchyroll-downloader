// Same logo as the main app, on the helper's .exe and its window's taskbar button.
fn main() {
    println!("cargo:rerun-if-changed=../src/gui/assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut r = winresource::WindowsResource::new();
        r.set_icon("../src/gui/assets/icon.ico");
        if let Err(e) = r.compile() {
            println!("cargo:warning=exe icon not embedded: {e}");
        }
    }
}
