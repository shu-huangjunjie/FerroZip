use std::collections::HashSet;
use std::sync::Arc;

use unrar::error::{Code, UnrarError};

use crate::error::{Error, Result};
use crate::extract::Target;
use crate::progress::Progress;
use crate::read::{Archive, EntryInfo, normalize_name};
use crate::util::from_dos_datetime;

// unRAR reads its settings path from the registry; unrar_sys forgets to link it.
#[link(name = "advapi32")]
unsafe extern "system" {}

fn map_err(e: UnrarError) -> Error {
    match e.code {
        Code::MissingPassword => Error::PasswordRequired,
        Code::BadPassword => Error::WrongPassword,
        Code::BadData | Code::BadArchive | Code::UnknownFormat => Error::Corrupt(e.to_string()),
        Code::EOpen | Code::ERead => Error::Other(format!("无法读取压缩文件: {e}")),
        Code::ECreate | Code::EWrite | Code::EClose => Error::Other(format!("无法写入文件: {e}")),
        _ => Error::Other(e.to_string()),
    }
}

fn method_name(m: u32) -> String {
    match m {
        0x30 => "Store".into(),
        0x31..=0x35 => format!("RAR m{}", m - 0x30),
        other => format!("RAR {other:#x}"),
    }
}

fn open<'a>(archive: &'a Archive, password: Option<&'a str>) -> unrar::Archive<'a> {
    match password {
        Some(pw) => unrar::Archive::with_password(&archive.path, pw),
        None => unrar::Archive::new(&archive.path),
    }
}

pub(crate) fn list(archive: &mut Archive) -> Result<()> {
    let password = archive.password.clone();
    let mut entries = Vec::new();
    let mut solid = false;
    {
        let listing = open(archive, password.as_deref()).open_for_listing().map_err(map_err)?;
        solid = solid || listing.is_solid();
        for header in listing {
            let h = header.map_err(map_err)?;
            entries.push(EntryInfo {
                name: normalize_name(&h.filename.to_string_lossy()),
                is_dir: h.is_directory(),
                size: h.unpacked_size,
                packed: None,
                mtime: from_dos_datetime(h.file_time),
                crc: (!h.is_directory()).then_some(h.file_crc),
                encrypted: h.is_encrypted(),
                method: method_name(h.method),
            });
        }
    }
    archive.solid = solid;
    archive.entries = entries;
    Ok(())
}

pub(crate) fn extract(archive: &Archive, mask: &[bool], target: &Target, password: Option<&str>, progress: &Arc<Progress>) -> Result<()> {
    if password.is_none() && archive.entries.iter().zip(mask).any(|(e, m)| *m && e.encrypted) {
        return Err(Error::PasswordRequired);
    }
    // Match by name: multi-volume listings and processing order can differ.
    let wanted: HashSet<&str> = archive.entries.iter().zip(mask).filter(|(_, m)| **m).map(|(e, _)| e.name.as_str()).collect();
    let mut cursor = open(archive, password).open_for_processing().map_err(map_err)?;
    while let Some(header) = cursor.read_header().map_err(map_err)? {
        progress.check()?;
        let entry = header.entry();
        let name = normalize_name(&entry.filename.to_string_lossy());
        let size = entry.unpacked_size;
        let mtime = from_dos_datetime(entry.file_time);
        if !wanted.contains(name.as_str()) {
            cursor = header.skip().map_err(map_err)?;
            continue;
        }
        if entry.is_directory() {
            target.make_dir(&name)?;
            cursor = header.skip().map_err(map_err)?;
            continue;
        }
        progress.set_current(&name);
        if target.is_test() {
            cursor = header.test().map_err(map_err)?;
        } else {
            match target.plan(&name) {
                Some(path) => {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| Error::io_path(parent, e))?;
                    }
                    cursor = header.extract_to(&path).map_err(map_err)?;
                    if let (Some(t), Ok(f)) = (mtime, std::fs::File::options().write(true).open(&path)) {
                        let _ = f.set_modified(t);
                    }
                    target.files.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                None => cursor = header.skip().map_err(map_err)?,
            }
        }
        progress.add_bytes(size);
        progress.file_done();
    }
    Ok(())
}
