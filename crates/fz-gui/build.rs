//! Embeds the application icon (generated from src/icon_pixels.rs) and
//! version info into FerroZip.exe so Explorer shows them.

#[path = "src/icon_pixels.rs"]
mod icon_pixels;

fn main() {
    println!("cargo:rerun-if-changed=src/icon_pixels.rs");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico = out.join("ferrozip.ico");
    icon_pixels::write_ico(&ico, &[16, 24, 32, 48, 64, 128, 256]).expect("write icon");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon(ico.to_str().unwrap());
        res.set("FileDescription", "FerroZip");
        res.set("ProductName", "FerroZip");
        res.set("LegalCopyright", "MIT License");
        if let Err(e) = res.compile() {
            println!("cargo:warning=icon resource not embedded: {e}");
        }
    }
}
