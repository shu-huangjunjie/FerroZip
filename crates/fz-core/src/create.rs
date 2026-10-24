use std::fs::File;
use std::os::windows::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{Error, IoContext, Result};
use crate::pool::OrderedPool;
use crate::progress::Progress;
use crate::scan::scan_inputs;
use crate::pstream::{deflate_piece, deflate_whole, zstd_frame};
use crate::winzip_aes::AesEncryptor;
use crate::zipwrite::{EntryMeta, METHOD_DEFLATE, METHOD_STORE, METHOD_ZSTD, ZipOut};
use crate::{resolve_threads, sevenz_write, tarfmt, util};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateFormat {
    Zip,
    SevenZ,
    Tar,
    TarGz,
    TarZst,
    TarXz,
    TarBz2,
}

impl CreateFormat {
    pub const ALL: [CreateFormat; 7] = [
        CreateFormat::Zip,
        CreateFormat::SevenZ,
        CreateFormat::TarZst,
        CreateFormat::TarGz,
        CreateFormat::TarXz,
        CreateFormat::TarBz2,
        CreateFormat::Tar,
    ];

    pub fn extension(self) -> &'static str {
        match self {
            CreateFormat::Zip => "zip",
            CreateFormat::SevenZ => "7z",
            CreateFormat::Tar => "tar",
            CreateFormat::TarGz => "tar.gz",
            CreateFormat::TarZst => "tar.zst",
            CreateFormat::TarXz => "tar.xz",
            CreateFormat::TarBz2 => "tar.bz2",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            CreateFormat::Zip => "ZIP",
            CreateFormat::SevenZ => "7Z",
            CreateFormat::Tar => "TAR",
            CreateFormat::TarGz => "TAR.GZ",
            CreateFormat::TarZst => "TAR.ZST",
            CreateFormat::TarXz => "TAR.XZ",
            CreateFormat::TarBz2 => "TAR.BZ2",
        }
    }

    pub fn supports_password(self) -> bool {
        matches!(self, CreateFormat::Zip | CreateFormat::SevenZ)
    }

    pub fn from_path(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
        Some(match () {
            _ if name.ends_with(".zip") => CreateFormat::Zip,
            _ if name.ends_with(".7z") => CreateFormat::SevenZ,
            _ if name.ends_with(".tar.gz") || name.ends_with(".tgz") => CreateFormat::TarGz,
            _ if name.ends_with(".tar.zst") || name.ends_with(".tzst") => CreateFormat::TarZst,
            _ if name.ends_with(".tar.xz") || name.ends_with(".txz") => CreateFormat::TarXz,
            _ if name.ends_with(".tar.bz2") || name.ends_with(".tbz2") => CreateFormat::TarBz2,
            _ if name.ends_with(".tar") => CreateFormat::Tar,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZipMethod {
    Deflate,
    Zstd,
}

#[derive(Clone, Debug)]
pub struct CreateOptions {
    pub format: CreateFormat,
    /// 0 = store, 1 = fastest, 5 = normal, 9 = maximum.
    pub level: u32,
    /// 0 = all logical CPUs.
    pub threads: usize,
    pub password: Option<String>,
    pub zip_method: ZipMethod,
    /// 7z only: also encrypt file names.
    pub encrypt_names: bool,
    /// Store files like .jpg/.mp4/.zip without trying to compress them.
    pub store_precompressed: bool,
}

impl Default for CreateOptions {
    fn default() -> Self {
        Self {
            format: CreateFormat::Zip,
            level: 5,
            threads: 0,
            password: None,
            zip_method: ZipMethod::Deflate,
            encrypt_names: true,
            store_precompressed: true,
        }
    }
}

/// libdeflate level (1..=12) for our 1..=9 scale; 5 ("normal") -> 6, which
/// is ~40% faster than zlib's 6 with a slightly better ratio.
pub(crate) fn deflate_level(level: u32) -> i32 {
    [6, 1, 2, 4, 5, 6, 7, 9, 10, 11][level.min(9) as usize]
}

pub(crate) fn zstd_level(level: u32) -> i32 {
    [1, 1, 2, 3, 4, 6, 9, 12, 16, 19][level.min(9) as usize]
}

/// A file or directory to be archived.
pub(crate) struct Source {
    pub path: PathBuf,
    pub meta: EntryMeta,
    pub size: u64,
}

/// Create `out` from `inputs` (files and/or folders). The archive is written to
/// a temporary file next to `out` and renamed into place only on success.
pub fn create_archive(out: &Path, inputs: &[PathBuf], opts: &CreateOptions, progress: Arc<Progress>) -> Result<()> {
    if opts.password.is_some() && !opts.format.supports_password() {
        return Err(Error::Unsupported(format!("{} 格式不支持密码", opts.format.label())));
    }
    let out = std::path::absolute(out).at(out)?;
    let tmp = PathBuf::from(format!("{}.fztmp", out.display()));
    let trace = crate::util::Trace::new("create");
    progress.set_current("正在扫描文件…");
    let sources = scan_inputs(inputs, &[&out, &tmp], &progress)?;
    let total: u64 = sources.iter().map(|s| s.size).sum();
    progress.set_totals(total, sources.iter().filter(|s| !s.meta.is_dir).count() as u64);
    trace.mark(&format!("scanned {} entries", sources.len()));

    let result = match opts.format {
        CreateFormat::Zip => create_zip(&tmp, &sources, opts, &progress),
        CreateFormat::SevenZ => sevenz_write::create(&tmp, &sources, opts, &progress),
        _ => tarfmt::create(&tmp, &sources, opts, &progress),
    };
    trace.mark("compressed");
    match result {
        Ok(()) => std::fs::rename(&tmp, &out).at(&out),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

enum Piece {
    Dir,
    Whole { method: u16, crc: u32, size: u64, data: Vec<u8> },
    Chunk { method: u16, first: bool, last: bool, crc: crc32fast::Hasher, size: u64, data: Vec<u8> },
}

/// Read `len` bytes at `offset`, or fewer at end of file.
fn read_at(file: &File, path: &Path, mut offset: u64, len: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    let mut filled = 0;
    while filled < len {
        let n = file.seek_read(&mut buf[filled..], offset).at(path)?;
        if n == 0 {
            break;
        }
        filled += n;
        offset += n as u64;
    }
    buf.truncate(filled);
    Ok(buf)
}

fn create_zip(tmp: &Path, sources: &[Source], opts: &CreateOptions, progress: &Arc<Progress>) -> Result<()> {
    let threads = resolve_threads(opts.threads);
    let (method, chunk_size) = match (opts.level, opts.zip_method) {
        (0, _) => (METHOD_STORE, 4 << 20),
        (_, ZipMethod::Deflate) => (METHOD_DEFLATE, 1 << 20),
        (_, ZipMethod::Zstd) => (METHOD_ZSTD, 8 << 20),
    };
    let inflight = if method == METHOD_ZSTD { threads * 2 } else { threads * 4 };
    let dlevel = deflate_level(opts.level);
    let zlevel = zstd_level(opts.level);
    let password: Option<Arc<Vec<u8>>> = opts.password.as_ref().map(|p| Arc::new(p.as_bytes().to_vec()));
    let aes = password.is_some();

    let mut zw = ZipOut::create(tmp)?;
    let mut pool: OrderedPool<(usize, Piece)> = OrderedPool::new(threads, inflight);

    struct Open {
        entry: crate::zipwrite::StreamEntry,
        crc: crc32fast::Hasher,
        size: u64,
        enc: Option<AesEncryptor>,
    }
    let mut open: Option<Open> = None;

    let mut sink = |(idx, piece): (usize, Piece)| -> Result<()> {
        let meta = &sources[idx].meta;
        match piece {
            Piece::Dir => zw.add_dir(meta),
            Piece::Whole { method, crc, size, data } => {
                progress.set_current(&meta.name);
                zw.add_whole(meta, method, aes, crc, size, &data)
            }
            Piece::Chunk { method, first, last, crc, size, mut data } => {
                if first {
                    progress.set_current(&meta.name);
                    let entry = zw.begin_stream(meta, method, aes, sources[idx].size)?;
                    let enc = password.as_ref().map(|p| AesEncryptor::new(p));
                    if let Some(e) = &enc {
                        zw.stream_data(e.header())?;
                    }
                    open = Some(Open { entry, crc: crc32fast::Hasher::new(), size: 0, enc });
                }
                let st = open.as_mut().expect("stream started");
                if let Some(e) = &mut st.enc {
                    e.encrypt(&mut data);
                }
                zw.stream_data(&data)?;
                st.crc.combine(&crc);
                st.size += size;
                if last {
                    let st = open.take().expect("stream started");
                    if let Some(e) = st.enc {
                        zw.stream_data(&e.finish())?;
                    }
                    zw.end_stream(st.entry, st.crc.finalize(), st.size)?;
                }
                Ok(())
            }
        }
    };

    for (idx, src) in sources.iter().enumerate() {
        progress.check()?;
        if src.meta.is_dir {
            pool.push(move || Ok((idx, Piece::Dir)), &mut sink)?;
            continue;
        }
        let store = method == METHOD_STORE || (opts.store_precompressed && util::is_precompressed(&src.meta.name));
        let path = src.path.clone();
        let progress = progress.clone();

        if src.size <= chunk_size as u64 {
            let password = password.clone();
            let size_hint = src.size;
            pool.push(
                move || {
                    progress.check()?;
                    let data = util::read_file(&path, size_hint).at(&path)?;
                    let size = data.len() as u64;
                    let crc = crc32fast::hash(&data);
                    let mut out = (METHOD_STORE, data);
                    if !store && size > 0 {
                        let packed = match method {
                            METHOD_ZSTD => zstd_frame(zlevel, &out.1)?,
                            _ => deflate_whole(dlevel, &out.1),
                        };
                        if packed.len() < out.1.len() {
                            out = (method, packed);
                        }
                    }
                    let (m, mut data) = out;
                    if let Some(pw) = password {
                        data = AesEncryptor::seal(&pw, data);
                    }
                    progress.add_bytes(size);
                    progress.file_done();
                    Ok((idx, Piece::Whole { method: m, crc, size, data }))
                },
                &mut sink,
            )?;
            continue;
        }

        // Large file: compress fixed-size chunks in parallel.
        let file = Arc::new(File::open(&path).at(&path)?);
        let chunks = src.size.div_ceil(chunk_size as u64);
        let entry_method = if store { METHOD_STORE } else { method };
        for k in 0..chunks {
            let file = file.clone();
            let path = path.clone();
            let progress = progress.clone();
            let offset = k * chunk_size as u64;
            let last = k + 1 == chunks;
            pool.push(
                move || {
                    progress.check()?;
                    let buf = read_at(&file, &path, offset, chunk_size)?;
                    let data = &buf[..];
                    let mut crc = crc32fast::Hasher::new();
                    crc.update(data);
                    let size = data.len() as u64;
                    let packed = match entry_method {
                        METHOD_STORE => data.to_vec(),
                        METHOD_ZSTD => zstd_frame(zlevel, data)?,
                        _ => deflate_piece(dlevel, data, last)?,
                    };
                    progress.add_bytes(size);
                    if last {
                        progress.file_done();
                    }
                    Ok((idx, Piece::Chunk { method: entry_method, first: k == 0, last, crc, size, data: packed }))
                },
                &mut sink,
            )?;
        }
    }
    pool.finish(&mut sink)?;
    zw.finish()
}
