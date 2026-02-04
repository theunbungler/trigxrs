//! Memory-mapped index reader
//!
//! Provides efficient access to index files via memory mapping.
//! The index is loaded once and sections are accessed on-demand.

use std::path::Path;

use memmap2::Mmap;
use serde::{Deserialize, Serialize};

use trigxrs_core::{decode_deltas, decode_varint, Ngram};

use crate::error::{Error, Result};

/// Magic bytes for index files
pub const INDEX_MAGIC: &[u8; 4] = b"ZRST";

/// Header size in bytes
pub const HEADER_SIZE: usize = 32;

/// Footer size in bytes
pub const FOOTER_SIZE: usize = 12;

/// Section tags
pub mod tags {
    pub const TRIGRAMS: &str = "trigrams";
    pub const POSTINGS: &str = "postings";
    pub const POSTING_INDEX: &str = "posting_index";
    pub const CONTENTS: &str = "contents";
    pub const CONTENT_BOUNDS: &str = "content_bounds";
    pub const FILES: &str = "files";
    pub const NEWLINES: &str = "newlines";
    pub const METADATA: &str = "metadata";
    pub const BTREE: &str = "btree";
    pub const RUNE_OFFSETS: &str = "rune_offsets";
    pub const END_RUNES: &str = "end_runes";
}

/// Table of contents entry
#[derive(Debug, Clone)]
pub struct TocEntry {
    pub tag: String,
    pub offset: u64,
    pub length: u64,
}

/// Table of contents
#[derive(Debug, Clone, Default)]
pub struct TableOfContents {
    pub entries: Vec<TocEntry>,
}

impl TableOfContents {
    pub fn get(&self, tag: &str) -> Option<&TocEntry> {
        self.entries.iter().find(|e| e.tag == tag)
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let mut entries = Vec::new();
        let mut pos = 0;

        let (count, n) = decode_varint(&data[pos..])?;
        pos += n;

        for _ in 0..count {
            let (tag_len, n) = decode_varint(&data[pos..])?;
            pos += n;

            let tag_end = pos + tag_len as usize;
            if tag_end > data.len() {
                return Err(Error::InvalidIndex("truncated TOC tag".into()));
            }
            let tag = String::from_utf8_lossy(&data[pos..tag_end]).into_owned();
            pos = tag_end;

            if pos + 16 > data.len() {
                return Err(Error::InvalidIndex("truncated TOC entry".into()));
            }
            let offset = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
            let length = u64::from_le_bytes(data[pos + 8..pos + 16].try_into().unwrap());
            pos += 16;

            entries.push(TocEntry { tag, offset, length });
        }

        Ok(Self { entries })
    }
}

/// File entry from the index
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub content_offset: u64,
    pub content_length: u32,
    pub newlines: Vec<u32>,
}

/// Index metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexMetadata {
    pub version: u32,
    pub file_count: u32,
    pub total_bytes: u64,
    pub plain_ascii: bool,
    pub created_at: u64,
}

/// Serialized B+-tree structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedBTree {
    pub inner_nodes: Vec<SerializedInnerNode>,
    pub leaves: Vec<SerializedLeaf>,
    pub bucket_size: usize,
    pub v: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedInnerNode {
    pub keys: Vec<Ngram>,
    pub child_indices: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedLeaf {
    pub bucket_index: u32,
    pub posting_index_offset: u32,
}

const INNER_NODE_FLAG: u32 = 0x8000_0000;

impl SerializedBTree {
    /// Find the bucket for an ngram
    pub fn find(&self, ng: Ngram) -> Option<BTreeLookup> {
        if self.inner_nodes.is_empty() && self.leaves.is_empty() {
            return None;
        }

        if self.inner_nodes.is_empty() {
            let leaf = self.leaves.first()?;
            return Some(BTreeLookup {
                bucket_index: leaf.bucket_index as usize,
                posting_index_offset: leaf.posting_index_offset as usize,
            });
        }

        let mut current_idx = (self.inner_nodes.len() - 1) as u32 | INNER_NODE_FLAG;

        loop {
            if current_idx & INNER_NODE_FLAG != 0 {
                let inner = &self.inner_nodes[(current_idx & !INNER_NODE_FLAG) as usize];
                let mut child_idx = inner.child_indices.len() - 1;
                for (i, &key) in inner.keys.iter().enumerate() {
                    if ng < key {
                        child_idx = i;
                        break;
                    }
                }
                current_idx = inner.child_indices[child_idx];
            } else {
                let leaf = &self.leaves[current_idx as usize];
                return Some(BTreeLookup {
                    bucket_index: leaf.bucket_index as usize,
                    posting_index_offset: leaf.posting_index_offset as usize,
                });
            }
        }
    }
}

/// Result of a B+-tree lookup
#[derive(Debug, Clone, Copy)]
pub struct BTreeLookup {
    pub bucket_index: usize,
    pub posting_index_offset: usize,
}

/// Memory-mapped index data
#[allow(dead_code)]
pub struct IndexData {
    /// Memory-mapped file data
    mmap: Mmap,
    /// Table of contents
    toc: TableOfContents,
    /// B+-tree for ngram lookups
    btree: SerializedBTree,
    /// Index metadata
    metadata: IndexMetadata,
    /// File entries
    files: Vec<FileEntry>,
    /// Content section offset
    content_offset: u64,
    /// Content section length
    content_length: u64,
    /// Trigrams section data
    trigrams_offset: u64,
    trigrams_length: u64,
    /// Posting index offset and length
    posting_index_offset: u64,
    posting_index_length: u64,
    /// Postings data offset and length
    postings_offset: u64,
    postings_length: u64,
    /// Rune offset mappings
    rune_offsets: Vec<u32>,
    /// End rune positions per document
    end_runes: Vec<u32>,
}

impl IndexData {
    /// Open an index file
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let file = std::fs::File::open(path.as_ref())
            .map_err(|e| Error::Io(e))?;

        let mmap = unsafe { Mmap::map(&file) }
            .map_err(|e| Error::Io(e))?;

        Self::from_mmap(mmap)
    }

    /// Create from memory-mapped data
    pub fn from_mmap(mmap: Mmap) -> Result<Self> {
        let data = &mmap[..];

        if data.len() < HEADER_SIZE + FOOTER_SIZE {
            return Err(Error::InvalidIndex("file too small".into()));
        }

        // Validate header magic
        if &data[0..4] != INDEX_MAGIC {
            return Err(Error::InvalidIndex("invalid magic bytes".into()));
        }

        // Read footer
        let footer_start = data.len() - FOOTER_SIZE;
        let toc_offset = u64::from_le_bytes(data[footer_start..footer_start + 8].try_into().unwrap());

        // Validate footer magic
        if &data[footer_start + 8..] != INDEX_MAGIC {
            return Err(Error::InvalidIndex("invalid footer magic".into()));
        }

        // Read TOC
        let toc_start = toc_offset as usize;
        let toc_end = footer_start;
        if toc_start >= toc_end || toc_start >= data.len() {
            return Err(Error::InvalidIndex("invalid TOC offset".into()));
        }

        let toc = TableOfContents::from_bytes(&data[toc_start..toc_end])?;

        // Read B+-tree
        let btree_entry = toc.get(tags::BTREE)
            .ok_or_else(|| Error::InvalidIndex("missing btree section".into()))?;
        let btree_data = &data[btree_entry.offset as usize..(btree_entry.offset + btree_entry.length) as usize];
        let btree: SerializedBTree = bincode::deserialize(btree_data)
            .map_err(|e| Error::InvalidIndex(format!("btree deserialize: {}", e)))?;

        // Read metadata
        let meta_entry = toc.get(tags::METADATA)
            .ok_or_else(|| Error::InvalidIndex("missing metadata section".into()))?;
        let meta_data = &data[meta_entry.offset as usize..(meta_entry.offset + meta_entry.length) as usize];
        let metadata: IndexMetadata = bincode::deserialize(meta_data)
            .map_err(|e| Error::InvalidIndex(format!("metadata deserialize: {}", e)))?;

        // Read files
        let files_entry = toc.get(tags::FILES)
            .ok_or_else(|| Error::InvalidIndex("missing files section".into()))?;
        let files_data = &data[files_entry.offset as usize..(files_entry.offset + files_entry.length) as usize];
        let files: Vec<FileEntry> = bincode::deserialize(files_data)
            .map_err(|e| Error::InvalidIndex(format!("files deserialize: {}", e)))?;

        // Get content section info
        let content_entry = toc.get(tags::CONTENTS)
            .ok_or_else(|| Error::InvalidIndex("missing contents section".into()))?
            .clone();

        // Get trigrams section info
        let trigrams_entry = toc.get(tags::TRIGRAMS)
            .ok_or_else(|| Error::InvalidIndex("missing trigrams section".into()))?
            .clone();

        // Get posting index info
        let posting_index_entry = toc.get(tags::POSTING_INDEX)
            .ok_or_else(|| Error::InvalidIndex("missing posting_index section".into()))?
            .clone();

        // Get postings info
        let postings_entry = toc.get(tags::POSTINGS)
            .ok_or_else(|| Error::InvalidIndex("missing postings section".into()))?
            .clone();

        // Read rune offsets
        let rune_offsets = if let Some(entry) = toc.get(tags::RUNE_OFFSETS) {
            let rune_data = &data[entry.offset as usize..(entry.offset + entry.length) as usize];
            decode_deltas(rune_data)?
        } else {
            Vec::new()
        };

        // Read end runes
        let end_runes = if let Some(entry) = toc.get(tags::END_RUNES) {
            let rune_data = &data[entry.offset as usize..(entry.offset + entry.length) as usize];
            decode_deltas(rune_data)?
        } else {
            Vec::new()
        };

        Ok(Self {
            mmap,
            toc,
            btree,
            metadata,
            files,
            content_offset: content_entry.offset,
            content_length: content_entry.length,
            trigrams_offset: trigrams_entry.offset,
            trigrams_length: trigrams_entry.length,
            posting_index_offset: posting_index_entry.offset,
            posting_index_length: posting_index_entry.length,
            postings_offset: postings_entry.offset,
            postings_length: postings_entry.length,
            rune_offsets,
            end_runes,
        })
    }

    /// Get the index metadata
    pub fn metadata(&self) -> &IndexMetadata {
        &self.metadata
    }

    /// Get the file entries
    pub fn files(&self) -> &[FileEntry] {
        &self.files
    }

    /// Get the number of files
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Get the number of trigrams
    pub fn ngram_count(&self) -> usize {
        (self.trigrams_length / 8) as usize
    }

    /// Get a file's content
    pub fn file_content(&self, file_idx: usize) -> Option<&[u8]> {
        let file = self.files.get(file_idx)?;
        let start = (self.content_offset + file.content_offset) as usize;
        let end = start + file.content_length as usize;

        if end <= self.mmap.len() {
            Some(&self.mmap[start..end])
        } else {
            None
        }
    }

    /// Get a slice of content by byte offset and length
    pub fn content_slice(&self, offset: u32, length: u32) -> Option<&[u8]> {
        let start = self.content_offset as usize + offset as usize;
        let end = start + length as usize;

        if end <= self.mmap.len() {
            Some(&self.mmap[start..end])
        } else {
            None
        }
    }

    /// Lookup an ngram and return its posting list
    pub fn get_posting_list(&self, ng: Ngram) -> Option<&[u8]> {
        // Find bucket via B+-tree
        let lookup = self.btree.find(ng)?;

        // Get the bucket of ngrams
        let bucket_size = self.btree.bucket_size / 2; // Half-filled buckets
        let bucket_start = self.trigrams_offset as usize + lookup.bucket_index * bucket_size * 8;

        // Calculate actual bucket size (last bucket may be smaller)
        let remaining_ngrams = self.ngram_count() - lookup.bucket_index * bucket_size;
        let actual_bucket_size = remaining_ngrams.min(bucket_size);
        let bucket_end = bucket_start + actual_bucket_size * 8;

        if bucket_end > self.mmap.len() {
            return None;
        }

        let bucket_data = &self.mmap[bucket_start..bucket_end];

        // Binary search for the ngram in the bucket
        let mut left = 0;
        let mut right = actual_bucket_size;

        while left < right {
            let mid = (left + right) / 2;
            let mid_ng = u64::from_be_bytes(
                bucket_data[mid * 8..(mid + 1) * 8].try_into().unwrap()
            );

            if mid_ng < ng {
                left = mid + 1;
            } else {
                right = mid;
            }
        }

        if left >= actual_bucket_size {
            return None;
        }

        let found_ng = u64::from_be_bytes(
            bucket_data[left * 8..(left + 1) * 8].try_into().unwrap()
        );

        if found_ng != ng {
            return None;
        }

        // Calculate posting list offset
        let posting_idx = lookup.posting_index_offset + left;
        let idx_start = self.posting_index_offset as usize + posting_idx * 4;
        let idx_end = idx_start + 8; // Read two offsets

        if idx_end > self.mmap.len() {
            return None;
        }

        let start_offset = u32::from_be_bytes(
            self.mmap[idx_start..idx_start + 4].try_into().unwrap()
        );
        let end_offset = u32::from_be_bytes(
            self.mmap[idx_start + 4..idx_end].try_into().unwrap()
        );

        let posting_start = self.postings_offset as usize + start_offset as usize;
        let posting_end = self.postings_offset as usize + end_offset as usize;

        if posting_end > self.mmap.len() {
            return None;
        }

        Some(&self.mmap[posting_start..posting_end])
    }

    /// Get the rune offsets for UTF-8 mapping
    pub fn rune_offsets(&self) -> &[u32] {
        &self.rune_offsets
    }

    /// Get the end rune positions
    pub fn end_runes(&self) -> &[u32] {
        &self.end_runes
    }

    /// Check if all content is plain ASCII
    pub fn is_plain_ascii(&self) -> bool {
        self.metadata.plain_ascii
    }

    /// Find which document contains the given rune offset
    pub fn rune_to_doc(&self, rune_offset: u32) -> Option<usize> {
        // Binary search in end_runes
        match self.end_runes.binary_search(&rune_offset) {
            Ok(idx) => Some(idx),
            Err(idx) => {
                if idx < self.end_runes.len() {
                    Some(idx)
                } else {
                    None
                }
            }
        }
    }

    /// Convert a global rune offset to (doc_id, local_rune_offset)
    pub fn global_to_local_rune(&self, global_rune: u32) -> Option<(usize, u32)> {
        let doc_id = self.rune_to_doc(global_rune)?;
        let doc_start = if doc_id == 0 { 0 } else { self.end_runes[doc_id - 1] };
        let local_rune = global_rune - doc_start;
        Some((doc_id, local_rune))
    }
}

impl std::fmt::Debug for IndexData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexData")
            .field("file_count", &self.file_count())
            .field("ngram_count", &self.ngram_count())
            .field("metadata", &self.metadata)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    // Integration tests will be added when we have a way to create indexes in tests
}
