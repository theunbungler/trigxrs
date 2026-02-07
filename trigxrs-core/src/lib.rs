//! trigxrs-core: Core data structures and algorithms for trigram-based code search
//!
//! This crate provides:
//! - Trigram (ngram) encoding and decoding
//! - Delta-varint encoding for posting lists
//! - B+-tree index for efficient ngram lookups

pub mod ngram;
pub mod varint;
pub mod btree;
pub mod case;
pub mod error;

pub use ngram::{Ngram, NgramOffset, runes_to_ngram, ngram_to_runes, ngram_to_string, split_ngrams};
pub use varint::{encode_deltas, decode_deltas, encode_varint, decode_varint, MAX_VARINT_LEN};
pub use btree::{BPlusTree, BPlusTreeOpts};
pub use case::generate_case_ngrams;
pub use error::{Error, Result};
