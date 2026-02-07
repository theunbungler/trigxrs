//! trigxrs-search: Search execution for trigram-based code search
//!
//! This crate provides:
//! - Memory-mapped index reader (`IndexData`)
//! - Posting list iterators for efficient search
//! - Literal search with trigram filtering
//! - Regex search with trigram-based candidate filtering
//! - Search enricher trait for extensions

pub mod error;
pub mod posting;
pub mod reader;
pub mod search;
pub mod searcher;

pub use error::{Error, Result};
pub use posting::{
    collect_hits, CompressedPostingIterator, DistanceIterator, HitIterator, InMemoryIterator,
    MergingIterator, MAX_OFFSET,
};
pub use reader::{FileEntry, IndexData, IndexMetadata, TableOfContents, TocEntry};
pub use search::{Match, MatchContext, Query, SearchOptions, SearchResult, SearchStats};
pub use searcher::Searcher;
