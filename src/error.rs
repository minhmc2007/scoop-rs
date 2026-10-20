//! Central error type for scoop-rs.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, ScoopError>;

#[derive(Error, Debug)]
pub enum ScoopError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("invalid URL: {0}")]
    Url(#[from] url::ParseError),

    #[error("git {op} failed:\n{stderr}")]
    Git { op: String, stderr: String },

    #[error("app '{0}' not found in any bucket. Try `scoop-rs bucket add main` first.")]
    AppNotFound(String),

    #[error("bucket '{0}' is not installed. Known buckets: `scoop-rs bucket known`")]
    BucketMissing(String),

    #[error("bucket '{0}' already exists at {1}")]
    BucketExists(String, String),

    #[error("'{0}' is already installed. Use --force to reinstall.")]
    AlreadyInstalled(String),

    #[error("'{0}' is not installed")]
    NotInstalled(String),

    #[error("hash mismatch for {file}:\n  expected: {expected}\n  actual:   {actual}")]
    HashMismatch {
        file: String,
        expected: String,
        actual: String,
    },

    #[error("download failed for {url}: {reason}")]
    Download { url: String, reason: String },

    #[error("unsupported archive or installer payload: {0} (supported: .zip, .7z, .tar.gz, .tgz, single-file executables; .msi/.exe installers run on Windows)")]
    UnsupportedPayload(String),

    #[error("PowerShell step failed (exit code {code}):\n{output}")]
    PowerShell { code: i32, output: String },

    #[error("manifest {path} is invalid: {problems}")]
    ManifestInvalid { path: String, problems: String },

    #[error("dependency cycle detected while resolving: {0}")]
    DependencyCycle(String),

    #[error("missing dependencies: {0}. Add the required buckets first (e.g. `scoop-rs bucket add extras`).")]
    MissingDependencies(String),

    #[error("another scoop-rs operation is already running (lock: {0})")]
    Locked(String),

    #[error("no such shim/alias: {0}")]
    ShimMissing(String),

    #[error("{0}")]
    Other(String),
}

impl ScoopError {
    pub fn other<S: Into<String>>(msg: S) -> Self {
        ScoopError::Other(msg.into())
    }
}
