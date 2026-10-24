//! Builds the vendored libdeflate (with FerroZip's chunked-compression patch,
//! see vendor/libdeflate/lib/deflate_compress.c).

fn main() {
    let dir = "vendor/libdeflate";
    println!("cargo:rerun-if-changed={dir}");
    let lib = |f: &str| format!("{dir}/lib/{f}");
    cc::Build::new()
        .files([
            lib("x86/cpu_features.c"),
            lib("arm/cpu_features.c"),
            lib("utils.c"),
            lib("crc32.c"),
            lib("deflate_compress.c"),
            lib("deflate_decompress.c"),
        ])
        .include(dir)
        .warnings(false)
        .compile("fzdeflate");
}
