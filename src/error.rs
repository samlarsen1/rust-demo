//! Crate-wide error type.

use thiserror::Error;

/// Errors produced by the read-only analytics pipeline.
#[derive(Debug, Error)]
pub enum Error {
    /// Underlying SQLite failure.
    #[error("storage error: {0}")]
    Storage(#[from] rusqlite::Error),

    /// Filesystem failure (e.g. creating the cache directory).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A market data source failed to produce data.
    #[error("market data source error: {0}")]
    Source(String),

    /// Persisted or supplied data could not be parsed.
    #[error("invalid data: {0}")]
    InvalidData(String),

    /// Invalid user input (CLI arguments, portfolio config, ...).
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;
