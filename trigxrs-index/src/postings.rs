//! Postings builder for collecting trigram occurrences
//!
//! The postings builder accumulates trigram positions across multiple documents.
//! Each trigram maps to a delta-encoded list of (doc_id, offset) pairs.

use std::collections::HashMap;
use trigxrs_core::{encode_varint, Ngram, MAX_VARINT_LEN};

/// Frequency to sample rune-to-byte offset mappings for UTF-8 support
pub const RUNE_OFFSET_FREQUENCY: u32 = 100;

/// Builder for accumulating trigram postings across documents
#[derive(Debug)]
pub struct PostingsBuilder {
    /// Map from ngram to delta-encoded posting list
    postings: HashMap<Ngram, Vec<u8>>,
    /// Last offset per ngram for delta encoding
    last_offsets: HashMap<Ngram, u32>,
    /// Sampled rune-to-byte offset mappings (every RUNE_OFFSET_FREQUENCY runes)
    rune_offsets: Vec<u32>,
    /// Total rune count across all documents
    rune_count: u32,
    /// End rune position for each document
    end_runes: Vec<u32>,
    /// Current byte offset (end of all content)
    end_byte: u32,
    /// Whether all content is plain ASCII
    is_plain_ascii: bool,
}

impl PostingsBuilder {
    /// Create a new postings builder
    pub fn new() -> Self {
        Self {
            postings: HashMap::new(),
            last_offsets: HashMap::new(),
            rune_offsets: Vec::new(),
            rune_count: 0,
            end_runes: Vec::new(),
            end_byte: 0,
            is_plain_ascii: true,
        }
    }

    /// Add a document's content to the postings
    ///
    /// Returns the rune offset range for this document (start_rune, end_rune).
    pub fn add_document(&mut self, content: &[u8]) -> (u32, u32) {
        let start_rune = self.rune_count;
        let content_str = String::from_utf8_lossy(content);

        let mut buf = [0u8; MAX_VARINT_LEN];
        let mut rune_index = 0u32;
        let mut byte_count = 0u32;

        // Track rune positions through the content
        let mut rune_window: [char; 3] = ['\0', '\0', '\0'];

        for ch in content_str.chars() {
            let char_len = ch.len_utf8() as u32;

            // Check for non-ASCII
            if char_len > 1 {
                self.is_plain_ascii = false;
            }

            // Sample rune-to-byte offsets
            let global_rune = self.rune_count + rune_index;
            if global_rune % RUNE_OFFSET_FREQUENCY == 0 {
                self.rune_offsets.push(self.end_byte + byte_count);
            }

            // Shift the rune window
            rune_window[0] = rune_window[1];
            rune_window[1] = rune_window[2];
            rune_window[2] = ch;

            byte_count += char_len;
            rune_index += 1;

            // Need at least 3 runes for a trigram
            if rune_index < 3 {
                continue;
            }

            // Create trigram from window
            let ng = trigxrs_core::runes_to_ngram(rune_window);

            // Delta encode the position
            let last_off = *self.last_offsets.get(&ng).unwrap_or(&0);
            let new_off = start_rune + rune_index - 2; // Position of first rune in trigram

            let delta = new_off.saturating_sub(last_off);
            let n = encode_varint(delta as u64, &mut buf);

            self.postings
                .entry(ng)
                .or_insert_with(Vec::new)
                .extend_from_slice(&buf[..n]);
            self.last_offsets.insert(ng, new_off);
        }

        self.rune_count += rune_index;
        self.end_runes.push(self.rune_count);
        self.end_byte += byte_count;

        (start_rune, self.rune_count)
    }

    /// Add a document from a string slice (convenience method)
    pub fn add_document_str(&mut self, content: &str) -> (u32, u32) {
        self.add_document(content.as_bytes())
    }

    /// Get the number of unique trigrams
    pub fn ngram_count(&self) -> usize {
        self.postings.len()
    }

    /// Get the number of documents added
    pub fn document_count(&self) -> usize {
        self.end_runes.len()
    }

    /// Get total content size in bytes
    pub fn total_bytes(&self) -> u32 {
        self.end_byte
    }

    /// Get total rune count
    pub fn total_runes(&self) -> u32 {
        self.rune_count
    }

    /// Check if all content is plain ASCII
    pub fn is_plain_ascii(&self) -> bool {
        self.is_plain_ascii
    }

    /// Get the rune offsets (for UTF-8 position mapping)
    pub fn rune_offsets(&self) -> &[u32] {
        &self.rune_offsets
    }

    /// Get the end rune positions for each document
    pub fn end_runes(&self) -> &[u32] {
        &self.end_runes
    }

    /// Finalize and return the postings data
    ///
    /// Returns (sorted_ngrams, posting_data) where posting_data[i] corresponds to sorted_ngrams[i]
    pub fn finalize(self) -> FinalizedPostings {
        let mut ngrams: Vec<Ngram> = self.postings.keys().copied().collect();
        ngrams.sort_unstable();

        let posting_data: Vec<Vec<u8>> = ngrams
            .iter()
            .map(|ng| self.postings.get(ng).cloned().unwrap_or_default())
            .collect();

        FinalizedPostings {
            ngrams,
            posting_data,
            rune_offsets: self.rune_offsets,
            end_runes: self.end_runes,
            is_plain_ascii: self.is_plain_ascii,
        }
    }

    /// Get posting list for a specific ngram (for testing)
    pub fn get_posting(&self, ng: Ngram) -> Option<&[u8]> {
        self.postings.get(&ng).map(|v| v.as_slice())
    }
}

impl Default for PostingsBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Finalized postings ready for writing to index
#[derive(Debug)]
pub struct FinalizedPostings {
    /// Sorted list of ngrams
    pub ngrams: Vec<Ngram>,
    /// Delta-encoded posting list for each ngram (parallel to ngrams)
    pub posting_data: Vec<Vec<u8>>,
    /// Sampled rune-to-byte offset mappings
    pub rune_offsets: Vec<u32>,
    /// End rune position for each document
    pub end_runes: Vec<u32>,
    /// Whether all content is plain ASCII
    pub is_plain_ascii: bool,
}

impl FinalizedPostings {
    /// Get the number of ngrams
    pub fn ngram_count(&self) -> usize {
        self.ngrams.len()
    }

    /// Get the total size of all posting lists in bytes
    pub fn posting_bytes(&self) -> usize {
        self.posting_data.iter().map(|p| p.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trigxrs_core::runes_to_ngram;

    #[test]
    fn test_empty_builder() {
        let builder = PostingsBuilder::new();
        assert_eq!(builder.ngram_count(), 0);
        assert_eq!(builder.document_count(), 0);
    }

    #[test]
    fn test_single_document() {
        let mut builder = PostingsBuilder::new();
        let (start, end) = builder.add_document_str("hello");

        assert_eq!(start, 0);
        assert_eq!(end, 5);
        assert_eq!(builder.document_count(), 1);
        // "hello" has trigrams: hel, ell, llo
        assert_eq!(builder.ngram_count(), 3);
    }

    #[test]
    fn test_multiple_documents() {
        let mut builder = PostingsBuilder::new();
        builder.add_document_str("hello");
        builder.add_document_str("world");

        assert_eq!(builder.document_count(), 2);
        assert_eq!(builder.total_runes(), 10);
    }

    #[test]
    fn test_repeated_trigram() {
        let mut builder = PostingsBuilder::new();
        // "ababab" has trigrams: aba(0), bab(1), aba(2), bab(3)
        builder.add_document_str("ababab");

        // Should have 2 unique trigrams
        assert_eq!(builder.ngram_count(), 2);

        let finalized = builder.finalize();

        // Check that we have postings for both trigrams
        let aba = runes_to_ngram(['a', 'b', 'a']);
        let bab = runes_to_ngram(['b', 'a', 'b']);

        assert!(finalized.ngrams.contains(&aba));
        assert!(finalized.ngrams.contains(&bab));
    }

    #[test]
    fn test_unicode_content() {
        let mut builder = PostingsBuilder::new();
        builder.add_document_str("日本語");

        assert!(!builder.is_plain_ascii());
        assert_eq!(builder.ngram_count(), 1); // Only one trigram
    }

    #[test]
    fn test_ascii_content() {
        let mut builder = PostingsBuilder::new();
        builder.add_document_str("hello world");

        assert!(builder.is_plain_ascii());
    }

    #[test]
    fn test_finalize() {
        let mut builder = PostingsBuilder::new();
        builder.add_document_str("abc");
        builder.add_document_str("abc"); // Same content in second doc

        let finalized = builder.finalize();

        assert_eq!(finalized.ngram_count(), 1);
        assert_eq!(finalized.end_runes.len(), 2);

        // The "abc" trigram should appear at position 0 and position 3
        let ng = runes_to_ngram(['a', 'b', 'c']);
        assert_eq!(finalized.ngrams[0], ng);
    }

    #[test]
    fn test_short_content() {
        let mut builder = PostingsBuilder::new();
        builder.add_document_str("ab"); // Too short for trigrams

        assert_eq!(builder.ngram_count(), 0);
        assert_eq!(builder.document_count(), 1);
    }

    #[test]
    fn test_ngram_sorted_order() {
        let mut builder = PostingsBuilder::new();
        builder.add_document_str("zyxwvu"); // Trigrams in reverse order

        let finalized = builder.finalize();

        // Verify ngrams are sorted
        for i in 1..finalized.ngrams.len() {
            assert!(
                finalized.ngrams[i - 1] < finalized.ngrams[i],
                "Ngrams not sorted at position {}",
                i
            );
        }
    }

    #[test]
    fn test_rune_offset_sampling() {
        let mut builder = PostingsBuilder::new();

        // Create content longer than RUNE_OFFSET_FREQUENCY
        let content: String = (0..150).map(|i| ((i % 26) as u8 + b'a') as char).collect();
        builder.add_document_str(&content);

        // Should have sampled offsets at 0, 100
        assert_eq!(builder.rune_offsets().len(), 2);
        assert_eq!(builder.rune_offsets()[0], 0);
        assert_eq!(builder.rune_offsets()[1], 100);
    }
}
