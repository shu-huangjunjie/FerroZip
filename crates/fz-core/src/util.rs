use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use time::{OffsetDateTime, PrimitiveDateTime, UtcOffset};

/// Seconds between 1601-01-01 (Windows FILETIME epoch) and 1970-01-01.
const FILETIME_UNIX_DIFF: u64 = 11_644_473_600;

pub fn to_filetime(t: SystemTime) -> u64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() + FILETIME_UNIX_DIFF) * 10_000_000 + u64::from(d.subsec_nanos() / 100),
        Err(_) => 0,
    }
}

pub fn from_filetime(ft: u64) -> Option<SystemTime> {
    if ft == 0 {
        return None;
    }
    let secs = ft / 10_000_000;
    let nanos = (ft % 10_000_000) * 100;
    let unix = secs.checked_sub(FILETIME_UNIX_DIFF)?;
    Some(UNIX_EPOCH + Duration::new(unix, nanos as u32))
}

fn local_offset_at(t: OffsetDateTime) -> UtcOffset {
    UtcOffset::local_offset_at(t).unwrap_or(UtcOffset::UTC)
}

/// MS-DOS (time, date) in local time, as stored in ZIP headers.
pub fn to_dos_datetime(t: SystemTime) -> (u16, u16) {
    let utc = OffsetDateTime::from(t);
    let local = utc.to_offset(local_offset_at(utc));
    let year = local.year().clamp(1980, 2107);
    let date = (((year - 1980) as u16) << 9) | ((local.month() as u16) << 5) | local.day() as u16;
    let time = ((local.hour() as u16) << 11) | ((local.minute() as u16) << 5) | (local.second() as u16 / 2);
    (time, date)
}

/// Interpret a local-time calendar value (ZIP/RAR DOS timestamps) as a `SystemTime`.
pub fn from_local_parts(year: i32, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> Option<SystemTime> {
    let month = time::Month::try_from(month).ok()?;
    let date = time::Date::from_calendar_date(year, month, day).ok()?;
    let tm = time::Time::from_hms(hour, minute, second.min(59)).ok()?;
    let naive = PrimitiveDateTime::new(date, tm);
    let offset = local_offset_at(naive.assume_utc());
    Some(naive.assume_offset(offset).into())
}

pub fn from_dos_datetime(dos: u32) -> Option<SystemTime> {
    let date = (dos >> 16) as u16;
    let time = dos as u16;
    from_local_parts(
        1980 + i32::from(date >> 9),
        ((date >> 5) & 0xF) as u8,
        (date & 0x1F) as u8,
        (time >> 11) as u8,
        ((time >> 5) & 0x3F) as u8,
        ((time & 0x1F) * 2) as u8,
    )
}

const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9",
    "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Turn an archive entry name into a safe relative path: no drive letters,
/// no root, no `..`, no characters Windows rejects. Returns `None` if
/// nothing is left.
pub fn sanitize_entry_path(name: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for part in name.split(['/', '\\']) {
        let part = part.trim_end_matches([' ', '.']);
        if part.is_empty() || part == "." || part == ".." {
            continue;
        }
        // "C:" style prefixes
        if part.len() == 2 && part.ends_with(':') {
            continue;
        }
        let mut clean: String = part
            .chars()
            .map(|c| if c < ' ' || "<>:\"|?*".contains(c) { '_' } else { c })
            .collect();
        let stem = clean.split('.').next().unwrap_or("").to_ascii_uppercase();
        if RESERVED.contains(&stem.as_str()) {
            clean.insert(0, '_');
        }
        out.push(clean);
    }
    if out.as_os_str().is_empty() { None } else { Some(out) }
}

/// `dir/name (2).ext`, `dir/name (3).ext`, ... — the first that does not exist.
pub fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let parent = path.parent().unwrap_or(Path::new(""));
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (2..)
        .map(|i| parent.join(format!("{stem} ({i}){ext}")))
        .find(|p| !p.exists())
        .expect("unbounded search")
}

/// Archive name without archive extensions: `a.tar.gz` -> `a`, `b.part1.rar` -> `b`.
pub fn archive_stem(path: &Path) -> String {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let lower = name.to_ascii_lowercase();
    for ext in [
        ".tar.gz", ".tar.zst", ".tar.xz", ".tar.bz2", ".tgz", ".tzst", ".txz", ".tbz2", ".tbz", ".zip", ".7z",
        ".rar", ".tar", ".gz", ".zst", ".xz", ".bz2", ".zipx", ".001",
    ] {
        if lower.ends_with(ext) && lower.len() > ext.len() {
            let mut stem = name[..name.len() - ext.len()].to_string();
            let ls = stem.to_ascii_lowercase();
            if let Some(pos) = ls.rfind(".part") {
                if ls[pos + 5..].chars().all(|c| c.is_ascii_digit()) {
                    stem.truncate(pos);
                }
            }
            if ls.ends_with(".7z") || ls.ends_with(".zip") {
                stem.truncate(stem.len() - if ls.ends_with(".7z") { 3 } else { 4 });
            }
            return stem;
        }
    }
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or(name)
}

/// Extensions whose content is already compressed; deflating them only burns CPU.
pub fn is_precompressed(name: &str) -> bool {
    let ext = match name.rsplit_once('.') {
        Some((_, e)) => e.to_ascii_lowercase(),
        None => return false,
    };
    matches!(
        ext.as_str(),
        "zip" | "7z" | "rar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "lz4" | "lzma" | "cab" | "jpg" | "jpeg"
            | "png" | "gif" | "webp" | "heic" | "avif" | "jxl" | "mp3" | "aac" | "m4a" | "ogg" | "opus" | "flac"
            | "mp4" | "m4v" | "mkv" | "mov" | "avi" | "wmv" | "webm" | "flv" | "docx" | "xlsx" | "pptx" | "epub"
            | "apk" | "jar" | "whl" | "nupkg" | "msi" | "dmg" | "woff" | "woff2"
    )
}

/// Physical memory currently available, used to size LZMA thread counts.
pub fn available_memory() -> u64 {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    unsafe {
        let mut st: MEMORYSTATUSEX = std::mem::zeroed();
        st.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut st) != 0 { st.ullAvailPhys } else { 4 << 30 }
    }
}

/// Lexically normalise a relative path for comparisons (`a/./b` -> `a/b`).
pub fn normal_components(p: &Path) -> Vec<String> {
    p.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

/// Read a whole file whose size is (probably) already known from the scan:
/// one open with a sequential-read hint, a buffer of the right size, and no
/// extra metadata query as `std::fs::read` does.
pub(crate) fn read_file(path: &Path, size_hint: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
    let mut f = std::fs::File::options().read(true).custom_flags(FILE_FLAG_SEQUENTIAL_SCAN).open(path)?;
    // +1 so a file that is exactly `size_hint` long is read without growing.
    let mut data = Vec::with_capacity(size_hint as usize + 1);
    f.read_to_end(&mut data)?;
    Ok(data)
}

/// Phase timing printed to stderr when `FZ_TRACE` is set (profiling aid).
pub(crate) struct Trace {
    start: Option<(std::time::Instant, &'static str)>,
}

impl Trace {
    pub fn new(what: &'static str) -> Self {
        Trace { start: std::env::var_os("FZ_TRACE").map(|_| (std::time::Instant::now(), what)) }
    }

    pub fn mark(&self, phase: &str) {
        if let Some((t, what)) = self.start {
            eprintln!("[{what}] {:>8.1} ms  {phase}", t.elapsed().as_secs_f64() * 1000.0);
        }
    }
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.2} {}", UNITS[u])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_blocks_traversal() {
        assert_eq!(sanitize_entry_path("../../etc/passwd"), Some(PathBuf::from("etc\\passwd")));
        assert_eq!(sanitize_entry_path("C:\\Windows\\x.dll"), Some(PathBuf::from("Windows\\x.dll")));
        assert_eq!(sanitize_entry_path("a/CON.txt"), Some(PathBuf::from("a\\_CON.txt")));
        assert_eq!(sanitize_entry_path("a/b?.txt"), Some(PathBuf::from("a\\b_.txt")));
        assert_eq!(sanitize_entry_path("/"), None);
    }

    #[test]
    fn stems() {
        assert_eq!(archive_stem(Path::new("x/a.tar.gz")), "a");
        assert_eq!(archive_stem(Path::new("b.part01.rar")), "b");
        assert_eq!(archive_stem(Path::new("c.7z.001")), "c");
        assert_eq!(archive_stem(Path::new("d.zip")), "d");
    }

    #[test]
    fn dos_roundtrip() {
        let t = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let (tm, dt) = to_dos_datetime(t);
        let back = from_dos_datetime((u32::from(dt) << 16) | u32::from(tm)).unwrap();
        assert_eq!(back, t);
    }
}
