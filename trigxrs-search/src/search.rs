//! Literal and pattern search implementation
//!
//! Implements trigram-based filtering for fast substring and regex search.
//! The algorithm:
//! 1. Extract trigrams from search pattern (or literal substrings from regex)
//! 2. Select the two most selective trigrams (lowest frequency)
//! 3. Intersect posting lists using distance constraint
//! 4. Verify candidates against actual content (or compiled regex)

use std::collections::BTreeSet;

use regex_syntax::hir::{Hir, HirKind};
use trigxrs_core::{split_ngrams, NgramOffset, Ngram};

use crate::posting::{
    collect_hits, CompressedPostingIterator, DistanceIterator, HitIterator, MAX_OFFSET,
};
use crate::reader::IndexData;
use crate::error::Error;
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

/// Represents extracted literals from a regex for trigram filtering
#[derive(Debug, Clone, PartialEq, Eq)]
enum RegexFilter {
    /// All literals must appear (from Concat)
    And(Vec<String>),
    /// At least one must appear (from Alternation)
    Or(Vec<String>),
    /// One literal
    Single(String),
    /// No useful literals — scan all files
    BruteForce,
}

/// Maximum number of OR branches before falling back to brute force
const MAX_OR_BRANCHES: usize = 16;

/// Extract literal substrings from a regex HIR tree for trigram filtering
fn extract_literals_from_hir(hir: &Hir, min_len: usize) -> RegexFilter {
    match hir.kind() {
        HirKind::Literal(lit) => {
            match std::str::from_utf8(&lit.0) {
                Ok(s) if s.len() >= min_len => RegexFilter::Single(s.to_string()),
                _ => RegexFilter::BruteForce,
            }
        }
        HirKind::Concat(subs) => {
            let mut literals = Vec::new();
            for sub in subs {
                match extract_literals_from_hir(sub, min_len) {
                    RegexFilter::BruteForce => {}
                    RegexFilter::Single(s) => literals.push(s),
                    RegexFilter::And(mut v) => literals.append(&mut v),
                    RegexFilter::Or(v) => {
                        // An OR inside a concat — keep it as a single "any of these" filter
                        // For simplicity, just add the longest literal from the OR
                        if let Some(longest) = v.into_iter().max_by_key(|s| s.len()) {
                            if longest.len() >= min_len {
                                literals.push(longest);
                            }
                        }
                    }
                }
            }
            match literals.len() {
                0 => RegexFilter::BruteForce,
                1 => RegexFilter::Single(literals.into_iter().next().unwrap()),
                _ => RegexFilter::And(literals),
            }
        }
        HirKind::Alternation(subs) => {
            if subs.len() > MAX_OR_BRANCHES {
                return RegexFilter::BruteForce;
            }
            let mut literals = Vec::new();
            for sub in subs {
                match extract_literals_from_hir(sub, min_len) {
                    RegexFilter::BruteForce => return RegexFilter::BruteForce,
                    RegexFilter::Single(s) => literals.push(s),
                    RegexFilter::And(v) => {
                        // Take the longest literal from the AND
                        if let Some(longest) = v.into_iter().max_by_key(|s| s.len()) {
                            literals.push(longest);
                        } else {
                            return RegexFilter::BruteForce;
                        }
                    }
                    RegexFilter::Or(v) => {
                        // Nested OR — flatten
                        literals.extend(v);
                        if literals.len() > MAX_OR_BRANCHES {
                            return RegexFilter::BruteForce;
                        }
                    }
                }
            }
            if literals.is_empty() {
                RegexFilter::BruteForce
            } else {
                RegexFilter::Or(literals)
            }
        }
        HirKind::Capture(cap) => extract_literals_from_hir(&cap.sub, min_len),
        HirKind::Repetition(rep) if rep.min >= 1 => {
            extract_literals_from_hir(&rep.sub, min_len)
        }
        _ => RegexFilter::BruteForce,
    }
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

    /// Get candidate file indices from a RegexFilter using trigram posting lists.
    ///
    /// Returns `None` for brute force (scan all files), or `Some(file_indices)`.
    fn candidate_files_from_filter(
        &self,
        filter: &RegexFilter,
        stats: &mut SearchStats,
    ) -> Option<Vec<usize>> {
        match filter {
            RegexFilter::BruteForce => None,
            RegexFilter::Single(lit) => {
                self.file_indices_for_literal(lit, stats)
            }
            RegexFilter::And(lits) => {
                let mut result_set: Option<BTreeSet<usize>> = None;
                for lit in lits {
                    if let Some(file_indices) = self.file_indices_for_literal(lit, stats) {
                        let set: BTreeSet<usize> = file_indices.into_iter().collect();
                        result_set = Some(match result_set {
                            Some(existing) => existing.intersection(&set).copied().collect(),
                            None => set,
                        });
                    }
                    // If a literal has no trigrams (too short), skip it
                }
                result_set.map(|s| s.into_iter().collect())
            }
            RegexFilter::Or(lits) => {
                let mut union_set = BTreeSet::new();
                for lit in lits {
                    if let Some(file_indices) = self.file_indices_for_literal(lit, stats) {
                        union_set.extend(file_indices);
                    } else {
                        // If any branch can't be filtered, must scan all
                        return None;
                    }
                }
                Some(union_set.into_iter().collect())
            }
        }
    }

    /// Get file indices that contain the given literal, using trigram posting lists.
    fn file_indices_for_literal(&self, lit: &str, stats: &mut SearchStats) -> Option<Vec<usize>> {
        let ngram_offs = split_ngrams(lit);
        if ngram_offs.is_empty() {
            return None; // Too short for trigram filtering
        }

        // Use the most selective trigram
        let mut best: Option<(Ngram, usize)> = None;
        for ngo in &ngram_offs {
            let freq = self.get_posting_list_size(ngo.ngram);
            stats.ngram_lookups += 1;
            if freq == 0 {
                return Some(Vec::new()); // Trigram not found — no files match
            }
            match best {
                Some((_, best_freq)) if freq < best_freq => {
                    best = Some((ngo.ngram, freq));
                }
                None => {
                    best = Some((ngo.ngram, freq));
                }
                _ => {}
            }
        }

        let (best_ngram, _) = best?;
        let posting = self.get_posting_list(best_ngram)?;
        let mut iter = CompressedPostingIterator::new(posting);
        let hits = collect_hits(&mut iter, usize::MAX);
        stats.bytes_loaded += iter.bytes_loaded();

        let mut file_indices = BTreeSet::new();
        for &rune_offset in &hits {
            if let Some((file_idx, _)) = self.global_to_local_rune(rune_offset) {
                file_indices.insert(file_idx);
            }
        }

        Some(file_indices.into_iter().collect())
    }

    /// Search for a regex pattern in the indexed content
    ///
    /// Parses the regex, extracts literal substrings for trigram-based candidate
    /// filtering, then verifies candidates with the compiled regex.
    ///
    /// # Arguments
    /// * `pattern` - The regex pattern to search for
    /// * `case_sensitive` - Whether to match case exactly
    /// * `max_matches` - Maximum number of matches to return (0 = unlimited)
    pub fn search_regex(
        &self,
        pattern: &str,
        case_sensitive: bool,
        max_matches: usize,
    ) -> Result<SearchResult> {
        let mut stats = SearchStats::default();

        // Parse the regex HIR
        let hir = regex_syntax::parse(pattern)
            .map_err(|e| Error::RegexSyntax(e.to_string()))?;

        // If it's a pure literal, delegate to search_literal
        if let HirKind::Literal(lit) = hir.kind() {
            if let Ok(s) = std::str::from_utf8(&lit.0) {
                if case_sensitive {
                    return self.search_literal(s, true, max_matches);
                }
            }
        }

        // Extract literals for trigram filtering
        let filter = extract_literals_from_hir(&hir, 3);

        // For case-insensitive search, skip trigram filtering since the index
        // stores case-sensitive trigrams
        let candidate_files = if !case_sensitive {
            None // brute force
        } else {
            self.candidate_files_from_filter(&filter, &mut stats)
        };

        // Compile the regex
        let re = regex::RegexBuilder::new(pattern)
            .case_insensitive(!case_sensitive)
            .build()?;

        let max = if max_matches == 0 { usize::MAX } else { max_matches };
        let mut matches = Vec::new();

        // Determine which files to scan
        let file_indices: Box<dyn Iterator<Item = usize>> = match candidate_files {
            Some(indices) => Box::new(indices.into_iter()),
            None => Box::new(0..self.file_count()),
        };

        for file_idx in file_indices {
            if let Some(content) = self.file_content(file_idx) {
                // Skip non-UTF-8 files
                let text = match std::str::from_utf8(content) {
                    Ok(t) => t,
                    Err(_) => continue,
                };

                stats.candidates_checked += 1;

                for m in re.find_iter(text) {
                    let byte_offset = m.start() as u32;
                    let byte_length = m.len() as u32;
                    let line_number = self.find_line_number(file_idx, byte_offset);

                    matches.push(Match {
                        file_idx,
                        byte_offset,
                        byte_length,
                        line_number,
                    });

                    if matches.len() >= max {
                        break;
                    }
                }
            }

            if matches.len() >= max {
                break;
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

#[cfg(test)]
mod regex_unit_tests {
    use super::*;

    fn parse_and_extract(pattern: &str) -> RegexFilter {
        let hir = regex_syntax::parse(pattern).unwrap();
        extract_literals_from_hir(&hir, 3)
    }

    #[test]
    fn test_extract_pure_literal() {
        assert_eq!(parse_and_extract("hello"), RegexFilter::Single("hello".into()));
    }

    #[test]
    fn test_extract_concat_with_wildcard() {
        let filter = parse_and_extract("foo.*bar");
        assert_eq!(filter, RegexFilter::And(vec!["foo".into(), "bar".into()]));
    }

    #[test]
    fn test_extract_alternation() {
        let filter = parse_and_extract("foo|bar");
        assert_eq!(filter, RegexFilter::Or(vec!["foo".into(), "bar".into()]));
    }

    #[test]
    fn test_extract_short_literals_brute_force() {
        // "a" and "b" are both < 3 chars
        assert_eq!(parse_and_extract("a.*b"), RegexFilter::BruteForce);
    }

    #[test]
    fn test_extract_char_class_brute_force() {
        assert_eq!(parse_and_extract("[a-z]+"), RegexFilter::BruteForce);
    }

    #[test]
    fn test_extract_capture_group() {
        let filter = parse_and_extract("(foo)bar");
        // (foo)bar is a concat of capture(foo) and literal(bar)
        // Depending on how regex-syntax parses this, it may be a single literal or concat
        match filter {
            RegexFilter::Single(s) => assert_eq!(s, "foobar"),
            RegexFilter::And(v) => {
                assert!(v.contains(&"foo".to_string()));
                assert!(v.contains(&"bar".to_string()));
            }
            _ => panic!("unexpected filter: {:?}", filter),
        }
    }

    #[test]
    fn test_extract_wildcard_only_brute_force() {
        assert_eq!(parse_and_extract(".*"), RegexFilter::BruteForce);
    }

    #[test]
    fn test_extract_repetition_with_min_1() {
        // "foo+" is parsed as concat(literal("fo"), repetition(literal("o"), min=1))
        let filter = parse_and_extract("foo+");
        // Should extract something from the literal prefix
        match filter {
            RegexFilter::BruteForce => {
                // If "fo" is too short and "o" is too short, this is expected
            }
            RegexFilter::Single(s) => {
                assert!(s.len() >= 3);
            }
            RegexFilter::And(v) => {
                assert!(!v.is_empty());
            }
            _ => panic!("unexpected filter: {:?}", filter),
        }
    }

    #[test]
    fn test_extract_long_alternation() {
        let filter = parse_and_extract("hello|world|testing");
        assert_eq!(
            filter,
            RegexFilter::Or(vec!["hello".into(), "world".into(), "testing".into()])
        );
    }
}

#[cfg(test)]
mod regex_integration_tests {
    use super::*;
    use std::io::Cursor;
    use trigxrs_index::IndexBuilder;

    /// Helper to create an index and search with regex
    fn create_index_and_search_regex(
        files: &[(&str, &[u8])],
        pattern: &str,
        case_sensitive: bool,
        max_matches: usize,
    ) -> SearchResult {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut builder = IndexBuilder::new();
        for (path, content) in files {
            builder.add_file(path, content).unwrap();
        }

        let mut temp = NamedTempFile::new().unwrap();
        {
            let mut buf = Cursor::new(Vec::new());
            builder.write_shard(&mut buf).unwrap();
            temp.write_all(&buf.into_inner()).unwrap();
        }

        let index = IndexData::open(temp.path()).unwrap();
        index.search_regex(pattern, case_sensitive, max_matches).unwrap()
    }

    #[test]
    fn test_regex_literal_only() {
        let result = create_index_and_search_regex(
            &[("test.txt", b"hello world")],
            "world",
            true,
            0,
        );
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].byte_offset, 6);
        assert_eq!(result.matches[0].byte_length, 5);
    }

    #[test]
    fn test_regex_simple_alternation() {
        let result = create_index_and_search_regex(
            &[("test.txt", b"hello world")],
            "hello|world",
            true,
            0,
        );
        assert_eq!(result.matches.len(), 2);
        assert_eq!(result.matches[0].byte_offset, 0);  // "hello"
        assert_eq!(result.matches[1].byte_offset, 6);  // "world"
    }

    #[test]
    fn test_regex_wildcard_pattern() {
        let result = create_index_and_search_regex(
            &[("test.txt", b"hello world")],
            "hel.*rld",
            true,
            0,
        );
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].byte_offset, 0);
        assert_eq!(result.matches[0].byte_length, 11); // "hello world"
    }

    #[test]
    fn test_regex_character_class() {
        let result = create_index_and_search_regex(
            &[("test.txt", b"hello world")],
            "[hw]orld",
            true,
            0,
        );
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].byte_offset, 6); // "world"
        assert_eq!(result.matches[0].byte_length, 5);
    }

    #[test]
    fn test_regex_no_match() {
        let result = create_index_and_search_regex(
            &[("test.txt", b"hello world")],
            "xyz+",
            true,
            0,
        );
        assert_eq!(result.matches.len(), 0);
    }

    #[test]
    fn test_regex_multiple_files() {
        let result = create_index_and_search_regex(
            &[
                ("a.txt", b"foo bar baz"),
                ("b.txt", b"hello world"),
                ("c.txt", b"foo world"),
            ],
            "foo|world",
            true,
            0,
        );
        // Should find: foo in a.txt, world in b.txt, foo+world in c.txt
        assert!(result.matches.len() >= 3);
    }

    #[test]
    fn test_regex_max_matches() {
        let result = create_index_and_search_regex(
            &[
                ("a.txt", b"abc abc abc"),
                ("b.txt", b"abc abc abc"),
            ],
            "abc",
            true,
            3,
        );
        assert_eq!(result.matches.len(), 3);
    }

    #[test]
    fn test_regex_case_insensitive() {
        let result = create_index_and_search_regex(
            &[("test.txt", b"Hello World HELLO")],
            "hello",
            false,
            0,
        );
        assert_eq!(result.matches.len(), 2);
    }

    #[test]
    fn test_regex_brute_force_fallback() {
        // ".*" should scan all files (brute force)
        let result = create_index_and_search_regex(
            &[
                ("a.txt", b"hello"),
                ("b.txt", b"world"),
            ],
            ".*",
            true,
            0,
        );
        // ".*" matches empty string at every position, but regex find_iter
        // handles this by returning non-overlapping matches
        assert!(result.matches.len() >= 2);
    }

    #[test]
    fn test_regex_byte_offset_length_correctness() {
        let result = create_index_and_search_regex(
            &[("test.txt", b"abc 12345 xyz")],
            r"\d+",
            true,
            0,
        );
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].byte_offset, 4);
        assert_eq!(result.matches[0].byte_length, 5); // "12345"
    }

    #[test]
    fn test_regex_invalid_pattern() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut builder = IndexBuilder::new();
        builder.add_file("test.txt", b"hello").unwrap();

        let mut temp = NamedTempFile::new().unwrap();
        {
            let mut buf = Cursor::new(Vec::new());
            builder.write_shard(&mut buf).unwrap();
            temp.write_all(&buf.into_inner()).unwrap();
        }

        let index = IndexData::open(temp.path()).unwrap();
        let result = index.search_regex("[invalid", true, 0);
        assert!(result.is_err());
    }

    #[test]
    fn test_regex_with_repetition() {
        let result = create_index_and_search_regex(
            &[("test.txt", b"foooo bar fooo baz")],
            "fo{2,}",
            true,
            0,
        );
        assert_eq!(result.matches.len(), 2);
    }

    #[test]
    fn test_regex_anchored() {
        let result = create_index_and_search_regex(
            &[("test.txt", b"hello world\nhello again")],
            "^hello",
            true,
            0,
        );
        // Default regex is not multiline, so ^ matches start of string only
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].byte_offset, 0);
    }
}
