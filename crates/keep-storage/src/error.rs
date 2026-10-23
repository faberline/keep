//! Persistence errors.

use thiserror::Error;

/// Persistence errors
#[derive(Error, Debug)]
pub enum PersistenceError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Corrupted WAL entry at position {pos}: {reason}")]
    CorruptedWal { pos: u64, reason: String },

    #[error("Corrupted snapshot: {0}")]
    CorruptedSnapshot(String),

    #[error("Serialization error: {0}")]
    Serialization(#[from] bincode::Error),

    #[error("Checksum mismatch at position {pos}: expected {expected:x}, got {actual:x}")]
    ChecksumMismatch {
        pos: u64,
        expected: u32,
        actual: u32,
    },

    #[error("Unsupported version: {0}")]
    UnsupportedVersion(u32),

    #[error("Invalid magic number: expected {expected:?}, got {actual:?}")]
    InvalidMagic { expected: Vec<u8>, actual: Vec<u8> },

    #[error("Data directory error: {0}")]
    DataDirectory(String),
}

pub type Result<T> = std::result::Result<T, PersistenceError>;
