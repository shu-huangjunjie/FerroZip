//! Explorer starts one process per selected file for a context-menu verb.
//! The first process becomes the collector: the others drop their path into
//! a queue folder and exit, and the collector gathers them into one job.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
use windows_sys::Win32::System::Threading::CreateMutexW;

/// Returns the full list for the collector, or `None` if another process
/// took ownership (this process should exit quietly).
pub fn collect(verb: &str, mine: Vec<PathBuf>) -> Option<Vec<PathBuf>> {
    let queue = std::env::temp_dir().join("FerroZip").join(format!("queue{verb}"));
    let _ = std::fs::create_dir_all(&queue);
    let name: Vec<u16> = format!("Local\\FerroZip-collect{verb}\0").encode_utf16().collect();
    let handle = unsafe { CreateMutexW(std::ptr::null(), 1, name.as_ptr()) };
    let already = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if already {
        let stamp = format!("{}-{}", std::process::id(), Instant::now().elapsed().as_nanos());
        let tmp = queue.join(format!("{stamp}.tmp"));
        let body: Vec<String> = mine.iter().map(|p| p.display().to_string()).collect();
        if std::fs::write(&tmp, body.join("\n")).is_ok() {
            let _ = std::fs::rename(&tmp, queue.join(format!("{stamp}.txt")));
        }
        unsafe { CloseHandle(handle) };
        return None;
    }

    let mut files = mine;
    let start = Instant::now();
    let mut last_new = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(50));
        let mut got = false;
        if let Ok(rd) = std::fs::read_dir(&queue) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "txt") {
                    if let Ok(text) = std::fs::read_to_string(&p) {
                        files.extend(text.lines().filter(|l| !l.is_empty()).map(PathBuf::from));
                        got = true;
                    }
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
        if got {
            last_new = Instant::now();
        }
        if start.elapsed() > Duration::from_millis(400) && last_new.elapsed() > Duration::from_millis(250) {
            break;
        }
        if start.elapsed() > Duration::from_secs(3) {
            break;
        }
    }
    unsafe { CloseHandle(handle) };
    files.sort();
    files.dedup();
    Some(files)
}
