use std::io;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("{}: {source}", path.display())]
    IoPath { path: PathBuf, source: io::Error },
    #[error("需要密码")]
    PasswordRequired,
    #[error("密码错误")]
    WrongPassword,
    #[error("操作已取消")]
    Cancelled,
    #[error("不支持: {0}")]
    Unsupported(String),
    #[error("压缩文件已损坏: {0}")]
    Corrupt(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn io_path(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Error::IoPath { path: path.into(), source }
    }

    pub fn is_password_problem(&self) -> bool {
        matches!(self, Error::PasswordRequired | Error::WrongPassword)
    }
}

/// Attach a path to an `io::Result`.
pub(crate) trait IoContext<T> {
    fn at(self, path: &std::path::Path) -> Result<T>;
}

impl<T> IoContext<T> for io::Result<T> {
    fn at(self, path: &std::path::Path) -> Result<T> {
        self.map_err(|e| Error::io_path(path, e))
    }
}

impl From<zip::result::ZipError> for Error {
    fn from(e: zip::result::ZipError) -> Self {
        use zip::result::ZipError as Z;
        match e {
            Z::Io(io) => from_codec_io(io),
            Z::InvalidPassword => Error::WrongPassword,
            Z::UnsupportedArchive(msg) if msg == Z::PASSWORD_REQUIRED => Error::PasswordRequired,
            Z::UnsupportedArchive(msg) => Error::Unsupported(msg.to_string()),
            Z::CompressionMethodNotSupported(m) => Error::Unsupported(format!("压缩方法 {m}")),
            Z::InvalidArchive(msg) => Error::Corrupt(msg.into_owned()),
            Z::FileNotFound => Error::Corrupt("找不到条目".into()),
            other => Error::Other(other.to_string()),
        }
    }
}

impl From<sevenz_rust2::Error> for Error {
    fn from(e: sevenz_rust2::Error) -> Self {
        use sevenz_rust2::Error as S;
        match e {
            S::PasswordRequired => Error::PasswordRequired,
            S::MaybeBadPassword(_) => Error::WrongPassword,
            S::Io(io, _) | S::FileOpen(io, _) => from_codec_io(io),
            S::ChecksumVerificationFailed => Error::Corrupt("CRC 校验失败".into()),
            S::UnsupportedCompressionMethod(m) => Error::Unsupported(format!("压缩方法 {m}")),
            S::Unsupported(m) => Error::Unsupported(m.into_owned()),
            S::BadSignature(_) => Error::Corrupt("不是 7z 文件".into()),
            other => Error::Corrupt(other.to_string()),
        }
    }
}

/// Codec errors arrive as `io::Error`s; unwrap the ones we created ourselves
/// (e.g. cancellation) and classify data errors as corruption.
pub(crate) fn from_codec_io(e: io::Error) -> Error {
    if e.get_ref().is_some_and(|inner| inner.is::<Cancelled>()) {
        return Error::Cancelled;
    }
    match e.kind() {
        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof => Error::Corrupt(e.to_string()),
        _ => Error::Io(e),
    }
}

/// Marker carried inside `io::Error` so cancellation survives codec layers.
#[derive(Debug)]
pub(crate) struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("操作已取消")
    }
}

impl std::error::Error for Cancelled {}

pub(crate) fn cancelled_io() -> io::Error {
    io::Error::other(Cancelled)
}

/// Convert our error into an `io::Error` for use inside `Read`/`Write` impls,
/// keeping cancellation recognisable.
pub(crate) fn to_io(e: Error) -> io::Error {
    match e {
        Error::Cancelled => cancelled_io(),
        Error::Io(io) | Error::IoPath { source: io, .. } => io,
        other => io::Error::other(other.to_string()),
    }
}
