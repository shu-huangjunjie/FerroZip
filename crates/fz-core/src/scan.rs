//! Parallel directory scan for archive creation.
//!
//! Every folder is listed with one `FindFirstFileExW` pass (large fetch, no
//! 8.3 names), which already carries sizes, times and attributes, and all
//! folders are listed concurrently. The result is assembled in the same
//! depth-first, name-sorted order a sequential walk would produce.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString, c_void};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_FILES, FILETIME, GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FIND_FIRST_EX_LARGE_FETCH, FindClose, FindExInfoBasic,
    FindExSearchNameMatch, FindFirstFileExW, FindNextFileW, WIN32_FIND_DATAW,
};

use crate::create::Source;
use crate::error::{Error, IoContext, Result};
use crate::progress::Progress;
use crate::util::{from_filetime, normal_components};
use crate::zipwrite::EntryMeta;

struct Raw {
    name: OsString,
    attrs: u32,
    size: u64,
    ctime: u64,
    atime: u64,
    mtime: u64,
}

impl Raw {
    fn is_dir(&self) -> bool {
        self.attrs & FILE_ATTRIBUTE_DIRECTORY != 0
    }

    /// Junctions and directory symlinks are recorded but not entered.
    fn descend(&self) -> bool {
        self.is_dir() && self.attrs & FILE_ATTRIBUTE_REPARSE_POINT == 0
    }
}

/// `\\?\` form of an absolute path, so long paths work.
fn verbatim(p: &Path) -> Vec<u16> {
    let s: Vec<u16> = p.as_os_str().encode_wide().map(|c| if c == u16::from(b'/') { u16::from(b'\\') } else { c }).collect();
    let starts = |prefix: &str| s.starts_with(&prefix.encode_utf16().collect::<Vec<_>>());
    let mut out: Vec<u16> = Vec::with_capacity(s.len() + 8);
    if starts(r"\\?\") {
        out.extend(&s);
    } else if starts(r"\\") {
        out.extend(r"\\?\UNC\".encode_utf16());
        out.extend(&s[2..]);
    } else {
        out.extend(r"\\?\".encode_utf16());
        out.extend(&s);
    }
    out
}

fn filetime(t: FILETIME) -> u64 {
    (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
}

fn list_dir(dir: &Path) -> std::io::Result<Vec<Raw>> {
    let mut pattern = verbatim(dir);
    if pattern.last() != Some(&u16::from(b'\\')) {
        pattern.push(u16::from(b'\\'));
    }
    pattern.extend([u16::from(b'*'), 0]);
    // SAFETY: plain Win32 calls on a zeroed out-structure and a handle that
    // is closed on every path.
    unsafe {
        let mut data: WIN32_FIND_DATAW = std::mem::zeroed();
        let h = FindFirstFileExW(
            pattern.as_ptr(),
            FindExInfoBasic,
            (&mut data as *mut WIN32_FIND_DATAW).cast::<c_void>(),
            FindExSearchNameMatch,
            std::ptr::null(),
            FIND_FIRST_EX_LARGE_FETCH,
        );
        if h == INVALID_HANDLE_VALUE {
            let err = GetLastError();
            return if err == ERROR_FILE_NOT_FOUND { Ok(Vec::new()) } else { Err(std::io::Error::from_raw_os_error(err as i32)) };
        }
        let mut out = Vec::new();
        loop {
            let len = data.cFileName.iter().position(|&c| c == 0).unwrap_or(data.cFileName.len());
            let name = &data.cFileName[..len];
            if name != [u16::from(b'.')] && name != [u16::from(b'.'), u16::from(b'.')] {
                out.push(Raw {
                    name: OsString::from_wide(name),
                    attrs: data.dwFileAttributes,
                    size: (u64::from(data.nFileSizeHigh) << 32) | u64::from(data.nFileSizeLow),
                    ctime: filetime(data.ftCreationTime),
                    atime: filetime(data.ftLastAccessTime),
                    mtime: filetime(data.ftLastWriteTime),
                });
            }
            if FindNextFileW(h, &mut data) == 0 {
                let err = GetLastError();
                FindClose(h);
                if err == ERROR_NO_MORE_FILES {
                    break;
                }
                return Err(std::io::Error::from_raw_os_error(err as i32));
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }
}

struct Walk<'a> {
    listings: Mutex<HashMap<PathBuf, Vec<Raw>>>,
    error: Mutex<Option<Error>>,
    progress: &'a Progress,
}

fn visit<'s>(s: &rayon::Scope<'s>, dir: PathBuf, walk: &'s Walk<'s>) {
    if walk.progress.is_cancelled() || walk.error.lock().unwrap().is_some() {
        return;
    }
    match list_dir(&dir) {
        Ok(entries) => {
            for e in entries.iter().filter(|e| e.descend()) {
                let sub = dir.join(&e.name);
                s.spawn(move |s| visit(s, sub, walk));
            }
            walk.listings.lock().unwrap().insert(dir, entries);
        }
        Err(e) => {
            walk.error.lock().unwrap().get_or_insert(Error::io_path(&dir, e));
        }
    }
}

/// `name` is the archive name without the trailing '/' of a folder.
fn source(path: PathBuf, mut name: String, raw: &Raw) -> Result<Source> {
    let is_dir = raw.is_dir();
    if is_dir {
        name.push('/');
    }
    let mut size = if is_dir { 0 } else { raw.size };
    if !is_dir && raw.attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        // A file symlink lists its own size (0); archive what it points to.
        size = std::fs::metadata(&path).at(&path)?.len();
    }
    Ok(Source {
        size,
        meta: EntryMeta {
            name,
            is_dir,
            mtime: from_filetime(raw.mtime),
            atime: from_filetime(raw.atime),
            ctime: from_filetime(raw.ctime),
            attrs: raw.attrs,
        },
        path,
    })
}

/// Paths to leave out (the archive being written), matched by file name
/// first so the common case costs no path building.
struct Exclude {
    names: HashSet<OsString>,
    paths: HashSet<PathBuf>,
}

impl Exclude {
    fn hit(&self, dir: &Path, name: &OsStr) -> bool {
        self.names.contains(name) && self.paths.contains(&dir.join(name))
    }
}

/// Append `dir`'s content (recursively) to `out`; `prefix` is the archive
/// name of `dir` including its trailing '/' (empty for a drive root).
fn emit(dir: &Path, prefix: &str, listings: &mut HashMap<PathBuf, Vec<Raw>>, exclude: &Exclude, out: &mut Vec<Source>) -> Result<()> {
    let Some(entries) = listings.remove(dir) else { return Ok(()) };
    for e in &entries {
        if exclude.hit(dir, &e.name) {
            continue;
        }
        let file = e.name.to_string_lossy();
        let mut name = String::with_capacity(prefix.len() + file.len() + 1);
        name.push_str(prefix);
        name.push_str(&file);
        let s = source(dir.join(&e.name), name, e)?;
        if e.descend() {
            let (sub, sub_prefix) = (s.path.clone(), s.meta.name.clone());
            out.push(s);
            emit(&sub, &sub_prefix, listings, exclude, out)?;
        } else {
            out.push(s);
        }
    }
    Ok(())
}

/// Files and folders under `inputs`, each folder followed by its content,
/// with archive names relative to the input's parent folder.
pub(crate) fn scan_inputs(inputs: &[PathBuf], exclude: &[&Path], progress: &Progress) -> Result<Vec<Source>> {
    let exclude = Exclude {
        names: exclude.iter().filter_map(|p| p.file_name().map(OsStr::to_os_string)).collect(),
        paths: exclude.iter().map(|p| p.to_path_buf()).collect(),
    };
    let mut out = Vec::new();
    for input in inputs {
        progress.check()?;
        let input = std::path::absolute(input).at(input)?;
        if exclude.paths.contains(&input) {
            continue;
        }
        let base = input.parent().map(Path::to_path_buf).unwrap_or_default();
        let md = std::fs::symlink_metadata(&input).at(&input)?;
        let raw = Raw {
            name: OsString::new(),
            attrs: md.file_attributes(),
            size: md.len(),
            ctime: md.creation_time(),
            atime: md.last_access_time(),
            mtime: md.last_write_time(),
        };
        let name = normal_components(input.strip_prefix(&base).unwrap_or(&input)).join("/");
        let mut prefix = String::new();
        if !name.is_empty() {
            let s = source(input.clone(), name, &raw)?;
            prefix = s.meta.name.clone();
            out.push(s);
        }
        if !raw.descend() {
            continue;
        }
        let trace = crate::util::Trace::new("scan");
        let walk = Walk { listings: Mutex::new(HashMap::new()), error: Mutex::new(None), progress };
        rayon::scope(|s| visit(s, input.clone(), &walk));
        progress.check()?;
        if let Some(e) = walk.error.into_inner().unwrap() {
            return Err(e);
        }
        let mut listings = walk.listings.into_inner().unwrap();
        trace.mark(&format!("listed {} folders", listings.len()));
        emit(&input, &prefix, &mut listings, &exclude, &mut out)?;
        trace.mark("assembled");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_order_as_a_sequential_walk() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("r");
        for d in ["b/y", "a", "c", "b/x/deep"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        for f in ["z.txt", "b/1.txt", "b/x/deep/f", "a/k", "c/m"] {
            std::fs::write(root.join(f), f).unwrap();
        }
        let got: Vec<String> = scan_inputs(&[root.clone()], &[], &Progress::new()).unwrap().into_iter().map(|s| s.meta.name).collect();
        let want = ["r/", "r/a/", "r/a/k", "r/b/", "r/b/1.txt", "r/b/x/", "r/b/x/deep/", "r/b/x/deep/f", "r/b/y/", "r/c/", "r/c/m", "r/z.txt"];
        assert_eq!(got, want);
        let sizes: u64 = scan_inputs(&[root], &[], &Progress::new()).unwrap().iter().map(|s| s.size).sum();
        assert_eq!(sizes, 5 + 7 + 10 + 3 + 3);
    }
}
