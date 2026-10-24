use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use crate::create::{CreateFormat, CreateOptions, Source, zstd_level};
use crate::error::{Error, IoContext, Result, from_codec_io};
use crate::extract::{OFFLOAD_MAX, Target, pump, with_writers};
use crate::progress::{Progress, ProgressReader};
use crate::pool::OrderedPool;
use crate::pstream::{ParallelWriter, StreamCodec};
use crate::read::{Archive, Compression, EntryInfo, Kind, normalize_name};
use crate::{resolve_threads, util};

/// POSIX/GNU magic, or a v7 header whose checksum adds up.
pub(crate) fn looks_like_tar(head: &[u8]) -> bool {
    if head.len() < 512 {
        return false;
    }
    if &head[257..262] == b"ustar" {
        return true;
    }
    let stored = std::str::from_utf8(&head[148..156])
        .ok()
        .and_then(|s| u32::from_str_radix(s.trim_matches(|c: char| c == '\0' || c == ' '), 8).ok());
    let sum: u32 = head[..512].iter().enumerate().map(|(i, &b)| if (148..156).contains(&i) { 32 } else { u32::from(b) }).sum();
    stored == Some(sum) && head[0] != 0
}

fn decoder<'a>(path: &Path, c: Compression, threads: usize, progress: Option<&'a Progress>) -> Result<Box<dyn Read + 'a>> {
    let file = File::open(path).at(path)?;
    let raw: Box<dyn io::BufRead + 'a> = match progress {
        Some(p) => Box::new(BufReader::with_capacity(1 << 20, ProgressReader { inner: file, progress: p })),
        None => Box::new(BufReader::with_capacity(1 << 20, file)),
    };
    Ok(match c {
        Compression::None => raw,
        Compression::Gzip => Box::new(flate2::read::MultiGzDecoder::new(raw)),
        Compression::Bzip2 => Box::new(bzip2::read::MultiBzDecoder::new(raw)),
        Compression::Zstd => {
            let mut d = zstd::stream::read::Decoder::with_buffer(raw).map_err(Error::Io)?;
            d.window_log_max(31).map_err(Error::Io)?;
            Box::new(d)
        }
        Compression::Xz => {
            let stream = liblzma::stream::MtStreamBuilder::new()
                .threads(threads.clamp(1, 64) as u32)
                .memlimit_threading(util::available_memory() / 2)
                .memlimit_stop(u64::MAX)
                .decoder()
                .map_err(|e| Error::Other(e.to_string()))?;
            Box::new(liblzma::read::XzDecoder::new_stream(raw, stream))
        }
    })
}

pub(crate) fn compressed_is_tar(path: &Path, c: Compression) -> Result<bool> {
    let mut d = decoder(path, c, 1, None)?;
    let mut head = vec![0u8; 512];
    let mut n = 0;
    while n < 512 {
        match d.read(&mut head[n..]) {
            Ok(0) => break,
            Ok(r) => n += r,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Ok(false),
        }
    }
    Ok(n == 512 && looks_like_tar(&head))
}

fn single_name(path: &Path) -> String {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_string(),
        _ => format!("{name}.out"),
    }
}

pub(crate) fn list(archive: &mut Archive, progress: &Progress) -> Result<()> {
    progress.set_totals(archive.archive_size, 0);
    let mtime = std::fs::metadata(&archive.path).ok().and_then(|m| m.modified().ok());
    match archive.kind {
        Kind::Single(c) => {
            archive.entries = vec![EntryInfo {
                name: single_name(&archive.path),
                size: single_size(&archive.path, c).unwrap_or(0),
                packed: Some(archive.archive_size),
                mtime,
                method: c.label().into(),
                ..Default::default()
            }];
            Ok(())
        }
        Kind::Tar(c) => {
            let d = decoder(&archive.path, c, 1, Some(progress))?;
            let mut ar = tar::Archive::new(d);
            let mut entries = Vec::new();
            for entry in ar.entries().map_err(from_codec_io)? {
                progress.check()?;
                let entry = entry.map_err(from_codec_io)?;
                entries.push(entry_info(&entry, c));
            }
            archive.entries = entries;
            Ok(())
        }
        _ => unreachable!(),
    }
}

fn entry_info<R: Read>(entry: &tar::Entry<R>, c: Compression) -> EntryInfo {
    let h = entry.header();
    let name = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
    let is_dir = h.entry_type().is_dir();
    EntryInfo {
        name: normalize_name(&name),
        is_dir,
        size: if is_dir { 0 } else { h.size().unwrap_or(0) },
        packed: None,
        mtime: h.mtime().ok().map(|s| UNIX_EPOCH + Duration::from_secs(s)),
        crc: None,
        encrypted: false,
        method: if c == Compression::None { "TAR".into() } else { format!("TAR+{}", c.label()) },
    }
}

/// Uncompressed size from the gzip trailer (mod 4 GiB); other formats would
/// need a full decode, so they report 0 until extracted.
fn single_size(path: &Path, c: Compression) -> Option<u64> {
    if c != Compression::Gzip {
        return None;
    }
    use std::io::{Seek, SeekFrom};
    let mut f = File::open(path).ok()?;
    f.seek(SeekFrom::End(-4)).ok()?;
    let mut b = [0u8; 4];
    f.read_exact(&mut b).ok()?;
    Some(u64::from(u32::from_le_bytes(b)))
}

pub(crate) fn extract(archive: &Archive, mask: &[bool], target: &Target, threads: usize, progress: &Arc<Progress>) -> Result<()> {
    match archive.kind {
        Kind::Single(c) => {
            if !mask.first().copied().unwrap_or(false) {
                return Ok(());
            }
            let e = &archive.entries[0];
            let mut d = decoder(&archive.path, c, threads, Some(progress))?;
            let Some(mut sink) = target.open(&e.name)? else { return Ok(()) };
            let mut buf = vec![0u8; 1 << 20];
            pump_quiet(&mut d, &mut sink, &mut buf, progress)?;
            sink.finish(e.mtime)?;
            target.files.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            progress.file_done();
            Ok(())
        }
        Kind::Tar(c) => {
            let d = decoder(&archive.path, c, threads, Some(progress))?;
            let mut ar = tar::Archive::new(d);
            let mut buf = vec![0u8; 512 * 1024];
            with_writers(target, threads, |w| {
                for (i, entry) in ar.entries().map_err(from_codec_io)?.enumerate() {
                    progress.check()?;
                    let mut entry = entry.map_err(from_codec_io)?;
                    if !mask.get(i).copied().unwrap_or(false) {
                        continue;
                    }
                    let e = &archive.entries[i];
                    let kind = entry.header().entry_type();
                    if kind.is_dir() {
                        target.make_dir(&e.name)?;
                        continue;
                    }
                    if !(kind.is_file() || kind == tar::EntryType::Continuous) {
                        // Links, devices and the like have no Windows equivalent here.
                        continue;
                    }
                    progress.set_current(&e.name);
                    if e.size <= OFFLOAD_MAX && !target.is_test() {
                        let mut data = Vec::with_capacity(e.size as usize);
                        entry.read_to_end(&mut data).map_err(from_codec_io)?;
                        w.write(e.name.clone(), data, e.mtime)?;
                    } else if let Some(mut sink) = target.open(&e.name)? {
                        pump_quiet(&mut entry, &mut sink, &mut buf, progress)?;
                        sink.finish(e.mtime)?;
                        target.files.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    progress.file_done();
                }
                Ok(())
            })
        }
        _ => unreachable!(),
    }
}

/// Like `pump`, but without counting bytes (the outer reader already does).
fn pump_quiet(reader: &mut dyn Read, sink: &mut crate::extract::Sink, buf: &mut [u8], progress: &Progress) -> Result<()> {
    let quiet = Progress::new();
    let r = pump(reader, sink, buf, &quiet);
    progress.check()?;
    r.map(|_| ())
}

pub(crate) fn create(tmp: &Path, sources: &[Source], opts: &CreateOptions, progress: &Arc<Progress>) -> Result<()> {
    let threads = resolve_threads(opts.threads);
    let level = opts.level.min(9);
    let file = BufWriter::with_capacity(1 << 20, File::create(tmp).at(tmp)?);
    match opts.format {
        CreateFormat::Tar => {
            write_tar(file, sources, progress)?.flush().at(tmp)?;
        }
        CreateFormat::TarGz => {
            let w = ParallelWriter::new(file, StreamCodec::Gzip { level: crate::create::deflate_level(level.max(1)) }, threads, progress.clone())?;
            write_tar(w, sources, progress)?.finish()?;
        }
        CreateFormat::TarBz2 => {
            let w = ParallelWriter::new(file, StreamCodec::Bzip2 { level: level.max(1) }, threads, progress.clone())?;
            write_tar(w, sources, progress)?.finish()?;
        }
        CreateFormat::TarZst => {
            let mut enc = zstd::Encoder::new(file, zstd_level(level)).map_err(Error::Io)?;
            enc.multithread(threads as u32).map_err(Error::Io)?;
            enc.include_checksum(true).map_err(Error::Io)?;
            write_tar(enc, sources, progress)?.finish().at(tmp)?;
        }
        CreateFormat::TarXz => {
            let stream = liblzma::stream::MtStreamBuilder::new()
                .threads(threads as u32)
                .preset(level)
                .check(liblzma::stream::Check::Crc64)
                .encoder()
                .map_err(|e| Error::Other(e.to_string()))?;
            write_tar(liblzma::write::XzEncoder::new_stream(file, stream), sources, progress)?
                .finish()
                .at(tmp)?;
        }
        CreateFormat::Zip | CreateFormat::SevenZ => unreachable!(),
    }
    Ok(())
}

/// Contents of one source, read ahead on a worker thread.
enum Prefetched {
    Dir,
    Data(Vec<u8>),
    /// Too large to buffer: streamed from disk by the writer.
    Stream,
}

const PREFETCH_MAX: u64 = 4 << 20;

/// Write the tar stream in order while worker threads read the next files,
/// so many small files are not opened and read one at a time.
fn write_tar<W: Write>(w: W, sources: &[Source], progress: &Arc<Progress>) -> Result<W> {
    let mut b = tar::Builder::new(w);
    let mut pool: OrderedPool<(usize, Prefetched)> = OrderedPool::new(8, 64);
    let mut sink = |(i, item): (usize, Prefetched)| -> Result<()> {
        let s = &sources[i];
        let mut h = tar::Header::new_gnu();
        let mtime = s.meta.mtime.and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        h.set_mtime(mtime);
        let name = s.meta.name.trim_end_matches('/');
        let at_path = |e: io::Error| match crate::error::from_codec_io(e) {
            Error::Io(e) => Error::io_path(&s.path, e),
            other => other,
        };
        match item {
            Prefetched::Dir => {
                h.set_entry_type(tar::EntryType::Directory);
                h.set_mode(0o755);
                h.set_size(0);
                return b.append_data(&mut h, format!("{name}/"), io::empty()).map_err(at_path);
            }
            Prefetched::Data(data) => {
                progress.set_current(name);
                h.set_entry_type(tar::EntryType::Regular);
                h.set_mode(0o644);
                h.set_size(data.len() as u64);
                b.append_data(&mut h, name, &data[..]).map_err(at_path)?;
                progress.add_bytes(data.len() as u64);
            }
            Prefetched::Stream => {
                progress.set_current(name);
                let f = File::open(&s.path).at(&s.path)?;
                let len = f.metadata().at(&s.path)?.len();
                h.set_entry_type(tar::EntryType::Regular);
                h.set_mode(0o644);
                h.set_size(len);
                let reader = ProgressReader { inner: BufReader::with_capacity(1 << 20, f).take(len), progress };
                b.append_data(&mut h, name, reader).map_err(at_path)?;
            }
        }
        progress.file_done();
        Ok(())
    };
    for (i, s) in sources.iter().enumerate() {
        progress.check()?;
        let path = s.path.clone();
        let (is_dir, size) = (s.meta.is_dir, s.size);
        pool.push(
            move || {
                Ok((
                    i,
                    if is_dir {
                        Prefetched::Dir
                    } else if size <= PREFETCH_MAX {
                        Prefetched::Data(std::fs::read(&path).at(&path)?)
                    } else {
                        Prefetched::Stream
                    },
                ))
            },
            &mut sink,
        )?;
    }
    pool.finish(&mut sink)?;
    b.into_inner().map_err(Error::Io)
}
