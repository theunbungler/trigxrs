//! trigxrs-index: Index building for trigram-based code search
//!
//! This crate provides:
//! - `PostingsBuilder` for collecting trigram occurrences
//! - `IndexBuilder` for processing files and creating indexes
//! - `SectionWriter`/`SectionReader` for tagged section I/O
//! - `IndexExtension` trait for custom index sections

pub mod builder;
pub mod error;
pub mod postings;
pub mod writer;

pub use builder::{
    BuilderOptions, DocChecker, ExtensionSection, FileEntry, IndexBuilder, IndexExtension,
    IndexMetadata, SkipReason, DEFAULT_FILE_MAX, DEFAULT_SHARD_MAX, DEFAULT_TRIGRAM_MAX,
};
pub use error::{Error, Result};
pub use postings::{FinalizedPostings, PostingsBuilder, RUNE_OFFSET_FREQUENCY};
pub use writer::{
    IndexHeader, SectionReader, SectionWriter, TableOfContents, TocEntry, HEADER_SIZE,
    INDEX_MAGIC, INDEX_VERSION,
};
