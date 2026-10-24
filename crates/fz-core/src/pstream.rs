//! Chunk-parallel compression primitives.
//!
//! Deflate: like pigz, each chunk is compressed independently and ends on a
//! byte-aligned sync-flush boundary, so the pieces concatenate into one valid
//! deflate stream. libdeflate does the work (via a small patch, see
//! `deflate.rs`); it compresses 1 MiB pieces without a dictionary smaller
//! than zlib does with one. bzip2: like lbzip2, each chunk becomes its own
//! stream and the streams are concatenated (every bzip2 decoder accepts that).

use std::io::{self, Write};
use std::sync::Arc;

use flate2::{Compress, Compression, FlushCompress, Status};

use crate::error::{Error, Result, to_io};
use crate::pool::OrderedPool;
use crate::progress::Progress;

/// One piece of a raw deflate stream compressed with libdeflate at `level`
/// (1..=12). `last` terminates the stream.
pub fn deflate_piece(level: i32, data: &[u8], last: bool) -> Result<Vec<u8>> {
    match crate::deflate::with_compressor(level, |c| c.compress_chunk(data, last)) {
        Some(v) => Ok(v),
        // Only a very short non-final piece (a file shrinking while being
        // read) can get here; zlib handles any length.
        None => deflate_chunk(6, &[], data, last),
    }
}

/// A complete raw deflate stream for an in-memory buffer.
pub fn deflate_whole(level: i32, data: &[u8]) -> Vec<u8> {
    crate::deflate::with_compressor(level, |c| c.compress(data))
}

/// zlib-based piece of a raw deflate stream, optionally primed with `dict`.
pub fn deflate_chunk(level: u32, dict: &[u8], data: &[u8], last: bool) -> Result<Vec<u8>> {
    let mut c = Compress::new(Compression::new(level.min(9)), false);
    if !dict.is_empty() {
        c.set_dictionary(dict).map_err(|e| Error::Other(e.to_string()))?;
    }
    let flush = if last { FlushCompress::Finish } else { FlushCompress::Sync };
    let mut out = Vec::with_capacity(data.len() / 2 + 1024);
    loop {
        let consumed = c.total_in() as usize;
        let status = c
            .compress_vec(&data[consumed..], &mut out, flush)
            .map_err(|e| Error::Other(e.to_string()))?;
        let all_in = c.total_in() as usize == data.len();
        let done = if last { status == Status::StreamEnd } else { all_in && out.len() < out.capacity() };
        if done {
            return Ok(out);
        }
        out.reserve(out.capacity().max(64 * 1024));
    }
}

pub fn zstd_frame(level: i32, data: &[u8]) -> Result<Vec<u8>> {
    zstd::bulk::compress(data, level).map_err(Error::Io)
}

pub fn bzip2_stream(level: u32, data: &[u8]) -> Result<Vec<u8>> {
    let mut enc = bzip2::write::BzEncoder::new(Vec::with_capacity(data.len() / 3), bzip2::Compression::new(level.clamp(1, 9)));
    enc.write_all(data)?;
    Ok(enc.finish()?)
}

#[derive(Clone, Copy)]
pub enum StreamCodec {
    /// `level` is libdeflate's 1..=12.
    Gzip { level: i32 },
    Bzip2 { level: u32 },
}

/// A `Write` that compresses into `inner` using all threads.
pub struct ParallelWriter<W: Write> {
    inner: W,
    codec: StreamCodec,
    pool: Option<OrderedPool<Vec<u8>>>,
    chunk: Vec<u8>,
    chunk_size: usize,
    crc: crc32fast::Hasher,
    total_in: u64,
    progress: Arc<Progress>,
}

impl<W: Write> ParallelWriter<W> {
    pub fn new(mut inner: W, codec: StreamCodec, threads: usize, progress: Arc<Progress>) -> io::Result<Self> {
        let chunk_size = match codec {
            StreamCodec::Gzip { .. } => 1 << 20,
            // One full bzip2 block per stream.
            StreamCodec::Bzip2 { level } => level.clamp(1, 9) as usize * 100_000 - 64,
        };
        if let StreamCodec::Gzip { .. } = codec {
            // magic, deflate, no flags, mtime 0, xfl 0, OS = NTFS
            inner.write_all(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 11])?;
        }
        Ok(Self {
            inner,
            codec,
            pool: Some(OrderedPool::new(threads, threads * 3)),
            chunk: Vec::with_capacity(chunk_size),
            chunk_size,
            crc: crc32fast::Hasher::new(),
            total_in: 0,
            progress,
        })
    }

    fn submit(&mut self, last: bool) -> Result<()> {
        let data = std::mem::replace(&mut self.chunk, Vec::with_capacity(self.chunk_size));
        self.crc.update(&data);
        self.total_in += data.len() as u64;
        let codec = self.codec;
        let progress = self.progress.clone();
        let inner = &mut self.inner;
        let mut sink = |buf: Vec<u8>| inner.write_all(&buf).map_err(Error::Io);
        self.pool.as_mut().expect("open").push(
            move || {
                progress.check()?;
                let out = match codec {
                    StreamCodec::Gzip { level } => deflate_piece(level, &data, last)?,
                    StreamCodec::Bzip2 { level } => {
                        if data.is_empty() && !last {
                            Vec::new()
                        } else {
                            bzip2_stream(level, &data)?
                        }
                    }
                };
                Ok(out)
            },
            &mut sink,
        )
    }

    /// Flush the final chunk and trailer; returns the inner writer.
    pub fn finish(mut self) -> Result<W> {
        self.submit(true)?;
        let pool = self.pool.take().expect("open");
        let inner = &mut self.inner;
        pool.finish(&mut |buf: Vec<u8>| inner.write_all(&buf).map_err(Error::Io))?;
        if let StreamCodec::Gzip { .. } = self.codec {
            let crc = std::mem::take(&mut self.crc).finalize();
            self.inner.write_all(&crc.to_le_bytes())?;
            self.inner.write_all(&(self.total_in as u32).to_le_bytes())?;
        }
        self.inner.flush()?;
        Ok(self.inner)
    }
}

impl<W: Write> Write for ParallelWriter<W> {
    fn write(&mut self, mut buf: &[u8]) -> io::Result<usize> {
        let n = buf.len();
        while !buf.is_empty() {
            let room = self.chunk_size - self.chunk.len();
            let take = room.min(buf.len());
            self.chunk.extend_from_slice(&buf[..take]);
            buf = &buf[take..];
            if self.chunk.len() == self.chunk_size {
                self.submit(false).map_err(to_io)?;
            }
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn sample(n: usize) -> Vec<u8> {
        (0..n).map(|i| ((i * 7919) % 251) as u8 ^ (i / 1000) as u8).collect()
    }

    #[test]
    fn chunked_deflate_is_one_stream() {
        let data = sample(3_500_000);
        let mut out = Vec::new();
        let mut dict: &[u8] = &[];
        let chunks: Vec<&[u8]> = data.chunks(1 << 20).collect();
        for (i, c) in chunks.iter().enumerate() {
            out.extend(deflate_chunk(6, dict, c, i + 1 == chunks.len()).unwrap());
            dict = &c[c.len() - 32 * 1024..];
        }
        let mut back = Vec::new();
        flate2::read::DeflateDecoder::new(&out[..]).read_to_end(&mut back).unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn parallel_gzip_and_bzip2() {
        let data = sample(5_000_000);
        for codec in [StreamCodec::Gzip { level: 6 }, StreamCodec::Bzip2 { level: 9 }] {
            let mut w = ParallelWriter::new(Vec::new(), codec, 4, Arc::new(Progress::new())).unwrap();
            for part in data.chunks(77_777) {
                w.write_all(part).unwrap();
            }
            let out = w.finish().unwrap();
            let mut back = Vec::new();
            match codec {
                StreamCodec::Gzip { .. } => {
                    flate2::read::GzDecoder::new(&out[..]).read_to_end(&mut back).unwrap();
                }
                StreamCodec::Bzip2 { .. } => {
                    bzip2::read::MultiBzDecoder::new(&out[..]).read_to_end(&mut back).unwrap();
                }
            }
            assert_eq!(back, data);
        }
    }
}
