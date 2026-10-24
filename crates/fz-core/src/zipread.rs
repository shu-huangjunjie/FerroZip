use std::fs::File;
use std::io::{Cursor, Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use memmap2::Mmap;
use rayon::prelude::*;
use zip::{CompressionMethod, ZipArchive};

use crate::codepage;
use crate::deflate::Decompressor;
use crate::error::{Error, IoContext, Result};
use crate::extract::{Target, pump};
use crate::progress::Progress;
use crate::read::{Archive, EntryInfo, normalize_name};
use crate::util::from_local_parts;

/// The archive mapped into memory: cheap to clone, so every extraction thread
/// gets its own reader without re-parsing the central directory.
#[derive(Clone)]
pub(crate) struct MapBytes(Arc<Mmap>);

impl AsRef<[u8]> for MapBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

pub(crate) type MapZip = ZipArchive<Cursor<MapBytes>>;

pub(crate) fn open_zip(path: &Path) -> Result<MapZip> {
    Ok(open_zip_mapped(path)?.1)
}

/// The mapping itself plus a reader over it.
pub(crate) fn open_zip_mapped(path: &Path) -> Result<(MapBytes, MapZip)> {
    let file = File::open(path).at(path)?;
    if file.metadata().at(path)?.len() == 0 {
        return Err(Error::Corrupt("空文件".into()));
    }
    // SAFETY: the archive is opened read-only; concurrent modification by
    // another process is the same hazard every mmap-based reader accepts.
    let map = MapBytes(Arc::new(unsafe { Mmap::map(&file) }.at(path)?));
    let zip = ZipArchive::new(Cursor::new(map.clone()))?;
    Ok((map, zip))
}

fn method_name(m: CompressionMethod) -> String {
    match m {
        CompressionMethod::Stored => "Store".into(),
        CompressionMethod::Deflated => "Deflate".into(),
        CompressionMethod::Deflate64 => "Deflate64".into(),
        CompressionMethod::Bzip2 => "BZip2".into(),
        CompressionMethod::Zstd => "Zstd".into(),
        CompressionMethod::Lzma => "LZMA".into(),
        CompressionMethod::Xz => "XZ".into(),
        other => format!("{other:?}"),
    }
}

pub(crate) fn list(archive: &mut Archive) -> Result<()> {
    let zip = open_zip(&archive.path)?;
    archive.comment = String::from_utf8_lossy(zip.comment()).into_owned();
    let mut entries = Vec::with_capacity(zip.len());
    for i in 0..zip.len() {
        let f = zip.by_index_data(i)?;
        let raw = f.name_raw();
        let mut name = codepage::decode(raw, archive.codepage);
        if raw.is_ascii() {
            // Honour Info-ZIP Unicode path extra fields when present.
            if let Ok(n) = f.name() {
                name = n.into_owned();
            }
        }
        let is_dir = f.is_dir() || name.ends_with('/') || name.ends_with('\\');
        let mtime = f.last_modified().and_then(|d| {
            from_local_parts(i32::from(d.year()), d.month(), d.day(), d.hour(), d.minute(), d.second())
        });
        let method = method_name(f.compression());
        entries.push(EntryInfo {
            name: normalize_name(&name),
            is_dir,
            size: f.size(),
            packed: Some(f.compressed_size()),
            mtime,
            // AE-2 encrypted entries store 0 instead of a CRC.
            crc: (!is_dir && !(f.encrypted() && f.crc32() == 0)).then(|| f.crc32()),
            encrypted: f.encrypted(),
            method,
        });
    }
    archive.entries = entries;
    Ok(())
}

/// Entries up to this size are decoded in one shot instead of streamed.
const ONE_SHOT_MAX: u64 = 64 << 20;

/// Where an entry's data lives inside the mapped archive, from its local
/// header. `None` if the header looks wrong (the slow path then handles it).
fn data_range(map: &[u8], header_start: u64, compressed: u64) -> Option<std::ops::Range<usize>> {
    let h = usize::try_from(header_start).ok()?;
    let hdr = map.get(h..h.checked_add(30)?)?;
    if hdr[..4] != *b"PK" {
        return None;
    }
    let name_len = usize::from(u16::from_le_bytes([hdr[26], hdr[27]]));
    let extra_len = usize::from(u16::from_le_bytes([hdr[28], hdr[29]]));
    let start = h + 30 + name_len + extra_len;
    let end = start.checked_add(usize::try_from(compressed).ok()?)?;
    (end <= map.len()).then_some(start..end)
}

pub(crate) fn extract(
    archive: &Archive,
    mask: &[bool],
    target: &Target,
    password: Option<&str>,
    threads: usize,
    progress: &Arc<Progress>,
) -> Result<()> {
    let (map, zip) = open_zip_mapped(&archive.path)?;
    if zip.len() != archive.entries.len() {
        return Err(Error::Corrupt("压缩文件在打开后被修改".into()));
    }
    // Archive order: each thread works through its own run of neighbouring
    // entries, which keeps file creation local to a few folders at a time
    // (measured ~1 s faster than largest-first on 45k files). Folders are
    // created by the workers as they go, overlapping with file writes.
    let items: Vec<usize> = (0..archive.entries.len()).filter(|&i| mask[i]).collect();
    if password.is_none() && items.iter().any(|&i| archive.entries[i].encrypted) {
        return Err(Error::PasswordRequired);
    }
    let map: &[u8] = map.as_ref();

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .map_err(|e| Error::Other(e.to_string()))?;
    pool.install(|| {
        items.par_iter().try_for_each_init(
            || (zip.clone(), Decompressor::default(), Vec::new(), vec![0u8; 512 * 1024]),
            |(zip, dec, out, buf), &i| -> Result<()> {
                progress.check()?;
                let e = &archive.entries[i];
                if e.is_dir {
                    return target.make_dir(&e.name);
                }
                progress.set_current(&e.name);
                let (method, encrypted, compressed, size, crc, header) = {
                    let m = zip.by_index_data(i)?;
                    (m.compression(), m.encrypted(), m.compressed_size(), m.size(), m.crc32(), m.header_start())
                };

                // Fast path: decode straight from the mapped archive with
                // libdeflate, verify the CRC, and write the file in one call.
                let one_shot = !encrypted
                    && size <= ONE_SHOT_MAX
                    && matches!(method, CompressionMethod::Stored | CompressionMethod::Deflated);
                if let Some(range) = one_shot.then(|| data_range(map, header, compressed)).flatten() {
                    let Some(mut sink) = target.open_sized(&e.name, size)? else {
                        progress.add_bytes(size);
                        progress.file_done();
                        return Ok(());
                    };
                    let raw = &map[range];
                    let data: &[u8] = if method == CompressionMethod::Stored {
                        raw
                    } else if dec.decompress_exact(raw, size as usize, out) {
                        out
                    } else {
                        return Err(Error::Corrupt(format!("数据错误: {}", e.name)));
                    };
                    if data.len() as u64 != size || crc32fast::hash(data) != crc {
                        return Err(Error::Corrupt(format!("CRC 校验失败: {}", e.name)));
                    }
                    sink.write_all(data).map_err(|err| sink.err(err))?;
                    sink.finish(e.mtime)?;
                    progress.add_bytes(size);
                    target.files.fetch_add(1, Relaxed);
                    progress.file_done();
                    return Ok(());
                }

                // General path: encryption, other methods, very large entries.
                let Some(mut sink) = target.open_sized(&e.name, size)? else {
                    progress.add_bytes(size);
                    progress.file_done();
                    return Ok(());
                };
                let result = (|| -> Result<()> {
                    let mut reader = match (encrypted, password) {
                        (true, Some(pw)) => zip.by_index_decrypt(i, pw.as_bytes())?,
                        (true, None) => return Err(Error::PasswordRequired),
                        (false, _) => zip.by_index(i)?,
                    };
                    pump(&mut reader as &mut dyn Read, &mut sink, buf, progress)?;
                    Ok(())
                })();
                match result {
                    // A ZipCrypto password that slips past the 1-in-256 header
                    // check shows up as a checksum failure.
                    Err(Error::Corrupt(_)) if encrypted => return Err(Error::WrongPassword),
                    Err(err) => return Err(err),
                    Ok(()) => {}
                }
                sink.finish(e.mtime)?;
                target.files.fetch_add(1, Relaxed);
                progress.file_done();
                Ok(())
            },
        )
    })
}
