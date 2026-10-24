use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::codepage::Codepage;
use crate::error::{Error, IoContext, Result};
use crate::progress::Progress;
use crate::{sevenz, tarfmt, zipread};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    None,
    Gzip,
    Zstd,
    Xz,
    Bzip2,
}

impl Compression {
    pub fn label(self) -> &'static str {
        match self {
            Compression::None => "",
            Compression::Gzip => "GZ",
            Compression::Zstd => "ZST",
            Compression::Xz => "XZ",
            Compression::Bzip2 => "BZ2",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Zip,
    SevenZ,
    Rar,
    Tar(Compression),
    /// A single compressed file such as `notes.txt.gz`.
    Single(Compression),
}

impl Kind {
    pub fn label(self) -> String {
        match self {
            Kind::Zip => "ZIP".into(),
            Kind::SevenZ => "7Z".into(),
            Kind::Rar => "RAR".into(),
            Kind::Tar(Compression::None) => "TAR".into(),
            Kind::Tar(c) => format!("TAR.{}", c.label()),
            Kind::Single(c) => c.label().into(),
        }
    }

    /// Whether entries can be extracted in any order without decoding the
    /// whole stream (affects how the UI opens a single file).
    pub fn random_access(self) -> bool {
        matches!(self, Kind::Zip | Kind::SevenZ)
    }
}

#[derive(Clone, Debug, Default)]
pub struct EntryInfo {
    /// '/'-separated path without leading or trailing '/'.
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Compressed size, when the format records it per entry.
    pub packed: Option<u64>,
    pub mtime: Option<SystemTime>,
    pub crc: Option<u32>,
    pub encrypted: bool,
    pub method: String,
}

#[derive(Clone, Debug)]
pub struct Archive {
    pub path: PathBuf,
    pub kind: Kind,
    pub entries: Vec<EntryInfo>,
    pub solid: bool,
    pub archive_size: u64,
    pub comment: String,
    pub codepage: Codepage,
    /// Password that opened the archive (encrypted 7z headers), reused for extraction.
    pub password: Option<String>,
}

impl Archive {
    pub fn has_encrypted(&self) -> bool {
        self.entries.iter().any(|e| e.encrypted)
    }

    pub fn total_size(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }
}

#[derive(Clone, Debug, Default)]
pub struct OpenOptions {
    pub password: Option<String>,
    pub codepage: Codepage,
}

pub(crate) fn normalize_name(name: &str) -> String {
    let n = name.replace('\\', "/");
    n.trim_matches('/').to_string()
}

fn magic_kind(head: &[u8]) -> Option<Kind> {
    Some(match head {
        [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C, ..] => Kind::SevenZ,
        [b'R', b'a', b'r', b'!', 0x1A, 0x07, ..] => Kind::Rar,
        [b'P', b'K', 3, 4, ..] | [b'P', b'K', 5, 6, ..] | [b'P', b'K', 7, 8, ..] => Kind::Zip,
        [0x1F, 0x8B, ..] => Kind::Single(Compression::Gzip),
        [0x28, 0xB5, 0x2F, 0xFD, ..] => Kind::Single(Compression::Zstd),
        [0xFD, b'7', b'z', b'X', b'Z', 0, ..] => Kind::Single(Compression::Xz),
        [b'B', b'Z', b'h', ..] => Kind::Single(Compression::Bzip2),
        _ if tarfmt::looks_like_tar(head) => Kind::Tar(Compression::None),
        _ => return None,
    })
}

pub fn detect(path: &Path) -> Result<Kind> {
    let mut f = File::open(path).at(path)?;
    let mut head = vec![0u8; 512];
    let mut n = 0;
    while n < head.len() {
        let r = f.read(&mut head[n..]).at(path)?;
        if r == 0 {
            break;
        }
        n += r;
    }
    head.truncate(n);
    match magic_kind(&head) {
        Some(Kind::Single(c)) => {
            if tarfmt::compressed_is_tar(path, c)? {
                Ok(Kind::Tar(c))
            } else {
                Ok(Kind::Single(c))
            }
        }
        Some(k) => Ok(k),
        // Self-extracting or otherwise prefixed ZIP files.
        None if zipread::open_zip(path).is_ok() => Ok(Kind::Zip),
        None => Err(Error::Unsupported("无法识别的压缩格式".into())),
    }
}

/// Open an archive and read its directory.
pub fn open_archive(path: &Path, opts: &OpenOptions, progress: &Progress) -> Result<Archive> {
    let kind = detect(path)?;
    let archive_size = std::fs::metadata(path).at(path)?.len();
    let mut archive = Archive {
        path: path.to_path_buf(),
        kind,
        entries: Vec::new(),
        solid: false,
        archive_size,
        comment: String::new(),
        codepage: opts.codepage,
        password: opts.password.clone(),
    };
    match kind {
        Kind::Zip => zipread::list(&mut archive)?,
        Kind::SevenZ => sevenz::list(&mut archive)?,
        Kind::Tar(_) | Kind::Single(_) => tarfmt::list(&mut archive, progress)?,
        #[cfg(feature = "rar")]
        Kind::Rar => crate::rar::list(&mut archive)?,
        #[cfg(not(feature = "rar"))]
        Kind::Rar => return Err(Error::Unsupported("此版本未包含 RAR 支持".into())),
    }
    Ok(archive)
}
