//! Error types for trigxrs-search

use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("core error: {0}")]
    Core(#[from] trigxrs_core::Error),

    #[error("invalid index format: {0}")]
    InvalidIndex(String),

    #[error("regex error: {0}")]
    Regex(#[from] regex::Error),

    #[error("regex syntax error: {0}")]
    RegexSyntax(String),

    #[error("index not found: {0}")]
    IndexNotFound(String),

    #[error("corrupted posting list at offset {offset}")]
    CorruptedPostingList { offset: usize },
}

pub type Result<T> = std::result::Result<T, Error>;
