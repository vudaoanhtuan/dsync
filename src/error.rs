//! Crate-level error type. All library functions return `DsyncError`; `main.rs` is the only
//! place that renders these to the user and maps them to process exit codes (see cli.rs).

#[derive(thiserror::Error, Debug)]
pub enum DsyncError {
    #[error("not a dsync directory: run `dsync init <path>` first")]
    NotInitialized,
    #[error("already a dsync directory (use `dsync remote` to manage targets)")]
    AlreadyInitialized,
    #[error("config error: {0}")]
    Config(String),
    #[error("io error at {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("ssh error: {0}")]
    Ssh(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("integrity check failed for {0}")]
    Integrity(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, DsyncError>;

impl DsyncError {
    /// Helper to attach a path to a raw `std::io::Error`.
    pub fn io(path: impl AsRef<std::path::Path>, source: std::io::Error) -> Self {
        DsyncError::Io {
            path: path.as_ref().display().to_string(),
            source,
        }
    }
}
