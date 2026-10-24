//! FerroZip core: archive creation, listing, extraction and testing.
//!
//! Everything CPU-heavy is spread across threads: ZIP entries (and chunks of
//! large ZIP entries) are deflated in parallel, 7z solid blocks are encoded in
//! parallel, gzip/bzip2 streams are chunked like pigz/lbzip2, and extraction
//! decodes independent entries/blocks concurrently.

pub mod codepage;
pub mod create;
pub mod deflate;
pub mod error;
pub mod extract;
pub mod pool;
pub mod progress;
pub mod read;
pub mod util;

mod pstream;
mod scan;
mod sevenz;
mod sevenz_write;
mod tarfmt;
mod winzip_aes;
mod zipread;
mod zipwrite;
#[cfg(feature = "rar")]
mod rar;

pub use create::{CreateFormat, CreateOptions, ZipMethod, create_archive};
pub use error::{Error, Result};
pub use extract::{ExtractOptions, Overwrite, extract, smart_destination, test_archive};
pub use progress::Progress;
pub use read::{Archive, Compression, EntryInfo, Kind, OpenOptions, open_archive};

/// Number of worker threads to use when the caller asks for "auto" (0).
pub fn default_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
}

pub(crate) fn resolve_threads(threads: usize) -> usize {
    if threads == 0 { default_threads() } else { threads }
}
