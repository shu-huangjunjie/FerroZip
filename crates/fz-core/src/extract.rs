use std::collections::HashSet;
use std::fs::File;
use std::hash::{BuildHasher, RandomState};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crossbeam_channel::bounded;

use crate::error::{Error, IoContext, Result};
use crate::progress::Progress;
use crate::read::{Archive, Kind};
use crate::util::{archive_stem, sanitize_entry_path, unique_path};
use crate::{resolve_threads, sevenz, tarfmt, zipread};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Overwrite {
    #[default]
    Always,
    Skip,
    Rename,
}

#[derive(Clone, Debug, Default)]
pub struct ExtractOptions {
    pub dest: PathBuf,
    /// Entry indices to extract; `None` means everything. Selecting a directory
    /// entry selects everything below it.
    pub selection: Option<Vec<usize>>,
    /// Folder inside the archive ('/'-terminated) that is removed from output
    /// paths, so extracting `a/b/` while browsing `a/` yields `dest/b/...`.
    pub strip_prefix: String,
    pub overwrite: Overwrite,
    pub password: Option<String>,
    /// 0 = all logical CPUs.
    pub threads: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Summary {
    pub files: u64,
    pub skipped: u64,
}

/// Folders already created during this extraction, sharded so that worker
/// threads creating files in parallel rarely wait on each other.
struct DirCache {
    hasher: RandomState,
    shards: Vec<Mutex<HashSet<PathBuf>>>,
}

impl DirCache {
    fn new() -> Self {
        Self { hasher: RandomState::new(), shards: (0..64).map(|_| Mutex::new(HashSet::new())).collect() }
    }

    fn shard(&self, dir: &Path) -> &Mutex<HashSet<PathBuf>> {
        &self.shards[(self.hasher.hash_one(dir) % self.shards.len() as u64) as usize]
    }
}

/// Files at least this large get their final size allocated up front.
const PREALLOCATE_MIN: u64 = 8 << 20;

/// Where extracted data goes: a directory, or nowhere (integrity test).
pub(crate) struct Target {
    dest: Option<PathBuf>,
    strip: String,
    overwrite: Overwrite,
    dirs: DirCache,
    pub files: AtomicU64,
    pub skipped: AtomicU64,
}

impl Target {
    fn new(dest: Option<PathBuf>, strip: String, overwrite: Overwrite) -> Self {
        Self { dest, strip, overwrite, dirs: DirCache::new(), files: AtomicU64::new(0), skipped: AtomicU64::new(0) }
    }

    pub fn is_test(&self) -> bool {
        self.dest.is_none()
    }

    /// Output path for an entry, or `None` if it falls outside the strip prefix
    /// or has no usable name.
    pub fn resolve(&self, name: &str) -> Option<PathBuf> {
        let dest = self.dest.as_ref()?;
        let rel = name.strip_prefix(self.strip.as_str())?;
        Some(dest.join(sanitize_entry_path(rel)?))
    }

    fn ensure_dir(&self, dir: &Path) -> Result<()> {
        let shard = self.dirs.shard(dir);
        if shard.lock().unwrap().contains(dir) {
            return Ok(());
        }
        std::fs::create_dir_all(dir).at(dir)?;
        shard.lock().unwrap().insert(dir.to_path_buf());
        Ok(())
    }

    pub fn make_dir(&self, name: &str) -> Result<()> {
        match self.resolve(name) {
            Some(p) => self.ensure_dir(&p),
            None => Ok(()),
        }
    }

    /// Output path after applying the overwrite policy; `None` = skip.
    pub fn plan(&self, name: &str) -> Option<PathBuf> {
        let mut path = self.resolve(name)?;
        if self.overwrite != Overwrite::Always && path.exists() {
            match self.overwrite {
                Overwrite::Skip => {
                    self.skipped.fetch_add(1, Relaxed);
                    return None;
                }
                _ => path = unique_path(&path),
            }
        }
        Some(path)
    }

    /// Open the output for a file entry. `None` means "skip this entry".
    pub fn open(&self, name: &str) -> Result<Option<Sink>> {
        self.open_sized(name, 0)
    }

    /// Like `open`, with the expected size so large files can be allocated
    /// in one go instead of growing with every write.
    pub fn open_sized(&self, name: &str, size: u64) -> Result<Option<Sink>> {
        if self.is_test() {
            return Ok(Some(Sink::Null));
        }
        let Some(path) = self.plan(name) else { return Ok(None) };
        if let Some(parent) = path.parent() {
            self.ensure_dir(parent)?;
        }
        let file = match File::create(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied && path.is_file() => {
                // Overwriting a read-only file.
                let mut perms = std::fs::metadata(&path).at(&path)?.permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                perms.set_readonly(false);
                std::fs::set_permissions(&path, perms).at(&path)?;
                File::create(&path).at(&path)?
            }
            Err(e) => return Err(Error::io_path(path, e)),
        };
        let mut prealloc = 0;
        if size >= PREALLOCATE_MIN && file.set_len(size).is_ok() {
            prealloc = size;
        }
        Ok(Some(Sink::File { file: Some(file), path, written: 0, prealloc }))
    }

    /// Write a whole in-memory file.
    pub fn write_file(&self, name: &str, data: &[u8], mtime: Option<SystemTime>) -> Result<()> {
        if let Some(mut sink) = self.open_sized(name, data.len() as u64)? {
            sink.write_all(data).map_err(|e| sink.err(e))?;
            sink.finish(mtime)?;
            self.files.fetch_add(1, Relaxed);
        }
        Ok(())
    }
}

/// An open output file. Dropping it without `finish` deletes the partial file.
/// Writes go straight to the file: callers hand over large buffers (a whole
/// small file, or 512 KiB pieces), so buffering would only add a copy.
pub(crate) enum Sink {
    Null,
    File { file: Option<File>, path: PathBuf, written: u64, prealloc: u64 },
}

impl Sink {
    pub fn err(&self, e: io::Error) -> Error {
        match self {
            Sink::File { path, .. } => Error::io_path(path, e),
            Sink::Null => Error::Io(e),
        }
    }

    pub fn finish(mut self, mtime: Option<SystemTime>) -> Result<()> {
        if let Sink::File { file, path, written, prealloc } = &mut self {
            let file = file.take().expect("open");
            if *prealloc != 0 && *written != *prealloc {
                file.set_len(*written).map_err(|e| Error::io_path(path.clone(), e))?;
            }
            if let Some(t) = mtime {
                let _ = file.set_modified(t);
            }
        }
        Ok(())
    }
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Sink::Null => Ok(buf.len()),
            Sink::File { file, written, .. } => {
                let n = file.as_mut().expect("open").write(buf)?;
                *written += n as u64;
                Ok(n)
            }
        }
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match self {
            Sink::Null => Ok(()),
            Sink::File { file, written, .. } => {
                file.as_mut().expect("open").write_all(buf)?;
                *written += buf.len() as u64;
                Ok(())
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        if let Sink::File { file, path, .. } = self {
            if let Some(f) = file.take() {
                drop(f);
                let _ = std::fs::remove_file(&*path);
            }
        }
    }
}

/// Copy a decoder into a sink, reporting progress and honouring cancel.
/// The buffer is filled completely before each write, so decoders that hand
/// out small pieces still produce few, large `WriteFile` calls.
pub(crate) fn pump(reader: &mut dyn io::Read, sink: &mut Sink, buf: &mut [u8], progress: &Progress) -> Result<u64> {
    let mut total = 0;
    loop {
        progress.check()?;
        let mut filled = 0;
        while filled < buf.len() {
            match reader.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(crate::error::from_codec_io(e)),
            }
        }
        if filled > 0 {
            sink.write_all(&buf[..filled]).map_err(|e| sink.err(e))?;
            progress.add_bytes(filled as u64);
            total += filled as u64;
        }
        if filled < buf.len() {
            return Ok(total);
        }
    }
}

/// Small files decoded on a sequential stream (solid 7z, tar) are handed to
/// these threads for writing, because on Windows creating/closing files —
/// not decoding — is usually the bottleneck.
pub(crate) struct Writers<'a> {
    tx: crossbeam_channel::Sender<(String, Vec<u8>, Option<SystemTime>)>,
    error: &'a Mutex<Option<Error>>,
}

pub(crate) const OFFLOAD_MAX: u64 = 4 << 20;

impl Writers<'_> {
    pub fn write(&self, name: String, data: Vec<u8>, mtime: Option<SystemTime>) -> Result<()> {
        if let Some(e) = self.error.lock().unwrap().take() {
            return Err(e);
        }
        self.tx.send((name, data, mtime)).map_err(|_| Error::Other("写入线程已退出".into()))
    }
}

pub(crate) fn with_writers<R>(target: &Target, threads: usize, f: impl FnOnce(&Writers) -> Result<R>) -> Result<R> {
    let error = Mutex::new(None);
    let threads = threads.clamp(1, 8);
    std::thread::scope(|s| {
        let (tx, rx) = bounded::<(String, Vec<u8>, Option<SystemTime>)>(threads * 4);
        for _ in 0..threads {
            let rx = rx.clone();
            let error = &error;
            s.spawn(move || {
                for (name, data, mtime) in rx {
                    if let Err(e) = target.write_file(&name, &data, mtime) {
                        error.lock().unwrap().get_or_insert(e);
                    }
                }
            });
        }
        let result = f(&Writers { tx, error: &error });
        // `tx` was moved into the Writers value, which is dropped here, so the
        // threads finish the queue and exit before the scope ends.
        result
    })
    .and_then(|r| match error.into_inner().unwrap() {
        Some(e) => Err(e),
        None => Ok(r),
    })
}

/// Expand a selection of entry indices into a per-entry mask.
fn selection_mask(archive: &Archive, selection: &Option<Vec<usize>>) -> Vec<bool> {
    let n = archive.entries.len();
    let Some(sel) = selection else { return vec![true; n] };
    let mut mask = vec![false; n];
    let mut dirs: Vec<String> = Vec::new();
    for &i in sel {
        if i < n {
            mask[i] = true;
            if archive.entries[i].is_dir {
                dirs.push(format!("{}/", archive.entries[i].name));
            }
        }
    }
    if !dirs.is_empty() {
        for (i, e) in archive.entries.iter().enumerate() {
            if !mask[i] && dirs.iter().any(|d| e.name.starts_with(d.as_str())) {
                mask[i] = true;
            }
        }
    }
    mask
}

fn run(archive: &Archive, mask: &[bool], target: &Target, password: Option<&str>, threads: usize, progress: &Arc<Progress>) -> Result<Summary> {
    let threads = resolve_threads(threads);
    let total: u64 = archive.entries.iter().zip(mask).filter(|(_, m)| **m).map(|(e, _)| e.size).sum();
    let files = archive.entries.iter().zip(mask).filter(|(e, m)| **m && !e.is_dir).count() as u64;
    progress.set_totals(total, files);
    match archive.kind {
        Kind::Zip => zipread::extract(archive, mask, target, password, threads, progress)?,
        Kind::SevenZ => sevenz::extract(archive, mask, target, password, threads, progress)?,
        Kind::Tar(_) | Kind::Single(_) => {
            // Streams: progress is measured on the compressed input.
            progress.set_totals(archive.archive_size, files);
            tarfmt::extract(archive, mask, target, threads, progress)?
        }
        #[cfg(feature = "rar")]
        Kind::Rar => crate::rar::extract(archive, mask, target, password, progress)?,
        #[cfg(not(feature = "rar"))]
        Kind::Rar => return Err(Error::Unsupported("此版本未包含 RAR 支持".into())),
    }
    Ok(Summary { files: target.files.load(Relaxed), skipped: target.skipped.load(Relaxed) })
}

pub fn extract(archive: &Archive, opts: &ExtractOptions, progress: &Arc<Progress>) -> Result<Summary> {
    std::fs::create_dir_all(&opts.dest).at(&opts.dest)?;
    let mask = selection_mask(archive, &opts.selection);
    let target = Target::new(Some(opts.dest.clone()), opts.strip_prefix.clone(), opts.overwrite);
    let password = opts.password.as_deref().or(archive.password.as_deref());
    run(archive, &mask, &target, password, opts.threads, progress)
}

/// Decode everything and verify checksums without writing anything.
pub fn test_archive(archive: &Archive, password: Option<&str>, threads: usize, progress: &Arc<Progress>) -> Result<Summary> {
    let mask = vec![true; archive.entries.len()];
    let target = Target::new(None, String::new(), Overwrite::Always);
    let password = password.or(archive.password.as_deref());
    run(archive, &mask, &target, password, threads, progress)
}

/// Bandizip-style "smart" extraction folder: if the archive holds a single
/// top-level item, extract next to the archive; otherwise into a new folder
/// named after it.
pub fn smart_destination(archive: &Archive, parent: &Path) -> PathBuf {
    let mut tops = archive.entries.iter().map(|e| e.name.split('/').next().unwrap_or(""));
    if let Some(first) = tops.next() {
        if !first.is_empty() && tops.all(|t| t == first) {
            if let Some(rel) = sanitize_entry_path(first) {
                if !parent.join(rel).exists() {
                    return parent.to_path_buf();
                }
            }
        }
    }
    unique_path(&parent.join(archive_stem(&archive.path)))
}
