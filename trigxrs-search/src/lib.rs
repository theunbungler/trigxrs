//! trigxrs-search: Search execution for trigram-based code search
//!
//! This crate provides:
//! - Memory-mapped index reader (`IndexData`)
//! - Posting list iterators for efficient search
//! - Literal and regex search (coming soon)
//! - Search enricher trait for extensions

pub mod error;
pub mod posting;
pub mod reader;

pub use error::{Error, Result};
pub use posting::{
    collect_hits, CompressedPostingIterator, DistanceIterator, HitIterator, InMemoryIterator,
    MergingIterator, MAX_OFFSET,
};
pub use reader::{FileEntry, IndexData, IndexMetadata, TableOfContents, TocEntry};
