use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

use crate::error::{Error, Result};

/// Shared, lock-free progress counters. The UI polls these; workers bump them.
#[derive(Default)]
pub struct Progress {
    total_bytes: AtomicU64,
    done_bytes: AtomicU64,
    total_files: AtomicU64,
    done_files: AtomicU64,
    cancel: AtomicBool,
    current: Mutex<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub total_bytes: u64,
    pub done_bytes: u64,
    pub total_files: u64,
    pub done_files: u64,
    pub current: String,
}

impl Progress {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_totals(&self, bytes: u64, files: u64) {
        self.total_bytes.store(bytes, Relaxed);
        self.total_files.store(files, Relaxed);
    }

    pub fn add_bytes(&self, n: u64) {
        self.done_bytes.fetch_add(n, Relaxed);
    }

    pub fn file_done(&self) {
        self.done_files.fetch_add(1, Relaxed);
    }

    pub fn set_current(&self, name: &str) {
        if let Ok(mut cur) = self.current.try_lock() {
            cur.clear();
            cur.push_str(name);
        }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Relaxed)
    }

    /// `Err(Cancelled)` once the user pressed cancel.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() { Err(Error::Cancelled) } else { Ok(()) }
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            total_bytes: self.total_bytes.load(Relaxed),
            done_bytes: self.done_bytes.load(Relaxed),
            total_files: self.total_files.load(Relaxed),
            done_files: self.done_files.load(Relaxed),
            current: self.current.lock().map(|c| c.clone()).unwrap_or_default(),
        }
    }
}

/// `Read` adapter that reports bytes read and honours cancellation.
pub(crate) struct ProgressReader<'a, R> {
    pub inner: R,
    pub progress: &'a Progress,
}

impl<R: std::io::Read> std::io::Read for ProgressReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.progress.is_cancelled() {
            return Err(crate::error::cancelled_io());
        }
        let n = self.inner.read(buf)?;
        self.progress.add_bytes(n as u64);
        Ok(n)
    }
}
