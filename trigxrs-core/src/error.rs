//! Error types for trigxrs-core

use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("invalid ngram encoding: {0}")]
    InvalidNgram(String),

    #[error("varint decoding error: unexpected end of data")]
    VarIntUnexpectedEnd,

    #[error("varint overflow: value too large")]
    VarIntOverflow,

    #[error("B+-tree error: {0}")]
    BTreeError(String),

    #[error("invalid data: {0}")]
    InvalidData(String),
}

pub type Result<T> = std::result::Result<T, Error>;
