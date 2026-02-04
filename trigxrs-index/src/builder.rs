//! Index builder for processing files and creating indexes
//!
//! The IndexBuilder provides:
//! - File processing with size and trigram limits
//! - Extension support via the IndexExtension trait
//! - Shard management for large codebases

use std::io::{Seek, Write};

use serde::{Deserialize, Serialize};
use trigxrs_core::{encode_deltas, BPlusTree, BPlusTreeOpts};

use crate::error::{Error, Result};
use crate::postings::{FinalizedPostings, PostingsBuilder};
use crate::writer::{tags, SectionWriter};

/// Default maximum shard size (100MB)
pub const DEFAULT_SHARD_MAX: usize = 100 * 1024 * 1024;

/// Default maximum file size (2MB)
pub const DEFAULT_FILE_MAX: usize = 2 * 1024 * 1024;

/// Default maximum trigram count per file (20,000)
pub const DEFAULT_TRIGRAM_MAX: usize = 20_000;

/// A section to be written by an extension
#[derive(Debug, Clone)]
pub struct ExtensionSection {
    /// Section tag
    pub tag: String,
    /// Section data
    pub data: Vec<u8>,
}

/// Extension trait for adding custom index sections
///
/// Extensions can process files during indexing and write custom sections
/// to the index. The core index ignores unknown tags when reading, so
/// extensions are forward-compatible.
pub trait IndexExtension: Send + Sync {
    /// Called for each file during indexing
    fn process_file(&mut self, path: &str, content: &[u8]);

    /// Get extension-specific sections to write
    fn get_sections(&self) -> Vec<ExtensionSection>;

    /// Section tags this extension owns
    fn section_tags(&self) -> &[&str];

    /// Reset state for a new shard
    fn reset(&mut self);
}

/// File metadata stored in the index
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// File path (relative to index root)
    pub path: String,
    /// Byte offset in the contents section
    pub content_offset: u64,
    /// Content length in bytes
    pub content_length: u32,
    /// Line ending positions (byte offsets)
    pub newlines: Vec<u32>,
}

/// Index metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexMetadata {
    /// Index format version
    pub version: u32,
    /// Number of files in the index
    pub file_count: u32,
    /// Total content bytes
    pub total_bytes: u64,
    /// Whether all content is plain ASCII
    pub plain_ascii: bool,
    /// Index creation timestamp (Unix seconds)
    pub created_at: u64,
}

/// Builder options
#[derive(Debug, Clone)]
pub struct BuilderOptions {
    /// Maximum shard size in bytes
    pub shard_max: usize,
    /// Maximum file size in bytes
    pub file_max: usize,
    /// Maximum unique trigrams per file
    pub trigram_max: usize,
    /// B+-tree bucket size
    pub btree_bucket_size: usize,
    /// B+-tree branching factor
    pub btree_branching_factor: usize,
}

impl Default for BuilderOptions {
    fn default() -> Self {
        Self {
            shard_max: DEFAULT_SHARD_MAX,
            file_max: DEFAULT_FILE_MAX,
            trigram_max: DEFAULT_TRIGRAM_MAX,
            btree_bucket_size: trigxrs_core::btree::DEFAULT_BUCKET_SIZE,
            btree_branching_factor: trigxrs_core::btree::DEFAULT_BRANCHING_FACTOR,
        }
    }
}

/// Result of checking if a file should be indexed
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// File should be indexed
    None,
    /// File is too small
    TooSmall,
    /// File is too large
    TooLarge,
    /// File contains binary data
    Binary,
    /// File has too many unique trigrams
    TooManyTrigrams,
}

impl SkipReason {
    pub fn should_skip(&self) -> bool {
        *self != SkipReason::None
    }
}

/// Checks documents for indexability
pub struct DocChecker {
    trigram_set: std::collections::HashSet<trigxrs_core::Ngram>,
    max_trigrams: usize,
}

impl DocChecker {
    pub fn new(max_trigrams: usize) -> Self {
        Self {
            trigram_set: std::collections::HashSet::with_capacity(max_trigrams),
            max_trigrams,
        }
    }

    /// Check if content should be indexed
    pub fn check(&mut self, content: &[u8], max_file_size: usize) -> SkipReason {
        if content.is_empty() {
            return SkipReason::None; // Empty files are OK
        }

        if content.len() < 3 {
            return SkipReason::TooSmall;
        }

        if content.len() > max_file_size {
            return SkipReason::TooLarge;
        }

        // Check for binary content (null bytes)
        if content.iter().any(|&b| b == 0) {
            return SkipReason::Binary;
        }

        // Check trigram count (only if content might exceed limit)
        let max_possible = content.len().saturating_sub(2);
        if max_possible > self.max_trigrams {
            self.trigram_set.clear();
            let content_str = String::from_utf8_lossy(content);

            for ng in trigxrs_core::split_ngrams(&content_str) {
                self.trigram_set.insert(ng.ngram);
                if self.trigram_set.len() > self.max_trigrams {
                    return SkipReason::TooManyTrigrams;
                }
            }
        }

        SkipReason::None
    }
}

/// Builder for creating search indexes
pub struct IndexBuilder {
    options: BuilderOptions,
    extensions: Vec<Box<dyn IndexExtension>>,

    // Current shard state
    postings: PostingsBuilder,
    files: Vec<FileEntry>,
    contents: Vec<u8>,
    current_size: usize,

    // Document checker
    doc_checker: DocChecker,
}

impl IndexBuilder {
    /// Create a new index builder with default options
    pub fn new() -> Self {
        Self::with_options(BuilderOptions::default())
    }

    /// Create a new index builder with custom options
    pub fn with_options(options: BuilderOptions) -> Self {
        let doc_checker = DocChecker::new(options.trigram_max);
        Self {
            options,
            extensions: Vec::new(),
            postings: PostingsBuilder::new(),
            files: Vec::new(),
            contents: Vec::new(),
            current_size: 0,
            doc_checker,
        }
    }

    /// Add an extension to the builder
    pub fn add_extension(&mut self, ext: Box<dyn IndexExtension>) {
        self.extensions.push(ext);
    }

    /// Get the current shard size
    pub fn current_size(&self) -> usize {
        self.current_size
    }

    /// Check if adding more content would exceed the shard limit
    pub fn would_exceed_shard(&self, additional_bytes: usize) -> bool {
        self.current_size + additional_bytes > self.options.shard_max
    }

    /// Get the number of files in the current shard
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Check if a file should be skipped
    pub fn check_file(&mut self, content: &[u8]) -> SkipReason {
        self.doc_checker.check(content, self.options.file_max)
    }

    /// Add a file to the index
    ///
    /// Returns the skip reason if the file was not added.
    pub fn add_file(&mut self, path: &str, content: &[u8]) -> Result<SkipReason> {
        // Check if file should be indexed
        let skip = self.check_file(content);
        if skip.should_skip() {
            return Ok(skip);
        }

        // Check shard size
        if self.would_exceed_shard(content.len()) {
            return Err(Error::InvalidIndex(
                "shard size exceeded, flush first".into(),
            ));
        }

        // Calculate newlines
        let newlines: Vec<u32> = content
            .iter()
            .enumerate()
            .filter(|&(_, &b)| b == b'\n')
            .map(|(i, _)| i as u32)
            .collect();

        // Add to contents
        let content_offset = self.contents.len() as u64;
        self.contents.extend_from_slice(content);

        // Add file entry
        self.files.push(FileEntry {
            path: path.to_string(),
            content_offset,
            content_length: content.len() as u32,
            newlines,
        });

        // Add to postings
        self.postings.add_document(content);

        // Update size
        self.current_size += content.len();

        // Process with extensions
        for ext in &mut self.extensions {
            ext.process_file(path, content);
        }

        Ok(SkipReason::None)
    }

    /// Write the current shard to the given writer
    pub fn write_shard<W: Write + Seek>(&mut self, writer: W) -> Result<()> {
        let mut section_writer =
            SectionWriter::new(writer).map_err(|e| Error::Io(e))?;

        // Finalize postings
        let postings = std::mem::replace(&mut self.postings, PostingsBuilder::new()).finalize();

        // Build B+-tree
        let btree = self.build_btree(&postings);
        let serialized_btree = btree.serialize_inner_nodes();

        // Write B+-tree section
        let btree_data =
            bincode::serialize(&serialized_btree).map_err(|e| Error::Serialization(e.to_string()))?;
        section_writer
            .write_section(tags::BTREE, &btree_data)
            .map_err(Error::Io)?;

        // Write trigrams section (sorted ngrams as u64 big-endian)
        section_writer.start_section(tags::TRIGRAMS).map_err(Error::Io)?;
        for &ng in &postings.ngrams {
            section_writer
                .writer_mut()
                .write_all(&ng.to_be_bytes())
                .map_err(Error::Io)?;
        }
        section_writer.end_section(tags::TRIGRAMS).map_err(Error::Io)?;

        // Write posting index (offsets into postings section)
        section_writer.start_section(tags::POSTING_INDEX).map_err(Error::Io)?;
        let mut offset = 0u32;
        for posting in &postings.posting_data {
            section_writer
                .writer_mut()
                .write_all(&offset.to_be_bytes())
                .map_err(Error::Io)?;
            offset += posting.len() as u32;
        }
        // Write final offset (end of postings)
        section_writer
            .writer_mut()
            .write_all(&offset.to_be_bytes())
            .map_err(Error::Io)?;
        section_writer.end_section(tags::POSTING_INDEX).map_err(Error::Io)?;

        // Write postings section
        section_writer.start_section(tags::POSTINGS).map_err(Error::Io)?;
        for posting in &postings.posting_data {
            section_writer
                .writer_mut()
                .write_all(posting)
                .map_err(Error::Io)?;
        }
        section_writer.end_section(tags::POSTINGS).map_err(Error::Io)?;

        // Write rune offsets
        let rune_offsets_data = encode_deltas(&postings.rune_offsets);
        section_writer
            .write_section(tags::RUNE_OFFSETS, &rune_offsets_data)
            .map_err(Error::Io)?;

        // Write end runes
        let end_runes_data = encode_deltas(&postings.end_runes);
        section_writer
            .write_section(tags::END_RUNES, &end_runes_data)
            .map_err(Error::Io)?;

        // Write contents section
        let contents = std::mem::take(&mut self.contents);
        section_writer
            .write_section(tags::CONTENTS, &contents)
            .map_err(Error::Io)?;

        // Write content bounds (offsets for each file)
        section_writer.start_section(tags::CONTENT_BOUNDS).map_err(Error::Io)?;
        for file in &self.files {
            section_writer
                .writer_mut()
                .write_all(&file.content_offset.to_le_bytes())
                .map_err(Error::Io)?;
        }
        // Write end offset
        section_writer
            .writer_mut()
            .write_all(&(contents.len() as u64).to_le_bytes())
            .map_err(Error::Io)?;
        section_writer.end_section(tags::CONTENT_BOUNDS).map_err(Error::Io)?;

        // Write files section
        let files = std::mem::take(&mut self.files);
        let files_data =
            bincode::serialize(&files).map_err(|e| Error::Serialization(e.to_string()))?;
        section_writer
            .write_section(tags::FILES, &files_data)
            .map_err(Error::Io)?;

        // Write newlines section (delta-encoded per file)
        section_writer.start_section(tags::NEWLINES).map_err(Error::Io)?;
        for file in &files {
            let encoded = encode_deltas(&file.newlines);
            // Write length prefix
            section_writer
                .writer_mut()
                .write_all(&(encoded.len() as u32).to_le_bytes())
                .map_err(Error::Io)?;
            section_writer
                .writer_mut()
                .write_all(&encoded)
                .map_err(Error::Io)?;
        }
        section_writer.end_section(tags::NEWLINES).map_err(Error::Io)?;

        // Write metadata
        let metadata = IndexMetadata {
            version: crate::writer::INDEX_VERSION,
            file_count: files.len() as u32,
            total_bytes: contents.len() as u64,
            plain_ascii: postings.is_plain_ascii,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        let metadata_data =
            bincode::serialize(&metadata).map_err(|e| Error::Serialization(e.to_string()))?;
        section_writer
            .write_section(tags::METADATA, &metadata_data)
            .map_err(Error::Io)?;

        // Write extension sections
        for ext in &self.extensions {
            for section in ext.get_sections() {
                section_writer
                    .write_section(&section.tag, &section.data)
                    .map_err(Error::Io)?;
            }
        }

        // Finalize
        section_writer.finish().map_err(Error::Io)?;

        // Reset state
        self.reset();

        Ok(())
    }

    /// Build the B+-tree from finalized postings
    fn build_btree(&self, postings: &FinalizedPostings) -> BPlusTree {
        let opts = BPlusTreeOpts {
            bucket_size: self.options.btree_bucket_size,
            v: self.options.btree_branching_factor,
        };

        let mut btree = BPlusTree::with_opts(opts);

        // Insert ngrams in sorted order
        for &ng in &postings.ngrams {
            btree.insert(ng);
        }

        btree.freeze();
        btree
    }

    /// Reset the builder state for a new shard
    pub fn reset(&mut self) {
        self.postings = PostingsBuilder::new();
        self.files.clear();
        self.contents.clear();
        self.current_size = 0;

        for ext in &mut self.extensions {
            ext.reset();
        }
    }
}

impl Default for IndexBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_empty_builder() {
        let builder = IndexBuilder::new();
        assert_eq!(builder.file_count(), 0);
        assert_eq!(builder.current_size(), 0);
    }

    #[test]
    fn test_add_file() {
        let mut builder = IndexBuilder::new();
        let result = builder.add_file("test.txt", b"hello world").unwrap();

        assert_eq!(result, SkipReason::None);
        assert_eq!(builder.file_count(), 1);
        assert_eq!(builder.current_size(), 11);
    }

    #[test]
    fn test_skip_binary() {
        let mut builder = IndexBuilder::new();
        let result = builder.add_file("binary.bin", b"hello\x00world").unwrap();

        assert_eq!(result, SkipReason::Binary);
        assert_eq!(builder.file_count(), 0);
    }

    #[test]
    fn test_skip_too_small() {
        let mut builder = IndexBuilder::new();
        let result = builder.add_file("tiny.txt", b"ab").unwrap();

        assert_eq!(result, SkipReason::TooSmall);
    }

    #[test]
    fn test_skip_too_large() {
        let mut builder = IndexBuilder::with_options(BuilderOptions {
            file_max: 100,
            ..Default::default()
        });

        let large: Vec<u8> = (0..200).map(|i| (b'a' + (i % 26)) as u8).collect();
        let result = builder.add_file("large.txt", &large).unwrap();

        assert_eq!(result, SkipReason::TooLarge);
    }

    #[test]
    fn test_write_and_read_shard() {
        let mut builder = IndexBuilder::new();
        builder.add_file("test1.txt", b"hello world").unwrap();
        builder.add_file("test2.txt", b"goodbye world").unwrap();

        let mut buf = Cursor::new(Vec::new());
        builder.write_shard(&mut buf).unwrap();

        // Verify we can read the index
        let data = buf.into_inner();
        let reader = crate::writer::SectionReader::new(&data).unwrap();

        assert!(reader.get_section(tags::CONTENTS).is_some());
        assert!(reader.get_section(tags::FILES).is_some());
        assert!(reader.get_section(tags::TRIGRAMS).is_some());
        assert!(reader.get_section(tags::POSTINGS).is_some());
        assert!(reader.get_section(tags::METADATA).is_some());
    }

    #[test]
    fn test_reset() {
        let mut builder = IndexBuilder::new();
        builder.add_file("test.txt", b"hello").unwrap();

        builder.reset();

        assert_eq!(builder.file_count(), 0);
        assert_eq!(builder.current_size(), 0);
    }

    #[test]
    fn test_shard_size_limit() {
        let mut builder = IndexBuilder::with_options(BuilderOptions {
            shard_max: 100,
            ..Default::default()
        });

        builder.add_file("test1.txt", b"hello world").unwrap();

        let large: Vec<u8> = (0..100).map(|i| (b'a' + (i % 26)) as u8).collect();
        let result = builder.add_file("test2.txt", &large);

        assert!(result.is_err());
    }

    #[test]
    fn test_metadata() {
        let mut builder = IndexBuilder::new();
        builder.add_file("test.txt", b"hello world").unwrap();

        let mut buf = Cursor::new(Vec::new());
        builder.write_shard(&mut buf).unwrap();

        let data = buf.into_inner();
        let reader = crate::writer::SectionReader::new(&data).unwrap();

        let metadata_bytes = reader.get_section(tags::METADATA).unwrap();
        let metadata: IndexMetadata = bincode::deserialize(metadata_bytes).unwrap();

        assert_eq!(metadata.file_count, 1);
        assert!(metadata.plain_ascii);
    }
}
