//! Literal and pattern search implementation
//!
//! Implements trigram-based filtering for fast substring search.
//! The algorithm:
//! 1. Extract trigrams from search pattern
//! 2. Select the two most selective trigrams (lowest frequency)
//! 3. Intersect posting lists using distance constraint
//! 4. Verify candidates against actual content

use trigxrs_core::{split_ngrams, NgramOffset, Ngram};

use crate::posting::{
    collect_hits, CompressedPostingIterator, DistanceIterator, HitIterator, MAX_OFFSET,
};
use crate::reader::IndexData;
use crate::Result;

/// A search match in the index
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// File index in the index
    pub file_idx: usize,
    /// Byte offset within the file where match starts
    pub byte_offset: u32,
    /// Length of the match in bytes
    pub byte_length: u32,
    /// Line number (1-indexed) if newlines available
    pub line_number: Option<u32>,
}

/// Search statistics
#[derive(Debug, Clone, Default)]
pub struct SearchStats {
    /// Number of ngram lookups performed
    pub ngram_lookups: usize,
    /// Number of candidate positions checked
    pub candidates_checked: usize,
    /// Number of matches found
    pub matches_found: usize,
    /// Bytes loaded from posting lists
    pub bytes_loaded: usize,
}

/// Result of literal search
#[derive(Debug)]
pub struct SearchResult {
    pub matches: Vec<Match>,
    pub stats: SearchStats,
}

impl IndexData {
    /// Search for a literal pattern in the indexed content
    ///
    /// Returns all matches of the pattern in indexed files.
    ///
    /// # Arguments
    /// * `pattern` - The literal string to search for
    /// * `case_sensitive` - Whether to match case exactly (case-insensitive not yet implemented)
    /// * `max_matches` - Maximum number of matches to return (0 = unlimited)
    ///
    /// # Example
    /// ```ignore
    /// let index = IndexData::open("index.zrst")?;
    /// let result = index.search_literal("fn main", true, 100)?;
    /// for m in result.matches {
    ///     println!("Found in file {} at offset {}", m.file_idx, m.byte_offset);
    /// }
    /// ```
    pub fn search_literal(
        &self,
        pattern: &str,
        case_sensitive: bool,
        max_matches: usize,
    ) -> Result<SearchResult> {
        let mut stats = SearchStats::default();

        // TODO: Implement case-insensitive search
        if !case_sensitive {
            unimplemented!("case-insensitive search not yet implemented");
        }

        // Extract trigrams from pattern
        let ngram_offs = split_ngrams(pattern);

        if ngram_offs.is_empty() {
            // Pattern too short for trigram search
            // Fall back to brute force scan
            return self.search_literal_brute_force(pattern, max_matches);
        }

        // Get posting list sizes for each trigram to find most selective
        let mut ngram_freqs: Vec<(NgramOffset, usize)> = Vec::with_capacity(ngram_offs.len());

        for ngo in &ngram_offs {
            let freq = self.get_posting_list_size(ngo.ngram);
            stats.ngram_lookups += 1;

            if freq == 0 {
                // Trigram not found - pattern can't exist
                return Ok(SearchResult {
                    matches: Vec::new(),
                    stats,
                });
            }

            ngram_freqs.push((*ngo, freq));
        }

        // Find the two most selective (lowest frequency) trigrams
        let (first, last) = find_selective_ngrams(&ngram_freqs);

        // Calculate pattern length in runes (for padding calculation)
        let pattern_runes = pattern.chars().count() as u32;
        let pattern_bytes = pattern.len() as u32;

        // Get candidate positions from posting lists
        let candidates = if first.index == last.index {
            // Only one unique trigram, or they're the same
            let posting = self.get_posting_list(first.ngram);
            match posting {
                Some(data) => {
                    let mut iter = CompressedPostingIterator::new(data);
                    let max = if max_matches == 0 { usize::MAX } else { max_matches * 10 };
                    let hits = collect_hits(&mut iter, max);
                    stats.bytes_loaded += iter.bytes_loaded();
                    hits
                }
                None => Vec::new(),
            }
        } else {
            // Two different trigrams - use distance iterator
            let dist = (last.index - first.index) as u32;

            let posting1 = self.get_posting_list(first.ngram);
            let posting2 = self.get_posting_list(last.ngram);

            match (posting1, posting2) {
                (Some(data1), Some(data2)) => {
                    let iter1 = CompressedPostingIterator::new(data1);
                    let iter2 = CompressedPostingIterator::new(data2);
                    let mut dist_iter = DistanceIterator::new(iter1, iter2, dist);
                    let max = if max_matches == 0 { usize::MAX } else { max_matches * 10 };
                    let hits = collect_hits(&mut dist_iter, max);
                    stats.bytes_loaded += dist_iter.bytes_loaded();
                    hits
                }
                _ => Vec::new(),
            }
        };

        // Verify candidates against actual content
        let pattern_bytes_slice = pattern.as_bytes();
        let mut matches = Vec::new();
        let max = if max_matches == 0 { usize::MAX } else { max_matches };

        for &rune_offset in &candidates {
            stats.candidates_checked += 1;

            // Convert rune offset to byte offset and verify
            if let Some(m) = self.verify_match(
                rune_offset,
                first.index,
                pattern_bytes_slice,
                pattern_bytes,
                pattern_runes,
            ) {
                matches.push(m);
                if matches.len() >= max {
                    break;
                }
            }
        }

        stats.matches_found = matches.len();

        Ok(SearchResult { matches, stats })
    }

    /// Get the approximate size of a posting list (for selectivity)
    fn get_posting_list_size(&self, ng: Ngram) -> usize {
        match self.get_posting_list(ng) {
            Some(data) => {
                // Count actual entries by iterating
                let mut iter = CompressedPostingIterator::new(data);
                let mut count = 0;
                while iter.first() != MAX_OFFSET {
                    count += 1;
                    iter.next(iter.first());
                }
                count
            }
            None => 0,
        }
    }

    /// Verify a candidate match at the given rune offset
    fn verify_match(
        &self,
        candidate_rune: u32,
        first_ngram_index: u32,
        pattern: &[u8],
        pattern_byte_len: u32,
        _pattern_rune_len: u32,
    ) -> Option<Match> {
        // The candidate rune offset is where the first selected trigram starts.
        // Adjust back to where the pattern starts.
        let pattern_start_rune = candidate_rune.saturating_sub(first_ngram_index);

        // Find which document contains this rune
        let (file_idx, local_rune) = self.global_to_local_rune(pattern_start_rune)?;

        // Get file content
        let content = self.file_content(file_idx)?;

        // Convert local rune offset to byte offset
        let byte_offset = if self.is_plain_ascii() {
            // For ASCII, rune offset == byte offset
            local_rune as usize
        } else {
            // For Unicode, count bytes up to the rune
            rune_to_byte_offset(content, local_rune as usize)?
        };

        // Check bounds
        let end = byte_offset + pattern_byte_len as usize;
        if end > content.len() {
            return None;
        }

        // Compare bytes
        if &content[byte_offset..end] == pattern {
            // Find line number if newlines are available
            let line_number = self.find_line_number(file_idx, byte_offset as u32);

            Some(Match {
                file_idx,
                byte_offset: byte_offset as u32,
                byte_length: pattern_byte_len,
                line_number,
            })
        } else {
            None
        }
    }

    /// Find line number for a byte offset in a file
    fn find_line_number(&self, file_idx: usize, byte_offset: u32) -> Option<u32> {
        let file = self.files().get(file_idx)?;
        if file.newlines.is_empty() {
            return None;
        }

        // Binary search for the line
        match file.newlines.binary_search(&byte_offset) {
            Ok(idx) => Some((idx + 1) as u32),
            Err(idx) => Some((idx + 1) as u32),
        }
    }

    /// Brute force search for patterns shorter than 3 characters
    fn search_literal_brute_force(
        &self,
        pattern: &str,
        max_matches: usize,
    ) -> Result<SearchResult> {
        let pattern_bytes = pattern.as_bytes();
        let mut matches = Vec::new();
        let mut stats = SearchStats::default();
        let max = if max_matches == 0 { usize::MAX } else { max_matches };

        for (file_idx, _file) in self.files().iter().enumerate() {
            if let Some(content) = self.file_content(file_idx) {
                // Find all occurrences in this file
                let mut pos = 0;
                while let Some(offset) = find_bytes(&content[pos..], pattern_bytes) {
                    let byte_offset = (pos + offset) as u32;
                    let line_number = self.find_line_number(file_idx, byte_offset);

                    matches.push(Match {
                        file_idx,
                        byte_offset,
                        byte_length: pattern_bytes.len() as u32,
                        line_number,
                    });

                    if matches.len() >= max {
                        break;
                    }

                    pos += offset + 1;
                }

                if matches.len() >= max {
                    break;
                }
            }
        }

        stats.matches_found = matches.len();
        Ok(SearchResult { matches, stats })
    }
}

/// Find two most selective trigrams (lowest frequency)
///
/// Returns (first, last) where first has a lower or equal index than last.
/// The selected trigrams are the two with lowest posting list frequency,
/// which maximizes intersection selectivity.
fn find_selective_ngrams(ngram_freqs: &[(NgramOffset, usize)]) -> (NgramOffset, NgramOffset) {
    if ngram_freqs.is_empty() {
        panic!("find_selective_ngrams called with empty list");
    }

    if ngram_freqs.len() == 1 {
        return (ngram_freqs[0].0, ngram_freqs[0].0);
    }

    // Find two lowest frequency ngrams
    let mut first_idx = 0;
    let mut second_idx = 1;
    let mut first_freq = ngram_freqs[0].1;
    let mut second_freq = ngram_freqs[1].1;

    if second_freq < first_freq {
        std::mem::swap(&mut first_idx, &mut second_idx);
        std::mem::swap(&mut first_freq, &mut second_freq);
    }

    for (i, &(_, freq)) in ngram_freqs.iter().enumerate().skip(2) {
        if freq < first_freq {
            second_idx = first_idx;
            second_freq = first_freq;
            first_idx = i;
            first_freq = freq;
        } else if freq < second_freq {
            second_idx = i;
            second_freq = freq;
        }
    }

    let mut first = ngram_freqs[first_idx].0;
    let mut last = ngram_freqs[second_idx].0;

    // Ensure first comes before last by index (for DistanceIterator)
    if first.index > last.index {
        std::mem::swap(&mut first, &mut last);
    }

    (first, last)
}

/// Convert rune offset to byte offset in UTF-8 content
fn rune_to_byte_offset(content: &[u8], rune_offset: usize) -> Option<usize> {
    let s = std::str::from_utf8(content).ok()?;
    let mut byte_pos = 0;
    for (i, ch) in s.chars().enumerate() {
        if i == rune_offset {
            return Some(byte_pos);
        }
        byte_pos += ch.len_utf8();
    }
    // If rune_offset == number of chars, return end position
    if rune_offset == s.chars().count() {
        return Some(byte_pos);
    }
    None
}

/// Find bytes in a slice (like memmem)
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }

    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_selective_ngrams_single() {
        let ngrams = split_ngrams("abc");
        let freqs: Vec<_> = ngrams.iter().map(|n| (*n, 10)).collect();
        let (first, last) = find_selective_ngrams(&freqs);
        assert_eq!(first.index, 0);
        assert_eq!(last.index, 0);
    }

    #[test]
    fn test_find_selective_ngrams_multiple() {
        let ngrams = split_ngrams("abcdef");
        // abc=0, bcd=1, cde=2, def=3
        let freqs: Vec<_> = ngrams
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let freq = match i {
                    0 => 100,  // abc - high
                    1 => 5,    // bcd - lowest
                    2 => 10,   // cde - second lowest
                    3 => 50,   // def - medium
                    _ => 100,
                };
                (*n, freq)
            })
            .collect();

        let (first, last) = find_selective_ngrams(&freqs);
        // Should pick bcd (idx 1, freq 5) and cde (idx 2, freq 10)
        assert_eq!(first.index, 1); // bcd comes first
        assert_eq!(last.index, 2);  // cde comes second
    }

    #[test]
    fn test_find_bytes() {
        assert_eq!(find_bytes(b"hello world", b"world"), Some(6));
        assert_eq!(find_bytes(b"hello world", b"xyz"), None);
        assert_eq!(find_bytes(b"aaa", b"aa"), Some(0));
        assert_eq!(find_bytes(b"", b"a"), None);
        assert_eq!(find_bytes(b"a", b""), Some(0));
    }

    #[test]
    fn test_rune_to_byte_offset_ascii() {
        let content = b"hello";
        assert_eq!(rune_to_byte_offset(content, 0), Some(0));
        assert_eq!(rune_to_byte_offset(content, 2), Some(2));
        assert_eq!(rune_to_byte_offset(content, 5), Some(5));
    }

    #[test]
    fn test_rune_to_byte_offset_unicode() {
        let content = "日本語".as_bytes();
        // Each Japanese char is 3 bytes
        assert_eq!(rune_to_byte_offset(content, 0), Some(0));
        assert_eq!(rune_to_byte_offset(content, 1), Some(3));
        assert_eq!(rune_to_byte_offset(content, 2), Some(6));
        assert_eq!(rune_to_byte_offset(content, 3), Some(9));
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use std::io::Cursor;
    use trigxrs_index::IndexBuilder;

    /// Helper to create an index from content and search it
    fn create_index_and_search(files: &[(&str, &[u8])], pattern: &str) -> SearchResult {
        use std::io::Write;
        use tempfile::NamedTempFile;

        // Build the index
        let mut builder = IndexBuilder::new();
        for (path, content) in files {
            builder.add_file(path, content).unwrap();
        }

        // Write to a temp file
        let mut temp = NamedTempFile::new().unwrap();
        {
            let mut buf = Cursor::new(Vec::new());
            builder.write_shard(&mut buf).unwrap();
            temp.write_all(&buf.into_inner()).unwrap();
        }

        // Open and search
        let index = IndexData::open(temp.path()).unwrap();
        index.search_literal(pattern, true, 0).unwrap()
    }

    #[test]
    fn test_debug_posting_list_retrieval() {
        use std::io::Write;
        use tempfile::NamedTempFile;
        use trigxrs_core::split_ngrams;

        let content = b"hello world";

        // Build the index
        let mut builder = IndexBuilder::new();
        builder.add_file("test.txt", content).unwrap();

        let mut temp = NamedTempFile::new().unwrap();
        {
            let mut buf = Cursor::new(Vec::new());
            builder.write_shard(&mut buf).unwrap();
            temp.write_all(&buf.into_inner()).unwrap();
        }

        let index = IndexData::open(temp.path()).unwrap();

        // Check basic index info
        eprintln!("Index file count: {}", index.file_count());
        eprintln!("Index ngram count: {}", index.ngram_count());
        eprintln!("Index metadata: {:?}", index.metadata());

        // Check file content retrieval
        let file_content = index.file_content(0);
        eprintln!("File content: {:?}", file_content.map(|c| std::str::from_utf8(c)));

        // Check trigrams from pattern
        let pattern = "world";
        let ngrams = split_ngrams(pattern);
        eprintln!("Pattern '{}' trigrams: {}", pattern, ngrams.len());
        for ngo in &ngrams {
            eprintln!("  Trigram at index {}: 0x{:016x}", ngo.index, ngo.ngram);
            let posting = index.get_posting_list(ngo.ngram);
            eprintln!("    Posting list: {:?}", posting.map(|p| p.len()));
        }

        // Check end_runes
        eprintln!("End runes: {:?}", index.end_runes());

        // Now try the actual search
        let result = index.search_literal(pattern, true, 0).unwrap();
        eprintln!("Search result: {} matches, stats: {:?}", result.matches.len(), result.stats);

        assert!(result.matches.len() > 0, "Should find at least one match");
    }

    #[test]
    fn test_search_single_file_single_match() {
        let result = create_index_and_search(
            &[("test.txt", b"hello world")],
            "world",
        );

        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].file_idx, 0);
        assert_eq!(result.matches[0].byte_offset, 6);
        assert_eq!(result.matches[0].byte_length, 5);
    }

    #[test]
    fn test_search_single_file_multiple_matches() {
        let result = create_index_and_search(
            &[("test.txt", b"abc abc abc")],
            "abc",
        );

        assert_eq!(result.matches.len(), 3);
        assert_eq!(result.matches[0].byte_offset, 0);
        assert_eq!(result.matches[1].byte_offset, 4);
        assert_eq!(result.matches[2].byte_offset, 8);
    }

    #[test]
    fn test_search_multiple_files() {
        let result = create_index_and_search(
            &[
                ("a.txt", b"hello world"),
                ("b.txt", b"goodbye world"),
            ],
            "world",
        );

        assert_eq!(result.matches.len(), 2);
        // Matches should be in file order
        assert_eq!(result.matches[0].file_idx, 0);
        assert_eq!(result.matches[1].file_idx, 1);
    }

    #[test]
    fn test_search_no_match() {
        let result = create_index_and_search(
            &[("test.txt", b"hello world")],
            "xyz",
        );

        assert_eq!(result.matches.len(), 0);
    }

    #[test]
    fn test_search_long_pattern() {
        // "fn main() { println!(\"Hello\"); }"
        //  0         1
        //  0123456789012345...
        //              ^ println starts at offset 12
        let result = create_index_and_search(
            &[("test.txt", b"fn main() { println!(\"Hello\"); }")],
            "println",
        );

        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].byte_offset, 12);
    }

    #[test]
    fn test_search_short_pattern_brute_force() {
        // Pattern "ab" is too short for trigrams, should use brute force
        let result = create_index_and_search(
            &[("test.txt", b"ab ab ab")],
            "ab",
        );

        assert_eq!(result.matches.len(), 3);
    }

    #[test]
    fn test_search_pattern_at_boundaries() {
        let result = create_index_and_search(
            &[("test.txt", b"abc123abc")],
            "abc",
        );

        assert_eq!(result.matches.len(), 2);
        assert_eq!(result.matches[0].byte_offset, 0);
        assert_eq!(result.matches[1].byte_offset, 6);
    }

    #[test]
    fn test_search_max_matches() {
        use tempfile::NamedTempFile;
        use std::io::Write;

        let content = b"abc abc abc abc abc";
        let mut builder = IndexBuilder::new();
        builder.add_file("test.txt", content).unwrap();

        let mut temp = NamedTempFile::new().unwrap();
        {
            let mut buf = Cursor::new(Vec::new());
            builder.write_shard(&mut buf).unwrap();
            temp.write_all(&buf.into_inner()).unwrap();
        }

        let index = IndexData::open(temp.path()).unwrap();
        let result = index.search_literal("abc", true, 2).unwrap();

        assert_eq!(result.matches.len(), 2);
    }
}
