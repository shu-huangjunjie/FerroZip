//! File-creation microbenchmark: replays a real directory tree (same paths and
//! contents) through different write strategies and thread counts, and reports
//! wall time plus the CPU time Windows Defender (MsMpEng) spent meanwhile.
//!
//!   cargo run --release -p fz-core --example fsbench -- <source dir> <scratch dir> [variants] [threads]

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
use windows_sys::Win32::System::Threading::*;

struct Item {
    rel: PathBuf,
    data: Vec<u8>,
}

fn load(root: &Path) -> (Vec<PathBuf>, Vec<Item>) {
    let mut dirs = Vec::new();
    let mut items = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            let rel = p.strip_prefix(root).unwrap().to_path_buf();
            if e.file_type().unwrap().is_dir() {
                dirs.push(rel);
                stack.push(p);
            } else {
                items.push(Item { rel, data: std::fs::read(&p).unwrap() });
            }
        }
    }
    dirs.sort();
    items.sort_by(|a, b| a.rel.cmp(&b.rel));
    (dirs, items)
}

fn wide(p: &Path) -> Vec<u16> {
    // Verbatim form so paths longer than MAX_PATH work.
    let s = p.to_string_lossy().replace('/', "\\");
    let v = if s.starts_with(r"\\?\") { s } else { format!(r"\\?\{s}") };
    std::ffi::OsStr::new(&v).encode_wide().chain(Some(0)).collect()
}

fn defender_cpu() -> Option<Duration> {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut pid = None;
        if Process32FirstW(snap, &mut e) != 0 {
            loop {
                let name = String::from_utf16_lossy(&e.szExeFile[..e.szExeFile.iter().position(|&c| c == 0).unwrap_or(0)]);
                if name.eq_ignore_ascii_case("MsMpEng.exe") {
                    pid = Some(e.th32ProcessID);
                    break;
                }
                if Process32NextW(snap, &mut e) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid?);
        if h.is_null() {
            return None;
        }
        let mut t = [FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 }; 4];
        let ok = GetProcessTimes(h, &mut t[0], &mut t[1], &mut t[2], &mut t[3]);
        CloseHandle(h);
        if ok == 0 {
            return None;
        }
        let ft = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
        Some(Duration::from_nanos((ft(t[2]) + ft(t[3])) * 100))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Variant {
    /// std::fs::write
    Std,
    /// File::create + write_all + set_modified
    StdTimes,
    /// CreateFileW + WriteFile + SetFileInformationByHandle(basic) + CloseHandle
    Raw,
    /// Raw, created with FILE_ATTRIBUTE_TEMPORARY, attribute cleared via the same basic-info call
    RawTemp,
    /// Raw without content (lower bound for create/close)
    Empty,
    /// Raw, but CloseHandle happens on a separate pool of closer threads
    DeferredClose,
}

fn create_raw(path: &Path, attrs: u32) -> HANDLE {
    let w = wide(path);
    unsafe { CreateFileW(w.as_ptr(), GENERIC_WRITE, 0, std::ptr::null(), CREATE_ALWAYS, attrs, std::ptr::null_mut()) }
}

fn write_raw(h: HANDLE, mut data: &[u8]) {
    while !data.is_empty() {
        let n = data.len().min(1 << 30) as u32;
        let mut written = 0u32;
        let ok = unsafe { WriteFile(h, data.as_ptr(), n, &mut written, std::ptr::null_mut()) };
        assert!(ok != 0, "WriteFile failed");
        data = &data[written as usize..];
    }
}

fn set_basic(h: HANDLE, mtime: i64) {
    let info = FILE_BASIC_INFO { CreationTime: 0, LastAccessTime: 0, LastWriteTime: mtime, ChangeTime: 0, FileAttributes: FILE_ATTRIBUTE_NORMAL };
    unsafe {
        SetFileInformationByHandle(h, FileBasicInfo, &info as *const _ as *const c_void, std::mem::size_of::<FILE_BASIC_INFO>() as u32);
    }
}

fn run(variant: Variant, items: &[Item], order: &[usize], dirs: &[PathBuf], dest: &Path, threads: usize) -> (f64, f64) {
    let _ = std::fs::remove_dir_all(dest);
    std::fs::create_dir_all(dest).unwrap();
    let t = Instant::now();
    for d in dirs {
        std::fs::create_dir_all(dest.join(d)).unwrap();
    }
    eprintln!("  (created {} dirs in {:.2}s)", dirs.len(), t.elapsed().as_secs_f64());
    let mtime: i64 = 133_000_000_000_000_000;
    let next = AtomicUsize::new(0);
    let cpu0 = defender_cpu();
    let start = Instant::now();
    std::thread::scope(|s| {
        let (tx, rx) = crossbeam_channel::bounded::<usize>(4096);
        if variant == Variant::DeferredClose {
            for _ in 0..64 {
                let rx = rx.clone();
                s.spawn(move || {
                    for h in rx {
                        unsafe { CloseHandle(h as HANDLE) };
                    }
                });
            }
        }
        for _ in 0..threads {
            let tx = tx.clone();
            let next = &next;
            s.spawn(move || {
                loop {
                    let k = next.fetch_add(1, Ordering::Relaxed);
                    if k >= order.len() {
                        break;
                    }
                    let it = &items[order[k]];
                    let path = dest.join(&it.rel);
                    match variant {
                        Variant::Std => std::fs::write(&path, &it.data).unwrap(),
                        Variant::StdTimes => {
                            use std::io::Write;
                            let mut f = std::fs::File::create(&path).unwrap();
                            f.write_all(&it.data).unwrap();
                            f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)).unwrap();
                        }
                        Variant::Raw | Variant::RawTemp | Variant::Empty | Variant::DeferredClose => {
                            let attrs = if variant == Variant::RawTemp { FILE_ATTRIBUTE_TEMPORARY } else { FILE_ATTRIBUTE_NORMAL };
                            let h = create_raw(&path, attrs);
                            assert!(h != INVALID_HANDLE_VALUE, "CreateFileW failed for {}", path.display());
                            if variant != Variant::Empty {
                                write_raw(h, &it.data);
                            }
                            set_basic(h, mtime);
                            if variant == Variant::DeferredClose {
                                tx.send(h as usize).unwrap();
                            } else {
                                unsafe { CloseHandle(h) };
                            }
                        }
                    }
                }
            });
        }
        drop(tx);
    });
    let secs = start.elapsed().as_secs_f64();
    let cpu = match (cpu0, defender_cpu()) {
        (Some(a), Some(b)) => (b - a).as_secs_f64(),
        _ => f64::NAN,
    };
    (secs, cpu)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let src = PathBuf::from(&args[1]);
    let scratch = PathBuf::from(&args[2]);
    let variants: Vec<Variant> = args
        .get(3)
        .map(|v| {
            v.split(',')
                .map(|n| match n {
                    "std" => Variant::Std,
                    "stdtimes" => Variant::StdTimes,
                    "raw" => Variant::Raw,
                    "rawtemp" => Variant::RawTemp,
                    "empty" => Variant::Empty,
                    "deferred" => Variant::DeferredClose,
                    other => panic!("unknown variant {other}"),
                })
                .collect()
        })
        .unwrap_or(vec![Variant::Raw]);
    let threads: Vec<usize> = args.get(4).map(|t| t.split(',').map(|x| x.parse().unwrap()).collect()).unwrap_or(vec![20]);
    let orders: Vec<&str> = args.get(5).map(|o| o.split(',').collect()).unwrap_or(vec!["dir"]);

    eprintln!("loading {} ...", src.display());
    let (dirs, items) = load(&src);
    let total: usize = items.iter().map(|i| i.data.len()).sum();
    eprintln!("{} files, {} dirs, {:.1} MB", items.len(), dirs.len(), total as f64 / 1e6);

    let dir_order: Vec<usize> = (0..items.len()).collect();
    let mut size_order = dir_order.clone();
    size_order.sort_by_key(|&i| std::cmp::Reverse(items[i].data.len()));

    println!("{:<14} {:>7} {:>6} {:>9} {:>11} {:>10}", "variant", "threads", "order", "wall s", "files/s", "defender s");
    for &v in &variants {
        for &t in &threads {
            for &o in &orders {
                let order = if o == "size" { &size_order } else { &dir_order };
                let dest = scratch.join("fsbench-out");
                let (secs, cpu) = run(v, &items, order, &dirs, &dest, t);
                println!("{:<14} {:>7} {:>6} {:>9.2} {:>11.0} {:>10.2}", format!("{v:?}"), t, o, secs, items.len() as f64 / secs, cpu);
            }
        }
    }
    let _ = std::fs::remove_dir_all(scratch.join("fsbench-out"));
}
