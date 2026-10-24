//! A ZIP writer built for parallel producers: entries arrive either fully
//! compressed (small files) or as an ordered stream of compressed chunks
//! (large files), and are written with ZIP64, UTF-8 names, NTFS timestamps
//! and optional WinZip AES-256.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::os::windows::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::error::{Error, IoContext, Result};
use crate::util::{to_dos_datetime, to_filetime};

pub const METHOD_STORE: u16 = 0;
pub const METHOD_DEFLATE: u16 = 8;
pub const METHOD_ZSTD: u16 = 93;
const METHOD_AES: u16 = 99;

const U32_MAX: u64 = 0xFFFF_FFFF;
/// Streamed entries above this size get ZIP64 local headers up front.
const ZIP64_STREAM_THRESHOLD: u64 = 0xF000_0000;

#[derive(Clone, Debug)]
pub struct EntryMeta {
    /// '/'-separated; directories end with '/'.
    pub name: String,
    pub is_dir: bool,
    pub mtime: Option<SystemTime>,
    pub atime: Option<SystemTime>,
    pub ctime: Option<SystemTime>,
    /// Windows file attributes.
    pub attrs: u32,
}

struct Record {
    name: Vec<u8>,
    flags: u16,
    method: u16,
    version_needed: u16,
    dos_time: u16,
    dos_date: u16,
    crc: u32,
    csize: u64,
    usize: u64,
    offset: u64,
    attrs: u32,
    ntfs: Option<[u64; 3]>,
    aes_method: Option<u16>,
}

pub struct StreamEntry {
    record: Record,
    zip64_local: bool,
    header_len: u64,
}

pub struct ZipOut {
    out: BufWriter<File>,
    path: PathBuf,
    pos: u64,
    records: Vec<Record>,
}

fn version_needed(method: u16, zip64: bool, aes: bool) -> u16 {
    let mut v = 20;
    if zip64 {
        v = 45;
    }
    if aes {
        v = 51;
    }
    if method == METHOD_ZSTD {
        v = 63;
    }
    v
}

impl ZipOut {
    pub fn create(path: &Path) -> Result<Self> {
        let file = File::create(path).at(path)?;
        Ok(Self { out: BufWriter::with_capacity(1 << 20, file), path: path.to_path_buf(), pos: 0, records: Vec::new() })
    }

    fn record(&self, meta: &EntryMeta, method: u16, aes: bool) -> Record {
        let (dos_time, dos_date) = to_dos_datetime(meta.mtime.unwrap_or_else(SystemTime::now));
        let ntfs = meta.mtime.map(|m| {
            [to_filetime(m), to_filetime(meta.atime.unwrap_or(m)), to_filetime(meta.ctime.unwrap_or(m))]
        });
        let mut flags = 1 << 11; // UTF-8 names
        if aes {
            flags |= 1;
        }
        Record {
            name: meta.name.as_bytes().to_vec(),
            flags,
            method: if aes { METHOD_AES } else { method },
            version_needed: version_needed(method, false, aes),
            dos_time,
            dos_date,
            crc: 0,
            csize: 0,
            usize: 0,
            offset: self.pos,
            attrs: meta.attrs,
            ntfs,
            aes_method: aes.then_some(method),
        }
    }

    fn write(&mut self, buf: &[u8]) -> Result<()> {
        self.out.write_all(buf).at(&self.path)?;
        self.pos += buf.len() as u64;
        Ok(())
    }

    fn local_header(rec: &Record, zip64: bool) -> Vec<u8> {
        let mut extra = Vec::new();
        if zip64 {
            extra.extend_from_slice(&1u16.to_le_bytes());
            extra.extend_from_slice(&16u16.to_le_bytes());
            extra.extend_from_slice(&rec.usize.to_le_bytes());
            extra.extend_from_slice(&rec.csize.to_le_bytes());
        }
        if let Some(m) = rec.aes_method {
            push_aes_extra(&mut extra, m);
        }
        let mut h = Vec::with_capacity(30 + rec.name.len() + extra.len());
        h.extend_from_slice(&0x04034b50u32.to_le_bytes());
        h.extend_from_slice(&rec.version_needed.to_le_bytes());
        h.extend_from_slice(&rec.flags.to_le_bytes());
        h.extend_from_slice(&rec.method.to_le_bytes());
        h.extend_from_slice(&rec.dos_time.to_le_bytes());
        h.extend_from_slice(&rec.dos_date.to_le_bytes());
        h.extend_from_slice(&rec.crc.to_le_bytes());
        if zip64 {
            h.extend_from_slice(&u32::MAX.to_le_bytes());
            h.extend_from_slice(&u32::MAX.to_le_bytes());
        } else {
            h.extend_from_slice(&(rec.csize as u32).to_le_bytes());
            h.extend_from_slice(&(rec.usize as u32).to_le_bytes());
        }
        h.extend_from_slice(&(rec.name.len() as u16).to_le_bytes());
        h.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        h.extend_from_slice(&rec.name);
        h.extend_from_slice(&extra);
        h
    }

    pub fn add_dir(&mut self, meta: &EntryMeta) -> Result<()> {
        let rec = self.record(meta, METHOD_STORE, false);
        let h = Self::local_header(&rec, false);
        self.write(&h)?;
        self.records.push(rec);
        Ok(())
    }

    /// Add an entry whose final payload (compressed, and encrypted if `aes`)
    /// is fully known.
    pub fn add_whole(&mut self, meta: &EntryMeta, method: u16, aes: bool, crc: u32, usize: u64, data: &[u8]) -> Result<()> {
        let mut rec = self.record(meta, method, aes);
        rec.crc = if aes { 0 } else { crc };
        rec.csize = data.len() as u64;
        rec.usize = usize;
        let zip64 = rec.csize >= U32_MAX || rec.usize >= U32_MAX;
        rec.version_needed = version_needed(method, zip64, aes);
        let h = Self::local_header(&rec, zip64);
        self.write(&h)?;
        self.write(data)?;
        self.records.push(rec);
        Ok(())
    }

    /// Start an entry whose payload will follow via [`Self::stream_data`].
    pub fn begin_stream(&mut self, meta: &EntryMeta, method: u16, aes: bool, size_hint: u64) -> Result<StreamEntry> {
        let mut rec = self.record(meta, method, aes);
        let zip64 = size_hint >= ZIP64_STREAM_THRESHOLD;
        rec.version_needed = version_needed(method, zip64, aes);
        let h = Self::local_header(&rec, zip64);
        self.write(&h)?;
        Ok(StreamEntry { record: rec, zip64_local: zip64, header_len: h.len() as u64 })
    }

    pub fn stream_data(&mut self, data: &[u8]) -> Result<()> {
        self.write(data)
    }

    /// Finish a streamed entry by patching its local header in place.
    pub fn end_stream(&mut self, mut e: StreamEntry, crc: u32, usize: u64) -> Result<()> {
        let rec = &mut e.record;
        rec.crc = if rec.aes_method.is_some() { 0 } else { crc };
        rec.usize = usize;
        rec.csize = self.pos - rec.offset - e.header_len;
        if !e.zip64_local && (rec.csize >= U32_MAX || rec.usize >= U32_MAX) {
            return Err(Error::Other(format!(
                "文件在压缩过程中变大，超过 4 GB: {}",
                String::from_utf8_lossy(&rec.name)
            )));
        }
        let h = Self::local_header(rec, e.zip64_local);
        self.out.flush().at(&self.path)?;
        self.out.get_ref().seek_write(&h, rec.offset).at(&self.path)?;
        self.out.seek(SeekFrom::Start(self.pos)).at(&self.path)?;
        self.records.push(e.record);
        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        let cd_start = self.pos;
        let records = std::mem::take(&mut self.records);
        let mut buf = Vec::with_capacity(records.len() * 96);
        for rec in &records {
            central_entry(&mut buf, rec);
            if buf.len() > 1 << 20 {
                self.write(&buf)?;
                buf.clear();
            }
        }
        self.write(&buf)?;
        let cd_size = self.pos - cd_start;
        let n = records.len() as u64;
        let mut tail = Vec::new();
        if n >= 0xFFFF || cd_size >= U32_MAX || cd_start >= U32_MAX {
            let z64_pos = self.pos;
            tail.extend_from_slice(&0x06064b50u32.to_le_bytes());
            tail.extend_from_slice(&44u64.to_le_bytes());
            tail.extend_from_slice(&63u16.to_le_bytes());
            tail.extend_from_slice(&45u16.to_le_bytes());
            tail.extend_from_slice(&0u32.to_le_bytes());
            tail.extend_from_slice(&0u32.to_le_bytes());
            tail.extend_from_slice(&n.to_le_bytes());
            tail.extend_from_slice(&n.to_le_bytes());
            tail.extend_from_slice(&cd_size.to_le_bytes());
            tail.extend_from_slice(&cd_start.to_le_bytes());
            tail.extend_from_slice(&0x07064b50u32.to_le_bytes());
            tail.extend_from_slice(&0u32.to_le_bytes());
            tail.extend_from_slice(&z64_pos.to_le_bytes());
            tail.extend_from_slice(&1u32.to_le_bytes());
        }
        tail.extend_from_slice(&0x06054b50u32.to_le_bytes());
        tail.extend_from_slice(&0u16.to_le_bytes());
        tail.extend_from_slice(&0u16.to_le_bytes());
        tail.extend_from_slice(&(n.min(0xFFFF) as u16).to_le_bytes());
        tail.extend_from_slice(&(n.min(0xFFFF) as u16).to_le_bytes());
        tail.extend_from_slice(&(cd_size.min(U32_MAX) as u32).to_le_bytes());
        tail.extend_from_slice(&(cd_start.min(U32_MAX) as u32).to_le_bytes());
        tail.extend_from_slice(&0u16.to_le_bytes());
        self.write(&tail)?;
        self.out.flush().at(&self.path)?;
        Ok(())
    }
}

fn push_aes_extra(extra: &mut Vec<u8>, method: u16) {
    extra.extend_from_slice(&0x9901u16.to_le_bytes());
    extra.extend_from_slice(&7u16.to_le_bytes());
    extra.extend_from_slice(&2u16.to_le_bytes()); // AE-2
    extra.extend_from_slice(b"AE");
    extra.push(3); // AES-256
    extra.extend_from_slice(&method.to_le_bytes());
}

fn central_entry(buf: &mut Vec<u8>, rec: &Record) {
    let mut extra = Vec::new();
    let mut z64 = Vec::new();
    if rec.usize >= U32_MAX {
        z64.extend_from_slice(&rec.usize.to_le_bytes());
    }
    if rec.csize >= U32_MAX {
        z64.extend_from_slice(&rec.csize.to_le_bytes());
    }
    if rec.offset >= U32_MAX {
        z64.extend_from_slice(&rec.offset.to_le_bytes());
    }
    if !z64.is_empty() {
        extra.extend_from_slice(&1u16.to_le_bytes());
        extra.extend_from_slice(&(z64.len() as u16).to_le_bytes());
        extra.extend_from_slice(&z64);
    }
    if let Some([m, a, c]) = rec.ntfs {
        extra.extend_from_slice(&0x000au16.to_le_bytes());
        extra.extend_from_slice(&32u16.to_le_bytes());
        extra.extend_from_slice(&0u32.to_le_bytes());
        extra.extend_from_slice(&1u16.to_le_bytes());
        extra.extend_from_slice(&24u16.to_le_bytes());
        extra.extend_from_slice(&m.to_le_bytes());
        extra.extend_from_slice(&a.to_le_bytes());
        extra.extend_from_slice(&c.to_le_bytes());
    }
    if let Some(m) = rec.aes_method {
        push_aes_extra(&mut extra, m);
    }
    let needed = if z64.is_empty() { rec.version_needed } else { rec.version_needed.max(45) };
    buf.extend_from_slice(&0x02014b50u32.to_le_bytes());
    buf.extend_from_slice(&63u16.to_le_bytes()); // made by: MS-DOS/Windows, spec 6.3
    buf.extend_from_slice(&needed.to_le_bytes());
    buf.extend_from_slice(&rec.flags.to_le_bytes());
    buf.extend_from_slice(&rec.method.to_le_bytes());
    buf.extend_from_slice(&rec.dos_time.to_le_bytes());
    buf.extend_from_slice(&rec.dos_date.to_le_bytes());
    buf.extend_from_slice(&rec.crc.to_le_bytes());
    buf.extend_from_slice(&(rec.csize.min(U32_MAX) as u32).to_le_bytes());
    buf.extend_from_slice(&(rec.usize.min(U32_MAX) as u32).to_le_bytes());
    buf.extend_from_slice(&(rec.name.len() as u16).to_le_bytes());
    buf.extend_from_slice(&(extra.len() as u16).to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // comment
    buf.extend_from_slice(&0u16.to_le_bytes()); // disk
    buf.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
    buf.extend_from_slice(&rec.attrs.to_le_bytes());
    buf.extend_from_slice(&(rec.offset.min(U32_MAX) as u32).to_le_bytes());
    buf.extend_from_slice(&rec.name);
    buf.extend_from_slice(&extra);
}
