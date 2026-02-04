//! Error types for trigxrs-index

use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("core error: {0}")]
    Core(#[from] trigxrs_core::Error),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("file too large: {path} ({size} bytes, max {max} bytes)")]
    FileTooLarge {
        path: String,
        size: usize,
        max: usize,
    },

    #[error("too many trigrams in file: {path} ({count} trigrams, max {max})")]
    TooManyTrigrams {
        path: String,
        count: usize,
        max: usize,
    },

    #[error("invalid index: {0}")]
    InvalidIndex(String),
}

pub type Result<T> = std::result::Result<T, Error>;
