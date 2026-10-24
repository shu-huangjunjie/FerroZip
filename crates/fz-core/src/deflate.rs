//! Raw DEFLATE via the vendored libdeflate: one-shot compression and
//! decompression of whole buffers, plus `compress_chunk`, a FerroZip patch
//! that emits a byte-aligned, non-final piece so pieces compressed on
//! different threads concatenate into one valid stream.

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::NonNull;

unsafe extern "C" {
    fn libdeflate_alloc_compressor(level: i32) -> *mut c_void;
    fn libdeflate_free_compressor(c: *mut c_void);
    fn libdeflate_deflate_compress_bound(c: *mut c_void, in_nbytes: usize) -> usize;
    fn libdeflate_deflate_compress(c: *mut c_void, input: *const c_void, in_nbytes: usize, out: *mut c_void, avail: usize) -> usize;
    fn libdeflate_deflate_compress_chunk(
        c: *mut c_void,
        input: *const c_void,
        in_nbytes: usize,
        out: *mut c_void,
        avail: usize,
        is_final: i32,
    ) -> usize;
    fn libdeflate_alloc_decompressor() -> *mut c_void;
    fn libdeflate_free_decompressor(d: *mut c_void);
    fn libdeflate_deflate_decompress(
        d: *mut c_void,
        input: *const c_void,
        in_nbytes: usize,
        out: *mut c_void,
        avail: usize,
        actual_out: *mut usize,
    ) -> i32;
}

pub struct Compressor(NonNull<c_void>);

// SAFETY: a libdeflate compressor has no thread affinity; it is only ever
// used by one thread at a time through `&mut self`.
unsafe impl Send for Compressor {}

impl Compressor {
    /// `level` is libdeflate's 1..=12.
    pub fn new(level: i32) -> Self {
        let p = unsafe { libdeflate_alloc_compressor(level.clamp(1, 12)) };
        Compressor(NonNull::new(p).expect("libdeflate: out of memory"))
    }

    fn run(&mut self, data: &[u8], chunk: Option<bool>) -> Option<Vec<u8>> {
        // Room for the sync-flush trailer that `compress_chunk` appends.
        let cap = unsafe { libdeflate_deflate_compress_bound(self.0.as_ptr(), data.len()) } + 16;
        let mut out = Vec::<u8>::with_capacity(cap);
        let n = unsafe {
            let (inp, len, dst) = (data.as_ptr().cast(), data.len(), out.as_mut_ptr().cast());
            match chunk {
                None => libdeflate_deflate_compress(self.0.as_ptr(), inp, len, dst, cap),
                Some(last) => libdeflate_deflate_compress_chunk(self.0.as_ptr(), inp, len, dst, cap, i32::from(last)),
            }
        };
        if n == 0 {
            return None;
        }
        // SAFETY: libdeflate initialised the first `n <= cap` bytes.
        unsafe { out.set_len(n) };
        Some(out)
    }

    /// A complete raw DEFLATE stream.
    pub fn compress(&mut self, data: &[u8]) -> Vec<u8> {
        self.run(data, None).expect("compress_bound is always sufficient")
    }

    /// One piece of a stream: with `last == false` the output is byte-aligned
    /// and non-final. `None` if `data` is too short for chunk mode.
    pub fn compress_chunk(&mut self, data: &[u8], last: bool) -> Option<Vec<u8>> {
        self.run(data, Some(last))
    }
}

impl Drop for Compressor {
    fn drop(&mut self) {
        unsafe { libdeflate_free_compressor(self.0.as_ptr()) }
    }
}

pub struct Decompressor(NonNull<c_void>);

// SAFETY: as for `Compressor`.
unsafe impl Send for Decompressor {}

impl Default for Decompressor {
    fn default() -> Self {
        let p = unsafe { libdeflate_alloc_decompressor() };
        Decompressor(NonNull::new(p).expect("libdeflate: out of memory"))
    }
}

impl Decompressor {
    /// Decompress a complete raw DEFLATE stream that must expand to exactly
    /// `size` bytes, into `out` (reused across calls).
    pub fn decompress_exact(&mut self, input: &[u8], size: usize, out: &mut Vec<u8>) -> bool {
        out.clear();
        out.reserve(size);
        let mut actual = 0usize;
        let rc = unsafe {
            libdeflate_deflate_decompress(self.0.as_ptr(), input.as_ptr().cast(), input.len(), out.as_mut_ptr().cast(), size, &mut actual)
        };
        if rc != 0 || actual != size {
            return false;
        }
        // SAFETY: libdeflate wrote exactly `size` bytes.
        unsafe { out.set_len(size) };
        true
    }
}

impl Drop for Decompressor {
    fn drop(&mut self) {
        unsafe { libdeflate_free_decompressor(self.0.as_ptr()) }
    }
}

thread_local! {
    static COMPRESSORS: RefCell<Vec<Option<Compressor>>> = RefCell::new((0..13).map(|_| None).collect());
}

/// Run `f` with this thread's cached compressor for `level` (1..=12).
pub fn with_compressor<R>(level: i32, f: impl FnOnce(&mut Compressor) -> R) -> R {
    let level = level.clamp(1, 12);
    COMPRESSORS.with(|cache| {
        let mut cache = cache.borrow_mut();
        f(cache[level as usize].get_or_insert_with(|| Compressor::new(level)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn sample(n: usize) -> Vec<u8> {
        (0..n).map(|i| ((i * 7919) % 251) as u8 ^ (i / 1000) as u8).collect()
    }

    #[test]
    fn chunks_concatenate_into_one_stream() {
        for level in [1, 6, 9, 12] {
            let data = sample(3_300_000);
            let mut c = Compressor::new(level);
            let pieces: Vec<&[u8]> = data.chunks(1 << 20).collect();
            let mut stream = Vec::new();
            for (i, p) in pieces.iter().enumerate() {
                stream.extend(c.compress_chunk(p, i + 1 == pieces.len()).unwrap());
            }
            let mut back = Vec::new();
            flate2::read::DeflateDecoder::new(&stream[..]).read_to_end(&mut back).unwrap();
            assert_eq!(back, data, "level {level}");
            let mut out = Vec::new();
            assert!(Decompressor::default().decompress_exact(&stream, data.len(), &mut out));
            assert_eq!(out, data);
        }
    }

    #[test]
    fn incompressible_and_tiny() {
        let mut x = 0x1234_5678u32;
        let noise: Vec<u8> = (0..2_000_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        let mut c = Compressor::new(6);
        let mut stream = c.compress_chunk(&noise[..1 << 20], false).unwrap();
        stream.extend(c.compress_chunk(&noise[1 << 20..], true).unwrap());
        let mut out = Vec::new();
        assert!(Decompressor::default().decompress_exact(&stream, noise.len(), &mut out));
        assert_eq!(out, noise);
        assert!(c.compress_chunk(b"tiny", false).is_none());
        let whole = c.compress(b"tiny");
        assert!(Decompressor::default().decompress_exact(&whole, 4, &mut out));
        assert!(!Decompressor::default().decompress_exact(&whole, 5, &mut out));
    }
}
