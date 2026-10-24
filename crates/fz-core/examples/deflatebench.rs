//! Single-thread Deflate comparison on real data: zlib-rs (via flate2) vs
//! libdeflate, compression speed/ratio per level and decompression speed.
//!
//!   cargo run --release -p fz-core --example deflatebench -- <dir> [big file]

use std::path::Path;
use std::time::Instant;

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress};

fn load(root: &Path, out: &mut Vec<Vec<u8>>) {
    for e in std::fs::read_dir(root).unwrap() {
        let e = e.unwrap();
        let p = e.path();
        if e.file_type().unwrap().is_dir() {
            load(&p, out);
        } else if e.metadata().unwrap().len() <= 1 << 20 {
            out.push(std::fs::read(&p).unwrap());
        }
    }
}

fn zlib_compress(level: u32, dict: &[u8], data: &[u8], last: bool, out: &mut Vec<u8>) {
    out.clear();
    out.reserve(data.len() + data.len() / 100 + 64);
    let mut c = Compress::new(Compression::new(level), false);
    if !dict.is_empty() {
        c.set_dictionary(dict).unwrap();
    }
    let flush = if last { FlushCompress::Finish } else { FlushCompress::Sync };
    loop {
        let consumed = c.total_in() as usize;
        let st = c.compress_vec(&data[consumed..], out, flush).unwrap();
        if (last && st == flate2::Status::StreamEnd) || (!last && c.total_in() as usize == data.len() && out.len() < out.capacity()) {
            break;
        }
        out.reserve(out.capacity());
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut files = Vec::new();
    load(Path::new(&args[1]), &mut files);
    let total: usize = files.iter().map(|f| f.len()).sum();
    println!("{} files <= 1 MiB, {:.1} MB", files.len(), total as f64 / 1e6);

    let mut out = Vec::new();
    println!("{:<16} {:>9} {:>10} {:>8}", "codec", "MB/s", "size MB", "ratio");
    for level in [1u32, 3, 5, 6, 7, 9] {
        let t = Instant::now();
        let mut size = 0;
        for f in &files {
            zlib_compress(level, &[], f, true, &mut out);
            size += out.len();
        }
        let s = t.elapsed().as_secs_f64();
        println!("{:<16} {:>9.1} {:>10.2} {:>7.2}%", format!("zlib-rs {level}"), total as f64 / 1e6 / s, size as f64 / 1e6, size as f64 * 100.0 / total as f64);
    }
    let mut packed_ld6 = Vec::new();
    for level in [1i32, 3, 4, 5, 6, 7, 8, 9, 10, 12] {
        let mut c = fz_core::deflate::Compressor::new(level);
        let t = Instant::now();
        let mut size = 0;
        for f in &files {
            let packed = c.compress(f);
            size += packed.len();
            if level == 6 {
                packed_ld6.push(packed);
            }
        }
        let s = t.elapsed().as_secs_f64();
        println!("{:<16} {:>9.1} {:>10.2} {:>7.2}%", format!("libdeflate {level}"), total as f64 / 1e6 / s, size as f64 / 1e6, size as f64 * 100.0 / total as f64);
    }

    // Decompression of the libdeflate-6 output.
    let t = Instant::now();
    for (p, f) in packed_ld6.iter().zip(&files) {
        let mut d = Decompress::new(false);
        out.clear();
        out.reserve(f.len());
        d.decompress_vec(p, &mut out, FlushDecompress::Finish).unwrap();
        assert_eq!(out.len(), f.len());
    }
    println!("inflate zlib-rs     {:>9.1} MB/s", total as f64 / 1e6 / t.elapsed().as_secs_f64());
    let mut d = fz_core::deflate::Decompressor::default();
    let t = Instant::now();
    for (p, f) in packed_ld6.iter().zip(&files) {
        assert!(d.decompress_exact(p, f.len(), &mut out));
    }
    println!("inflate libdeflate  {:>9.1} MB/s", total as f64 / 1e6 / t.elapsed().as_secs_f64());
    let t = Instant::now();
    let mut x = 0u32;
    for f in &files {
        x ^= crc32fast::hash(f);
    }
    println!("crc32fast           {:>9.1} MB/s ({x})", total as f64 / 1e6 / t.elapsed().as_secs_f64());

    if let Some(big) = args.get(2) {
        let data = std::fs::read(big).unwrap();
        let data = &data[..data.len().min(256 << 20)];
        let chunk = 1 << 20;
        for level in [5u32, 6] {
            let t = Instant::now();
            let mut size = 0;
            let mut dict: &[u8] = &[];
            let n = data.len().div_ceil(chunk);
            for (i, c) in data.chunks(chunk).enumerate() {
                zlib_compress(level, dict, c, i + 1 == n, &mut out);
                size += out.len();
                dict = &c[c.len() - 32768..];
            }
            println!("big zlib-rs {level} 1M+dict {:>8.1} MB/s {:>8.2}%", data.len() as f64 / 1e6 / t.elapsed().as_secs_f64(), size as f64 * 100.0 / data.len() as f64);
        }
        for level in [5i32, 6] {
            let mut c = fz_core::deflate::Compressor::new(level);
            for chunk in [1usize << 20, 4 << 20] {
                let t = Instant::now();
                let mut size = 0;
                let n = data.len().div_ceil(chunk);
                for (i, ch) in data.chunks(chunk).enumerate() {
                    size += c.compress_chunk(ch, i + 1 == n).unwrap().len();
                }
                println!("big libdeflate {level} {}M nodict {:>6.1} MB/s {:>8.2}%", chunk >> 20, data.len() as f64 / 1e6 / t.elapsed().as_secs_f64(), size as f64 * 100.0 / data.len() as f64);
            }
        }
    }
}
