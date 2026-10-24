//! Packs the release FerroZip.exe and fz.exe into the installer (zstd), and
//! embeds the icon and a manifest. The payload comes from `FZ_PAYLOAD_DIR`,
//! by default the workspace's target\release (see package.ps1).

#[path = "../fz-gui/src/icon_pixels.rs"]
mod icon_pixels;

use std::path::PathBuf;

const FILES: [&str; 2] = ["FerroZip.exe", "fz.exe"];

const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <!-- Per-user install: never ask for administrator rights. -->
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
</assembly>
"#;

fn main() {
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    println!("cargo:rerun-if-env-changed=FZ_PAYLOAD_DIR");
    let payload_dir = std::env::var_os("FZ_PAYLOAD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join("../../target/release"));

    // name_len u16 | name | size u64 | bytes, for each file; zstd-compressed.
    let mut raw = Vec::new();
    let mut complete = true;
    for name in FILES {
        let path = payload_dir.join(name);
        println!("cargo:rerun-if-changed={}", path.display());
        match std::fs::read(&path) {
            Ok(data) => {
                raw.extend_from_slice(&(name.len() as u16).to_le_bytes());
                raw.extend_from_slice(name.as_bytes());
                raw.extend_from_slice(&(data.len() as u64).to_le_bytes());
                raw.extend_from_slice(&data);
            }
            Err(_) => complete = false,
        }
    }
    if !complete {
        println!("cargo:warning=installer payload missing in {}: build fz-gui and fz-cli in release first (package.ps1)", payload_dir.display());
        raw.clear();
    }
    let mut enc = zstd::Encoder::new(Vec::new(), 19).unwrap();
    enc.include_checksum(true).unwrap();
    std::io::Write::write_all(&mut enc, &raw).unwrap();
    std::fs::write(out.join("payload.zst"), enc.finish().unwrap()).unwrap();

    println!("cargo:rerun-if-changed=../fz-gui/src/icon_pixels.rs");
    let ico = out.join("setup.ico");
    icon_pixels::write_ico(&ico, &[16, 24, 32, 48, 64, 128, 256]).expect("write icon");
    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico.to_str().unwrap());
    res.set("FileDescription", "FerroZip 安装程序");
    res.set("ProductName", "FerroZip");
    res.set("LegalCopyright", "MIT License");
    res.set_manifest(MANIFEST);
    if let Err(e) = res.compile() {
        println!("cargo:warning=resources not embedded: {e}");
    }
}
