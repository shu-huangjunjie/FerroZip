//! 7z writer.
//!
//! * LZMA2 comes from liblzma's raw encoder (C, ~25% faster per thread than
//!   the pure-Rust encoder used for reading).
//! * Small files are grouped into solid blocks ("folders") that are
//!   compressed on all threads and written in *completion* order, so a slow
//!   block never stalls the others. Block sizes shrink towards the end of the
//!   job so that every core stays busy until the last moment.
//! * Large files are cut into pieces that are compressed independently and
//!   joined at LZMA2 dictionary resets, the same layout 7-Zip's multi-threaded
//!   LZMA2 produces, so a single huge file also uses every core.
//! * Already-compressed files (JPG, MP4, ZIP...) can go into Copy folders.
//! * 7-Zip-compatible AES-256 for the data and, optionally, the header.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::os::windows::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aes::Aes256;
use aes::cipher::{BlockCipherEncrypt, KeyInit};
use liblzma::stream::{Action, Filters, LzmaOptions, Status, Stream};

use crate::create::{CreateOptions, Source};
use crate::error::{Error, IoContext, Result};
use crate::pool::OrderedPool;
use crate::progress::Progress;
use crate::{resolve_threads, util};

const SIGNATURE: [u8; 6] = [b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C];

/// Property IDs of the 7z header.
mod id {
    pub const END: u8 = 0x00;
    pub const HEADER: u8 = 0x01;
    pub const MAIN_STREAMS_INFO: u8 = 0x04;
    pub const FILES_INFO: u8 = 0x05;
    pub const PACK_INFO: u8 = 0x06;
    pub const UNPACK_INFO: u8 = 0x07;
    pub const SUBSTREAMS_INFO: u8 = 0x08;
    pub const SIZE: u8 = 0x09;
    pub const CRC: u8 = 0x0A;
    pub const FOLDER: u8 = 0x0B;
    pub const CODERS_UNPACK_SIZE: u8 = 0x0C;
    pub const NUM_UNPACK_STREAM: u8 = 0x0D;
    pub const EMPTY_STREAM: u8 = 0x0E;
    pub const EMPTY_FILE: u8 = 0x0F;
    pub const NAME: u8 = 0x11;
    pub const MTIME: u8 = 0x14;
    pub const WIN_ATTRIBUTES: u8 = 0x15;
    pub const ENCODED_HEADER: u8 = 0x17;
}

/// LZMA2 dictionary per level, following 7-Zip (-mx5 = 16 MiB).
const DICT: [u32; 10] = [1 << 16, 1 << 20, 1 << 21, 1 << 22, 1 << 23, 1 << 24, 1 << 24, 1 << 25, 1 << 25, 1 << 26];

/// How large solid blocks (and pieces of large files) are. Each block resets
/// the dictionary, so bigger blocks compress better while more blocks keep
/// more cores busy. Blocks are sized from the work still left — about
/// `remaining / (jobs * div)` — so they start large and shrink towards the
/// end, and the cores (fast and slow ones alike) finish together.
#[derive(Clone, Copy)]
struct Schedule {
    /// Divisor ×10.
    div10: u64,
    min: u64,
    max: u64,
}

impl Schedule {
    fn for_level(level: u32) -> Self {
        // Measured on 1.1 GB of source code, 20 threads, against Bandizip
        // 7.43: level 5 takes 15.8 s / 75.0 MB (Bandizip 25.0 s / 76.8 MB),
        // level 9 38.6 s / 63.3 MB (Bandizip 46.2 s / 64.1 MB).
        match level {
            0..=3 => Schedule { div10: 20, min: 4 << 20, max: 32 << 20 },
            4..=6 => Schedule { div10: 15, min: 8 << 20, max: 64 << 20 },
            7 | 8 => Schedule { div10: 10, min: 16 << 20, max: 128 << 20 },
            _ => Schedule { div10: 5, min: 64 << 20, max: 256 << 20 },
        }
    }

    fn size(self, remaining: u64, jobs: usize) -> u64 {
        (remaining * 10 / (jobs as u64 * self.div10)).clamp(self.min, self.max)
    }
}

fn xz_err(e: liblzma::stream::Error) -> Error {
    Error::Other(format!("LZMA2: {e}"))
}

/// 1-byte LZMA2 property: the smallest encodable dictionary >= `dict`.
fn lzma2_dict_prop(dict: u32) -> u8 {
    (0..40u8).find(|b| (2u64 | u64::from(b & 1)) << (b / 2 + 11) >= u64::from(dict)).unwrap_or(40)
}

#[derive(Clone, Copy)]
enum Method {
    Copy,
    Lzma2 { preset: u32, dict: u32 },
}

impl Method {
    fn coder(self) -> Coder {
        match self {
            Method::Copy => Coder::Copy,
            Method::Lzma2 { dict, .. } => Coder::Lzma2(lzma2_dict_prop(dict)),
        }
    }
}

/// Raw LZMA2 encoder writing into a `Vec`.
struct Lzma2(Stream);

impl Lzma2 {
    fn new(preset: u32, dict: u32) -> Result<Self> {
        let mut opts = LzmaOptions::new_preset(preset).map_err(xz_err)?;
        opts.dict_size(dict);
        let mut filters = Filters::new();
        filters.lzma2(&opts);
        Ok(Lzma2(Stream::new_raw_encoder(&filters).map_err(xz_err)?))
    }

    fn reserve(out: &mut Vec<u8>) {
        if out.capacity() - out.len() < 256 * 1024 {
            out.reserve(1 << 20);
        }
    }

    fn feed(&mut self, mut input: &[u8], out: &mut Vec<u8>) -> Result<()> {
        while !input.is_empty() {
            Self::reserve(out);
            let before = self.0.total_in();
            self.0.process_vec(input, out, Action::Run).map_err(xz_err)?;
            input = &input[(self.0.total_in() - before) as usize..];
        }
        Ok(())
    }

    fn finish(mut self, out: &mut Vec<u8>) -> Result<()> {
        loop {
            Self::reserve(out);
            if self.0.process_vec(&[], out, Action::Finish).map_err(xz_err)? == Status::StreamEnd {
                return Ok(());
            }
        }
    }
}

/// Rough working set of one LZMA2 encoder (binary-tree match finder).
fn encoder_memory(dict: u32) -> u64 {
    u64::from(dict) * 23 / 2 + (8 << 20)
}

// ---------------------------------------------------------------- 7zAES

/// 7-Zip's key derivation: SHA-256 over (password UTF-16LE, round counter)
/// repeated 2^19 times, no salt.
fn derive_key(password: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let pw: Vec<u8> = password.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut h = Sha256::new();
    for round in 0u64..(1 << 19) {
        h.update(&pw);
        h.update(round.to_le_bytes());
    }
    h.finalize().into()
}

/// AES-256-CBC with a fresh random IV, fed in arbitrary pieces.
struct Cbc {
    cipher: Aes256,
    prev: [u8; 16],
    iv: [u8; 16],
    rem: Vec<u8>,
}

impl Cbc {
    fn new(key: &[u8; 32]) -> Self {
        let mut iv = [0u8; 16];
        getrandom::fill(&mut iv).expect("system RNG");
        Cbc { cipher: Aes256::new(key.into()), prev: iv, iv, rem: Vec::new() }
    }

    fn encrypt_blocks(&mut self, data: &mut [u8]) {
        for block in data.chunks_exact_mut(16) {
            for (b, p) in block.iter_mut().zip(self.prev) {
                *b ^= p;
            }
            let block: &mut aes::Block = block.try_into().expect("16-byte block");
            self.cipher.encrypt_block(block);
            self.prev.copy_from_slice(block);
        }
    }

    /// Encrypt what forms whole blocks, keep the rest for later.
    fn update(&mut self, data: &[u8]) -> Vec<u8> {
        self.rem.extend_from_slice(data);
        let full = self.rem.len() / 16 * 16;
        let mut out: Vec<u8> = self.rem.drain(..full).collect();
        self.encrypt_blocks(&mut out);
        out
    }

    /// Zero-pad and encrypt the tail. Returns it with the coder properties.
    fn finish(mut self) -> (Vec<u8>, Vec<u8>) {
        let mut tail = std::mem::take(&mut self.rem);
        if !tail.is_empty() {
            tail.resize(tail.len().next_multiple_of(16), 0);
            self.encrypt_blocks(&mut tail);
        }
        // NumCyclesPower 19, IV present (16 bytes), no salt.
        let mut props = vec![19 | 0x40, 0x0F];
        props.extend_from_slice(&self.iv);
        (tail, props)
    }
}

// ---------------------------------------------------------------- folders

#[derive(Clone)]
enum Coder {
    Copy,
    Lzma2(u8),
    Aes(Vec<u8>),
}

impl Coder {
    fn id(&self) -> &'static [u8] {
        match self {
            Coder::Copy => &[0x00],
            Coder::Lzma2(_) => &[0x21],
            Coder::Aes(_) => &[0x06, 0xF1, 0x07, 0x01],
        }
    }

    fn props(&self) -> Option<Vec<u8>> {
        match self {
            Coder::Copy => None,
            Coder::Lzma2(p) => Some(vec![*p]),
            Coder::Aes(p) => Some(p.clone()),
        }
    }
}

/// One folder: the main coder, optionally followed by AES whose output
/// feeds it (7-Zip's layout).
struct Folder {
    coders: Vec<Coder>,
    /// Output size of each coder, in coder order.
    unpack: Vec<u64>,
    pack_size: u64,
    /// CRC of the folder output (only used for the encoded header).
    crc: Option<u32>,
    /// (source index, size, CRC) of each file in the folder.
    streams: Vec<(usize, u64, u32)>,
}

/// A fully compressed folder, ready to be written.
struct Packed {
    data: Vec<u8>,
    folder: Folder,
}

impl Packed {
    fn seal(&mut self, key: &[u8; 32]) {
        let raw = self.data.len() as u64;
        let mut cbc = Cbc::new(key);
        let mut out = cbc.update(&self.data);
        let (tail, props) = cbc.finish();
        out.extend_from_slice(&tail);
        self.data = out;
        self.folder.coders.push(Coder::Aes(props));
        self.folder.unpack.push(raw);
        self.folder.pack_size = self.data.len() as u64;
    }
}

/// Compress files into one solid folder.
fn compress_block(files: Vec<(usize, PathBuf)>, method: Method, key: Option<Arc<[u8; 32]>>, progress: Arc<Progress>) -> Result<Packed> {
    let mut out = Vec::new();
    let mut enc = match method {
        Method::Lzma2 { preset, dict } => Some(Lzma2::new(preset, dict)?),
        Method::Copy => None,
    };
    let mut buf = vec![0u8; 1 << 20];
    let mut streams = Vec::with_capacity(files.len());
    let mut total = 0u64;
    for (idx, path) in files {
        progress.check()?;
        let mut f = File::open(&path).at(&path)?;
        let mut crc = crc32fast::Hasher::new();
        let mut size = 0u64;
        loop {
            let n = f.read(&mut buf).at(&path)?;
            if n == 0 {
                break;
            }
            crc.update(&buf[..n]);
            match &mut enc {
                Some(e) => e.feed(&buf[..n], &mut out)?,
                None => out.extend_from_slice(&buf[..n]),
            }
            size += n as u64;
            progress.add_bytes(n as u64);
            progress.check()?;
        }
        streams.push((idx, size, crc.finalize()));
        total += size;
        progress.file_done();
    }
    if let Some(e) = enc {
        e.finish(&mut out)?;
    }
    let mut packed = Packed {
        folder: Folder { coders: vec![method.coder()], unpack: vec![total], pack_size: out.len() as u64, crc: None, streams },
        data: out,
    };
    if let Some(k) = key {
        packed.seal(&k);
    }
    Ok(packed)
}

/// One piece of a large file.
struct Piece {
    big: usize,
    k: usize,
    last: bool,
    data: Vec<u8>,
    crc: crc32fast::Hasher,
    len: u64,
}

fn compress_piece(file: &File, path: &Path, offset: u64, len: u64, method: Method, last: bool, progress: &Progress) -> Result<(Vec<u8>, crc32fast::Hasher, u64)> {
    let mut out = Vec::new();
    let mut enc = match method {
        Method::Lzma2 { preset, dict } => Some(Lzma2::new(preset, dict)?),
        Method::Copy => None,
    };
    let mut buf = vec![0u8; (1 << 20).min(len.max(1) as usize)];
    let mut crc = crc32fast::Hasher::new();
    let mut done = 0u64;
    while done < len {
        progress.check()?;
        let want = buf.len().min((len - done) as usize);
        let n = file.seek_read(&mut buf[..want], offset + done).at(path)?;
        if n == 0 {
            break; // the file shrank while being read
        }
        crc.update(&buf[..n]);
        match &mut enc {
            Some(e) => e.feed(&buf[..n], &mut out)?,
            None => out.extend_from_slice(&buf[..n]),
        }
        done += n as u64;
        progress.add_bytes(n as u64);
    }
    if let Some(e) = enc {
        e.finish(&mut out)?;
        // Every piece is a complete LZMA2 stream; all but the last drop
        // their end marker so the next piece continues the same stream,
        // starting with a dictionary reset.
        if !last && out.last() == Some(&0) {
            out.pop();
        }
    }
    Ok((out, crc, done))
}

// ---------------------------------------------------------------- header

struct Buf(Vec<u8>);

impl Buf {
    fn byte(&mut self, b: u8) {
        self.0.push(b);
    }

    fn bytes(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }

    /// 7z variable-length number.
    fn number(&mut self, v: u64) {
        let mut first = 0u8;
        let mut mask = 0x80u8;
        let mut extra = 0;
        while extra < 8 {
            if v < (1u64 << (7 * (extra + 1))) {
                first |= (v >> (8 * extra)) as u8;
                break;
            }
            first |= mask;
            mask >>= 1;
            extra += 1;
        }
        self.byte(first);
        for i in 0..extra {
            self.byte((v >> (8 * i)) as u8);
        }
    }

    fn bits(&mut self, bits: &[bool]) {
        let mut v = vec![0u8; bits.len().div_ceil(8)];
        for (i, _) in bits.iter().enumerate().filter(|(_, b)| **b) {
            v[i / 8] |= 0x80 >> (i % 8);
        }
        self.bytes(&v);
    }

    fn property(&mut self, id: u8, data: &[u8]) {
        self.byte(id);
        self.number(data.len() as u64);
        self.bytes(data);
    }
}

fn write_folder(b: &mut Buf, f: &Folder) {
    b.number(f.coders.len() as u64);
    for c in &f.coders {
        let id = c.id();
        let props = c.props();
        b.byte(id.len() as u8 | if props.is_some() { 0x20 } else { 0 });
        b.bytes(id);
        if let Some(p) = props {
            b.number(p.len() as u64);
            b.bytes(&p);
        }
    }
    // Coder i+1 feeds coder i: bind (in stream i) <- (out stream i+1).
    for i in 1..f.coders.len() {
        b.number(i as u64 - 1);
        b.number(i as u64);
    }
}

fn write_streams_info(b: &mut Buf, pack_pos: u64, folders: &[Folder], substreams: bool) {
    b.byte(id::PACK_INFO);
    b.number(pack_pos);
    b.number(folders.len() as u64);
    b.byte(id::SIZE);
    for f in folders {
        b.number(f.pack_size);
    }
    b.byte(id::END);

    b.byte(id::UNPACK_INFO);
    b.byte(id::FOLDER);
    b.number(folders.len() as u64);
    b.byte(0);
    for f in folders {
        write_folder(b, f);
    }
    b.byte(id::CODERS_UNPACK_SIZE);
    for f in folders {
        for &u in &f.unpack {
            b.number(u);
        }
    }
    if folders.iter().any(|f| f.crc.is_some()) {
        b.byte(id::CRC);
        let all = folders.iter().all(|f| f.crc.is_some());
        b.byte(u8::from(all));
        if !all {
            b.bits(&folders.iter().map(|f| f.crc.is_some()).collect::<Vec<_>>());
        }
        for c in folders.iter().filter_map(|f| f.crc) {
            b.bytes(&c.to_le_bytes());
        }
    }
    b.byte(id::END);

    if substreams {
        b.byte(id::SUBSTREAMS_INFO);
        if folders.iter().any(|f| f.streams.len() != 1) {
            b.byte(id::NUM_UNPACK_STREAM);
            for f in folders {
                b.number(f.streams.len() as u64);
            }
            b.byte(id::SIZE);
            for f in folders {
                for s in &f.streams[..f.streams.len().saturating_sub(1)] {
                    b.number(s.1);
                }
            }
        }
        b.byte(id::CRC);
        b.byte(1);
        for f in folders {
            for s in &f.streams {
                b.bytes(&s.2.to_le_bytes());
            }
        }
        b.byte(id::END);
    }
    b.byte(id::END);
}

fn build_header(sources: &[Source], folders: &[Folder]) -> Vec<u8> {
    // Entries without data first (folders, then empty files), then every
    // file in the order of the folder streams.
    let mut order: Vec<usize> = (0..sources.len()).filter(|&i| sources[i].meta.is_dir).collect();
    let mut has_stream = std::collections::HashSet::new();
    for f in folders {
        for s in &f.streams {
            has_stream.insert(s.0);
        }
    }
    order.extend((0..sources.len()).filter(|&i| !sources[i].meta.is_dir && !has_stream.contains(&i)));
    order.extend(folders.iter().flat_map(|f| f.streams.iter().map(|s| s.0)));

    let mut b = Buf(Vec::with_capacity(64 + sources.len() * 64));
    b.byte(id::HEADER);
    if !folders.is_empty() {
        b.byte(id::MAIN_STREAMS_INFO);
        write_streams_info(&mut b, 0, folders, true);
    }
    if !order.is_empty() {
        b.byte(id::FILES_INFO);
        b.number(order.len() as u64);
        let empty: Vec<bool> = order.iter().map(|i| !has_stream.contains(i)).collect();
        if empty.iter().any(|e| *e) {
            let mut p = Buf(Vec::new());
            p.bits(&empty);
            b.property(id::EMPTY_STREAM, &p.0);
            let empty_files: Vec<bool> =
                order.iter().filter(|i| !has_stream.contains(*i)).map(|&i| !sources[i].meta.is_dir).collect();
            if empty_files.iter().any(|e| *e) {
                let mut p = Buf(Vec::new());
                p.bits(&empty_files);
                b.property(id::EMPTY_FILE, &p.0);
            }
        }
        let mut names = Buf(Vec::new());
        names.byte(0);
        for &i in &order {
            for u in sources[i].meta.name.trim_end_matches('/').encode_utf16() {
                names.bytes(&u.to_le_bytes());
            }
            names.bytes(&[0, 0]);
        }
        b.property(id::NAME, &names.0);

        let times: Vec<Option<u64>> = order.iter().map(|&i| sources[i].meta.mtime.map(util::to_filetime)).collect();
        if times.iter().any(Option::is_some) {
            let mut p = Buf(Vec::new());
            let all = times.iter().all(Option::is_some);
            p.byte(u8::from(all));
            if !all {
                p.bits(&times.iter().map(Option::is_some).collect::<Vec<_>>());
            }
            p.byte(0);
            for t in times.iter().flatten() {
                p.bytes(&t.to_le_bytes());
            }
            b.property(id::MTIME, &p.0);
        }

        let mut p = Buf(Vec::new());
        p.byte(1);
        p.byte(0);
        for &i in &order {
            // Plain DOS attributes only: 0x8000 would make 7-Zip read the
            // high half as a Unix mode.
            let mut a = sources[i].meta.attrs & 0x37;
            if sources[i].meta.is_dir {
                a |= 0x10;
            }
            p.bytes(&a.to_le_bytes());
        }
        b.property(id::WIN_ATTRIBUTES, &p.0);
        b.byte(id::END);
    }
    b.byte(id::END);
    b.0
}

// ---------------------------------------------------------------- output

struct Out {
    w: BufWriter<File>,
    path: PathBuf,
    /// Bytes written after the 32-byte signature header.
    pos: u64,
}

impl Out {
    fn write(&mut self, data: &[u8]) -> Result<()> {
        self.w.write_all(data).at(&self.path)?;
        self.pos += data.len() as u64;
        Ok(())
    }
}

/// A large file whose pieces are being appended.
struct OpenBig {
    big: usize,
    next: usize,
    start: u64,
    crc: crc32fast::Hasher,
    total: u64,
    raw_len: u64,
    cbc: Option<Cbc>,
}

/// Writes finished work in a valid layout: folders are contiguous, so while a
/// large file's pieces are being appended, other finished folders wait.
struct Writer {
    out: Out,
    folders: Vec<Folder>,
    bigs: Vec<(usize, Method)>,
    open: Option<OpenBig>,
    next_big: usize,
    waiting_pieces: HashMap<(usize, usize), Piece>,
    waiting_blocks: Vec<Packed>,
    key: Option<Arc<[u8; 32]>>,
}

impl Writer {
    fn write_block(&mut self, p: Packed) -> Result<()> {
        self.out.write(&p.data)?;
        self.folders.push(p.folder);
        Ok(())
    }

    fn block(&mut self, p: Packed) -> Result<()> {
        if self.open.is_some() {
            self.waiting_blocks.push(p);
            Ok(())
        } else {
            self.write_block(p)
        }
    }

    fn piece(&mut self, p: Piece) -> Result<()> {
        self.waiting_pieces.insert((p.big, p.k), p);
        self.advance()
    }

    fn advance(&mut self) -> Result<()> {
        loop {
            if self.open.is_none() {
                for b in std::mem::take(&mut self.waiting_blocks) {
                    self.write_block(b)?;
                }
                if self.next_big < self.bigs.len() && self.waiting_pieces.contains_key(&(self.next_big, 0)) {
                    self.open = Some(OpenBig {
                        big: self.next_big,
                        next: 0,
                        start: self.out.pos,
                        crc: crc32fast::Hasher::new(),
                        total: 0,
                        raw_len: 0,
                        cbc: self.key.as_deref().map(Cbc::new),
                    });
                } else {
                    return Ok(());
                }
            }
            let o = self.open.as_mut().expect("open");
            let Some(p) = self.waiting_pieces.remove(&(o.big, o.next)) else { return Ok(()) };
            o.next += 1;
            o.crc.combine(&p.crc);
            o.total += p.len;
            o.raw_len += p.data.len() as u64;
            let data = match &mut o.cbc {
                Some(c) => c.update(&p.data),
                None => p.data,
            };
            self.out.write(&data)?;
            if p.last {
                let o = self.open.take().expect("open");
                let (src, method) = self.bigs[o.big];
                let mut coders = vec![method.coder()];
                let mut unpack = vec![o.total];
                if let Some(c) = o.cbc {
                    let (tail, props) = c.finish();
                    self.out.write(&tail)?;
                    coders.push(Coder::Aes(props));
                    unpack.push(o.raw_len);
                }
                self.folders.push(Folder {
                    coders,
                    unpack,
                    pack_size: self.out.pos - o.start,
                    crc: None,
                    streams: vec![(src, o.total, o.crc.finalize())],
                });
                self.next_big += 1;
            }
        }
    }
}

enum Done {
    Block(Packed),
    Piece(Piece),
}

pub(crate) fn create(tmp: &Path, sources: &[Source], opts: &CreateOptions, progress: &Arc<Progress>) -> Result<()> {
    let threads = resolve_threads(opts.threads);
    let level = opts.level.min(9);
    let lzma = (level > 0).then(|| Method::Lzma2 { preset: level, dict: DICT[level as usize] });
    let key = opts.password.as_deref().map(|p| Arc::new(derive_key(p)));

    // Memory decides how many encoders may run at once.
    let dict = DICT[level as usize];
    let sched = Schedule::for_level(level);
    let max_block = sched.max;
    let per_job = if lzma.is_some() { encoder_memory(dict) + max_block } else { 2 << 20 };
    let budget = util::available_memory() / 10 * 6;
    let jobs = ((budget / per_job) as usize).clamp(1, threads);

    // Sort like 7-Zip (by extension, then name) so similar data shares a
    // dictionary; split out large files and, optionally, already-compressed
    // ones, which only waste LZMA time.
    let mut small: Vec<usize> = Vec::new();
    let mut stored: Vec<usize> = Vec::new();
    let mut bigs: Vec<(usize, Method)> = Vec::new();
    for (i, s) in sources.iter().enumerate() {
        if s.meta.is_dir || s.size == 0 {
            continue;
        }
        let precompressed = opts.store_precompressed && util::is_precompressed(&s.meta.name);
        let method = if precompressed { Method::Copy } else { lzma.unwrap_or(Method::Copy) };
        if s.size > max_block {
            bigs.push((i, method));
        } else if precompressed {
            stored.push(i);
        } else {
            small.push(i);
        }
    }
    // 7-Zip's order: extension, then file name *without* the folder, so the
    // many identical LICENSE / Cargo.toml / index.js files of a tree end up
    // next to each other inside one dictionary window.
    let ext_key = |i: &usize| {
        let path = sources[*i].meta.name.to_lowercase();
        let file = path.rsplit('/').next().unwrap_or("").to_string();
        let ext = file.rsplit_once('.').map(|(_, e)| e.to_string()).unwrap_or_default();
        (ext, file, sources[*i].size, path)
    };
    small.sort_by_cached_key(ext_key);
    stored.sort_by_cached_key(ext_key);

    let file = File::create(tmp).at(tmp)?;
    let mut w = Writer {
        out: Out { w: BufWriter::with_capacity(4 << 20, file), path: tmp.to_path_buf(), pos: 0 },
        folders: Vec::new(),
        bigs: bigs.clone(),
        open: None,
        next_big: 0,
        waiting_pieces: HashMap::new(),
        waiting_blocks: Vec::new(),
        key: key.clone(),
    };
    w.out.w.write_all(&[0u8; 32]).at(tmp)?;

    let mut pool: OrderedPool<Done> = OrderedPool::unordered(jobs, jobs * 2);
    let mut sink = |d: Done| match d {
        Done::Block(p) => w.block(p),
        Done::Piece(p) => w.piece(p),
    };

    // 1. Pieces of large files: the biggest units of work go first.
    for (b, &(i, method)) in bigs.iter().enumerate() {
        let src = &sources[i];
        let piece = match method {
            Method::Lzma2 { dict, .. } => sched.size(src.size, jobs).max(u64::from(dict)),
            Method::Copy => 16 << 20,
        };
        let file = Arc::new(File::open(&src.path).at(&src.path)?);
        let count = src.size.div_ceil(piece) as usize;
        progress.set_current(&src.meta.name);
        for k in 0..count {
            let (file, path, progress) = (file.clone(), src.path.clone(), progress.clone());
            let last = k + 1 == count;
            let offset = k as u64 * piece;
            // The last piece reads to the real end even if the file grew.
            let len = if last { u64::MAX - offset } else { piece };
            pool.push(
                move || {
                    let (data, crc, len) = compress_piece(&file, &path, offset, len, method, last, &progress)?;
                    if last {
                        progress.file_done();
                    }
                    Ok(Done::Piece(Piece { big: b, k, last, data, crc, len }))
                },
                &mut sink,
            )?;
        }
    }

    // 2. Already-compressed files: Copy folders, I/O only.
    let push_blocks = |list: &[usize], method: Method, pool: &mut OrderedPool<Done>, sink: &mut dyn FnMut(Done) -> Result<()>| -> Result<()> {
        let mut remaining: u64 = list.iter().map(|&i| sources[i].size).sum();
        let mut group: Vec<(usize, PathBuf)> = Vec::new();
        let mut group_size = 0u64;
        let mut target = 0u64;
        for (n, &i) in list.iter().enumerate() {
            progress.check()?;
            if group.is_empty() {
                // Guided sizes: big blocks first, smaller and smaller towards
                // the end so all workers finish at about the same time.
                target = match method {
                    Method::Copy => max_block,
                    Method::Lzma2 { .. } => sched.size(remaining, jobs),
                };
            }
            group.push((i, sources[i].path.clone()));
            group_size += sources[i].size;
            if group_size >= target || n + 1 == list.len() {
                remaining = remaining.saturating_sub(group_size);
                let files = std::mem::take(&mut group);
                group_size = 0;
                let (key, progress) = (key.clone(), progress.clone());
                pool.push(move || Ok(Done::Block(compress_block(files, method, key, progress)?)), &mut |d| sink(d))?;
            }
        }
        Ok(())
    };
    push_blocks(&stored, Method::Copy, &mut pool, &mut sink)?;
    // 3. Everything else: solid LZMA2 blocks.
    push_blocks(&small, lzma.unwrap_or(Method::Copy), &mut pool, &mut sink)?;
    pool.finish(&mut sink)?;
    drop(sink);
    w.advance()?;
    if w.open.is_some() || !w.waiting_pieces.is_empty() || !w.waiting_blocks.is_empty() {
        return Err(Error::Other("7z: 内部错误（未写完的数据块）".into()));
    }

    // Header, LZMA2-compressed (and encrypted when names are to be hidden).
    let header = build_header(sources, &w.folders);
    let header_pos = w.out.pos;
    let hdict = (header.len() as u32).next_power_of_two().clamp(1 << 16, 1 << 24);
    let mut enc = Lzma2::new(6, hdict)?;
    let mut packed = Vec::with_capacity(header.len() / 4 + 64);
    enc.feed(&header, &mut packed)?;
    enc.finish(&mut packed)?;
    let mut hp = Packed {
        folder: Folder {
            coders: vec![Coder::Lzma2(lzma2_dict_prop(hdict))],
            unpack: vec![header.len() as u64],
            pack_size: packed.len() as u64,
            crc: Some(crc32fast::hash(&header)),
            streams: Vec::new(),
        },
        data: packed,
    };
    if opts.encrypt_names {
        if let Some(k) = &key {
            hp.seal(k);
        }
    }
    w.out.write(&hp.data)?;
    let mut next = Buf(Vec::new());
    next.byte(id::ENCODED_HEADER);
    write_streams_info(&mut next, header_pos, std::slice::from_ref(&hp.folder), false);
    let next_offset = w.out.pos;
    w.out.write(&next.0)?;

    let mut start = Vec::with_capacity(20);
    start.extend_from_slice(&next_offset.to_le_bytes());
    start.extend_from_slice(&(next.0.len() as u64).to_le_bytes());
    start.extend_from_slice(&crc32fast::hash(&next.0).to_le_bytes());
    let mut sig = Vec::with_capacity(32);
    sig.extend_from_slice(&SIGNATURE);
    sig.extend_from_slice(&[0, 4]);
    sig.extend_from_slice(&crc32fast::hash(&start).to_le_bytes());
    sig.extend_from_slice(&start);
    let mut file = w.out.w.into_inner().map_err(|e| Error::io_path(tmp, e.into_error()))?;
    file.seek(SeekFrom::Start(0)).at(tmp)?;
    file.write_all(&sig).at(tmp)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        let enc = |v| {
            let mut b = Buf(Vec::new());
            b.number(v);
            b.0
        };
        assert_eq!(enc(0x7F), vec![0x7F]);
        assert_eq!(enc(0x80), vec![0x80, 0x80]);
        assert_eq!(enc(0x3FFF), vec![0xBF, 0xFF]);
        assert_eq!(enc(0x4000), vec![0xC0, 0x00, 0x40]);
        assert_eq!(enc(u64::MAX), vec![0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn dict_props() {
        assert_eq!(lzma2_dict_prop(1 << 12), 0);
        assert_eq!(lzma2_dict_prop(1 << 16), 8);
        assert_eq!(lzma2_dict_prop(8 << 20), 22);
        assert_eq!(lzma2_dict_prop(1 << 24), 24);
        assert_eq!(lzma2_dict_prop(3 << 23), 25);
        assert_eq!(lzma2_dict_prop((1 << 24) + 1), 25);
    }
}
