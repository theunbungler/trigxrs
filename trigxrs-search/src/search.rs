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
use trigxrs_core::{split_ngrams, generate_case_ngrams, NgramOffset, Ngram};

use crate::posting::{
    collect_hits, CompressedPostingIterator, DistanceIterator, HitIterator, InMemoryIterator,
    MAX_OFFSET,
};
use crate::reader::IndexData;
use crate::error::Error;
use crate::Result;

/// Search options for controlling search behavior
#[derive(Debug, Clone)]
pub struct SearchOptions {
    /// Whether to match case exactly (default: true)
    pub case_sensitive: bool,
    /// Maximum number of matches to return (0 = unlimited)
    pub max_matches: usize,
    /// Number of context lines before/after each match (0 = no context)
    pub context_lines: usize,
    /// File path glob pattern (None = all files)
    pub file_pattern: Option<glob::Pattern>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            case_sensitive: true,
            max_matches: 0,
            context_lines: 0,
            file_pattern: None,
        }
    }
}

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

/// Context lines around a match
#[derive(Debug, Clone)]
pub struct MatchContext {
    /// The match itself
    pub m: Match,
    /// The matched line text
    pub line_text: String,
    /// Context lines before the match
    pub before: Vec<String>,
    /// Context lines after the match
    pub after: Vec<String>,
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
    /// Whether trigram filtering was used (vs brute force)
    pub used_trigram_filtering: bool,
}

/// Result of a search
#[derive(Debug)]
pub struct SearchResult {
    pub matches: Vec<Match>,
    /// Populated only when context_lines > 0
    pub contexts: Vec<MatchContext>,
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
    // ── Public API: opts variants ────────────────────────────────────────

    /// Search for a literal pattern with full options.
    pub fn search_literal_opts(
        &self,
        pattern: &str,
        opts: &SearchOptions,
    ) -> Result<SearchResult> {
        let mut stats = SearchStats::default();
        let max_matches = opts.max_matches;

        let matches = if !opts.case_sensitive {
            self.search_literal_case_insensitive(pattern, max_matches, &opts.file_pattern, &mut stats)?
        } else {
            self.search_literal_case_sensitive(pattern, max_matches, &opts.file_pattern, &mut stats)?
        };

        stats.matches_found = matches.len();

        let contexts = if opts.context_lines > 0 {
            matches.iter().filter_map(|m| self.extract_context(m, opts.context_lines)).collect()
        } else {
            Vec::new()
        };

        Ok(SearchResult { matches, contexts, stats })
    }

    /// Search for a regex pattern with full options.
    pub fn search_regex_opts(
        &self,
        pattern: &str,
        opts: &SearchOptions,
    ) -> Result<SearchResult> {
        let mut stats = SearchStats::default();
        let case_sensitive = opts.case_sensitive;
        let max_matches = opts.max_matches;

        // Parse the regex HIR
        let hir = regex_syntax::parse(pattern)
            .map_err(|e| Error::RegexSyntax(e.to_string()))?;

        // If it's a pure literal, delegate to literal search
        if let HirKind::Literal(lit) = hir.kind() {
            if let Ok(s) = std::str::from_utf8(&lit.0) {
                return self.search_literal_opts(s, opts);
            }
        }

        // Extract literals for trigram filtering
        let filter = extract_literals_from_hir(&hir, 3);

        // For case-insensitive, use case-variant trigram expansion
        let candidate_files = if !case_sensitive {
            let lowered_filter = lowercase_filter(&filter);
            let cf = self.candidate_files_from_filter_case_insensitive(&lowered_filter, &mut stats);
            if cf.is_some() {
                stats.used_trigram_filtering = true;
            }
            cf
        } else {
            let cf = self.candidate_files_from_filter(&filter, &mut stats);
            if cf.is_some() {
                stats.used_trigram_filtering = true;
            }
            cf
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
            // Apply file filter
            if let Some(ref pat) = opts.file_pattern {
                if let Some(file) = self.files().get(file_idx) {
                    if !pat.matches(&file.path) {
                        continue;
                    }
                }
            }

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

        let contexts = if opts.context_lines > 0 {
            matches.iter().filter_map(|m| self.extract_context(m, opts.context_lines)).collect()
        } else {
            Vec::new()
        };

        Ok(SearchResult { matches, contexts, stats })
    }

    // ── Legacy API: thin wrappers ───────────────────────────────────────

    /// Search for a literal pattern in the indexed content (legacy API).
    ///
    /// For full control, use `search_literal_opts` instead.
    pub fn search_literal(
        &self,
        pattern: &str,
        case_sensitive: bool,
        max_matches: usize,
    ) -> Result<SearchResult> {
        let opts = SearchOptions {
            case_sensitive,
            max_matches,
            ..Default::default()
        };
        self.search_literal_opts(pattern, &opts)
    }

    /// Search for a regex pattern in the indexed content (legacy API).
    ///
    /// For full control, use `search_regex_opts` instead.
    pub fn search_regex(
        &self,
        pattern: &str,
        case_sensitive: bool,
        max_matches: usize,
    ) -> Result<SearchResult> {
        let opts = SearchOptions {
            case_sensitive,
            max_matches,
            ..Default::default()
        };
        self.search_regex_opts(pattern, &opts)
    }

    // ── Context extraction ──────────────────────────────────────────────

    /// Extract context lines around a match.
    fn extract_context(&self, m: &Match, context_lines: usize) -> Option<MatchContext> {
        let content = self.file_content(m.file_idx)?;
        let text = std::str::from_utf8(content).ok()?;
        let lines: Vec<&str> = text.lines().collect();

        // line_number is 1-indexed; convert to 0-indexed
        let match_line_0 = m.line_number.unwrap_or(1).saturating_sub(1) as usize;
        if match_line_0 >= lines.len() {
            return None;
        }

        let line_text = lines[match_line_0].to_string();

        let ctx_start = match_line_0.saturating_sub(context_lines);
        let ctx_end = (match_line_0 + context_lines).min(lines.len().saturating_sub(1));

        let before: Vec<String> = (ctx_start..match_line_0)
            .map(|i| lines[i].to_string())
            .collect();
        let after: Vec<String> = ((match_line_0 + 1)..=ctx_end)
            .map(|i| lines[i].to_string())
            .collect();

        Some(MatchContext {
            m: m.clone(),
            line_text,
            before,
            after,
        })
    }

    // ── Case-sensitive literal search ───────────────────────────────────

    fn search_literal_case_sensitive(
        &self,
        pattern: &str,
        max_matches: usize,
        file_pattern: &Option<glob::Pattern>,
        stats: &mut SearchStats,
    ) -> Result<Vec<Match>> {
        // Extract trigrams from pattern
        let ngram_offs = split_ngrams(pattern);

        if ngram_offs.is_empty() {
            return self.search_literal_brute_force(pattern, max_matches, file_pattern, true, stats);
        }

        // Get posting list sizes for each trigram to find most selective
        let mut ngram_freqs: Vec<(NgramOffset, usize)> = Vec::with_capacity(ngram_offs.len());

        for ngo in &ngram_offs {
            let freq = self.get_posting_list_size(ngo.ngram);
            stats.ngram_lookups += 1;

            if freq == 0 {
                return Ok(Vec::new());
            }

            ngram_freqs.push((*ngo, freq));
        }

        stats.used_trigram_filtering = true;

        let (first, last) = find_selective_ngrams(&ngram_freqs);

        let pattern_runes = pattern.chars().count() as u32;
        let pattern_bytes = pattern.len() as u32;

        let candidates = self.get_candidates_from_posting_lists(
            first, last, max_matches, stats,
        );

        // Verify candidates against actual content
        let pattern_bytes_slice = pattern.as_bytes();
        let mut matches = Vec::new();
        let max = if max_matches == 0 { usize::MAX } else { max_matches };

        for &rune_offset in &candidates {
            stats.candidates_checked += 1;

            if let Some(m) = self.verify_match(
                rune_offset,
                first.index,
                pattern_bytes_slice,
                pattern_bytes,
                pattern_runes,
            ) {
                // Apply file filter
                if let Some(pat) = file_pattern {
                    if let Some(file) = self.files().get(m.file_idx) {
                        if !pat.matches(&file.path) {
                            continue;
                        }
                    }
                }
                matches.push(m);
                if matches.len() >= max {
                    break;
                }
            }
        }

        Ok(matches)
    }

    // ── Case-insensitive literal search ─────────────────────────────────

    fn search_literal_case_insensitive(
        &self,
        pattern: &str,
        max_matches: usize,
        file_pattern: &Option<glob::Pattern>,
        stats: &mut SearchStats,
    ) -> Result<Vec<Match>> {
        let ngram_offs = split_ngrams(pattern);

        if ngram_offs.is_empty() {
            return self.search_literal_brute_force(pattern, max_matches, file_pattern, false, stats);
        }

        // For each pattern trigram, generate all case variants and union their posting lists
        let mut variant_sets: Vec<(NgramOffset, Vec<u32>)> = Vec::with_capacity(ngram_offs.len());

        for ngo in &ngram_offs {
            let case_ngrams = generate_case_ngrams(ngo.ngram);
            let mut all_hits = BTreeSet::new();
            let mut any_found = false;

            for &cng in &case_ngrams {
                stats.ngram_lookups += 1;
                if let Some(data) = self.get_posting_list(cng) {
                    any_found = true;
                    let mut iter = CompressedPostingIterator::new(data);
                    let hits = collect_hits(&mut iter, usize::MAX);
                    stats.bytes_loaded += iter.bytes_loaded();
                    all_hits.extend(hits);
                }
            }

            if !any_found {
                // None of the case variants exist — no matches possible
                return Ok(Vec::new());
            }

            variant_sets.push((*ngo, all_hits.into_iter().collect()));
        }

        stats.used_trigram_filtering = true;

        // Pick the two variant-sets with smallest combined posting size
        let mut sets_by_size: Vec<(usize, usize)> = variant_sets
            .iter()
            .enumerate()
            .map(|(i, (_, hits))| (i, hits.len()))
            .collect();
        sets_by_size.sort_by_key(|&(_, size)| size);

        let first_set_idx = sets_by_size[0].0;
        let last_set_idx = if sets_by_size.len() > 1 {
            sets_by_size[1].0
        } else {
            first_set_idx
        };

        // Ensure ordering by ngram index
        let (first_set_idx, last_set_idx) = if variant_sets[first_set_idx].0.index
            <= variant_sets[last_set_idx].0.index
        {
            (first_set_idx, last_set_idx)
        } else {
            (last_set_idx, first_set_idx)
        };

        let first_ngo = variant_sets[first_set_idx].0;
        let last_ngo = variant_sets[last_set_idx].0;

        // Intersect with distance constraint using in-memory iterators
        let candidates = if first_set_idx == last_set_idx {
            variant_sets[first_set_idx].1.clone()
        } else {
            let dist = (last_ngo.index - first_ngo.index) as u32;
            let first_hits = &variant_sets[first_set_idx].1;
            let last_hits = &variant_sets[last_set_idx].1;
            let iter1 = InMemoryIterator::new(first_hits);
            let iter2 = InMemoryIterator::new(last_hits);
            let mut dist_iter = DistanceIterator::new(iter1, iter2, dist);
            collect_hits(&mut dist_iter, if max_matches == 0 { usize::MAX } else { max_matches * 10 })
        };

        // Verify candidates with case-insensitive comparison
        let pattern_lower = pattern.to_lowercase();
        let pattern_byte_len = pattern.len() as u32;
        let max = if max_matches == 0 { usize::MAX } else { max_matches };
        let mut matches = Vec::new();

        for &rune_offset in &candidates {
            stats.candidates_checked += 1;

            if let Some(m) = self.verify_match_case_insensitive(
                rune_offset,
                first_ngo.index,
                &pattern_lower,
                pattern_byte_len,
            ) {
                if let Some(pat) = file_pattern {
                    if let Some(file) = self.files().get(m.file_idx) {
                        if !pat.matches(&file.path) {
                            continue;
                        }
                    }
                }
                matches.push(m);
                if matches.len() >= max {
                    break;
                }
            }
        }

        Ok(matches)
    }

    /// Verify a candidate match with case-insensitive comparison
    fn verify_match_case_insensitive(
        &self,
        candidate_rune: u32,
        first_ngram_index: u32,
        pattern_lower: &str,
        _pattern_byte_len: u32,
    ) -> Option<Match> {
        let pattern_start_rune = candidate_rune.saturating_sub(first_ngram_index);
        let (file_idx, local_rune) = self.global_to_local_rune(pattern_start_rune)?;
        let content = self.file_content(file_idx)?;

        let byte_offset = if self.is_plain_ascii() {
            local_rune as usize
        } else {
            rune_to_byte_offset(content, local_rune as usize)?
        };

        // For case-insensitive, the match length in bytes may differ from the pattern length
        // (e.g., 'ß' lowercases to 'ss'). We need to find the right span.
        // Use the pattern's char count to extract the same number of chars from content.
        let text = std::str::from_utf8(content).ok()?;
        let text_from_offset = &text[byte_offset..];
        let pattern_char_count = pattern_lower.chars().count();

        let mut end_byte = byte_offset;
        for (i, ch) in text_from_offset.chars().enumerate() {
            if i >= pattern_char_count {
                break;
            }
            end_byte += ch.len_utf8();
        }

        if end_byte > content.len() {
            return None;
        }

        let candidate_text = &text[byte_offset..end_byte];
        if candidate_text.to_lowercase() == *pattern_lower {
            let line_number = self.find_line_number(file_idx, byte_offset as u32);
            Some(Match {
                file_idx,
                byte_offset: byte_offset as u32,
                byte_length: (end_byte - byte_offset) as u32,
                line_number,
            })
        } else {
            None
        }
    }

    // ── Case-insensitive trigram filtering for regex ─────────────────────

    /// Get candidate files using case-variant trigram expansion.
    fn candidate_files_from_filter_case_insensitive(
        &self,
        filter: &RegexFilter,
        stats: &mut SearchStats,
    ) -> Option<Vec<usize>> {
        match filter {
            RegexFilter::BruteForce => None,
            RegexFilter::Single(lit) => {
                self.file_indices_for_literal_case_insensitive(lit, stats)
            }
            RegexFilter::And(lits) => {
                let mut result_set: Option<BTreeSet<usize>> = None;
                for lit in lits {
                    if let Some(file_indices) = self.file_indices_for_literal_case_insensitive(lit, stats) {
                        let set: BTreeSet<usize> = file_indices.into_iter().collect();
                        result_set = Some(match result_set {
                            Some(existing) => existing.intersection(&set).copied().collect(),
                            None => set,
                        });
                    }
                }
                result_set.map(|s| s.into_iter().collect())
            }
            RegexFilter::Or(lits) => {
                let mut union_set = BTreeSet::new();
                for lit in lits {
                    if let Some(file_indices) = self.file_indices_for_literal_case_insensitive(lit, stats) {
                        union_set.extend(file_indices);
                    } else {
                        return None;
                    }
                }
                Some(union_set.into_iter().collect())
            }
        }
    }

    /// Get file indices containing a literal using case-variant trigrams.
    fn file_indices_for_literal_case_insensitive(
        &self,
        lit: &str,
        stats: &mut SearchStats,
    ) -> Option<Vec<usize>> {
        let ngram_offs = split_ngrams(lit);
        if ngram_offs.is_empty() {
            return None;
        }

        // Find the trigram with the smallest combined case-variant posting size
        let mut best: Option<(Vec<u32>, usize)> = None;

        for ngo in &ngram_offs {
            let case_ngrams = generate_case_ngrams(ngo.ngram);
            let mut all_hits = Vec::new();

            for &cng in &case_ngrams {
                stats.ngram_lookups += 1;
                if let Some(data) = self.get_posting_list(cng) {
                    let mut iter = CompressedPostingIterator::new(data);
                    let hits = collect_hits(&mut iter, usize::MAX);
                    stats.bytes_loaded += iter.bytes_loaded();
                    all_hits.extend(hits);
                }
            }

            if all_hits.is_empty() {
                return Some(Vec::new());
            }

            all_hits.sort_unstable();
            all_hits.dedup();

            match best {
                Some((_, best_size)) if all_hits.len() < best_size => {
                    best = Some((all_hits.clone(), all_hits.len()));
                }
                None => {
                    best = Some((all_hits.clone(), all_hits.len()));
                }
                _ => {}
            }
        }

        let (hits, _) = best?;
        let mut file_indices = BTreeSet::new();
        for &rune_offset in &hits {
            if let Some((file_idx, _)) = self.global_to_local_rune(rune_offset) {
                file_indices.insert(file_idx);
            }
        }

        Some(file_indices.into_iter().collect())
    }

    // ── Internal helpers ────────────────────────────────────────────────

    /// Get the approximate size of a posting list (for selectivity)
    fn get_posting_list_size(&self, ng: Ngram) -> usize {
        match self.get_posting_list(ng) {
            Some(data) => {
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

    /// Verify a candidate match at the given rune offset (case-sensitive)
    fn verify_match(
        &self,
        candidate_rune: u32,
        first_ngram_index: u32,
        pattern: &[u8],
        pattern_byte_len: u32,
        _pattern_rune_len: u32,
    ) -> Option<Match> {
        let pattern_start_rune = candidate_rune.saturating_sub(first_ngram_index);
        let (file_idx, local_rune) = self.global_to_local_rune(pattern_start_rune)?;
        let content = self.file_content(file_idx)?;

        let byte_offset = if self.is_plain_ascii() {
            local_rune as usize
        } else {
            rune_to_byte_offset(content, local_rune as usize)?
        };

        let end = byte_offset + pattern_byte_len as usize;
        if end > content.len() {
            return None;
        }

        if &content[byte_offset..end] == pattern {
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

        match file.newlines.binary_search(&byte_offset) {
            Ok(idx) => Some((idx + 1) as u32),
            Err(idx) => Some((idx + 1) as u32),
        }
    }

    /// Get candidates from posting lists using the two most selective trigrams.
    fn get_candidates_from_posting_lists(
        &self,
        first: NgramOffset,
        last: NgramOffset,
        max_matches: usize,
        stats: &mut SearchStats,
    ) -> Vec<u32> {
        if first.index == last.index {
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
        }
    }

    /// Brute force search for short patterns or case-insensitive short patterns.
    fn search_literal_brute_force(
        &self,
        pattern: &str,
        max_matches: usize,
        file_pattern: &Option<glob::Pattern>,
        case_sensitive: bool,
        _stats: &mut SearchStats,
    ) -> Result<Vec<Match>> {
        let pattern_bytes = pattern.as_bytes();
        let pattern_lower = pattern.to_lowercase();
        let mut matches = Vec::new();
        let max = if max_matches == 0 { usize::MAX } else { max_matches };

        for (file_idx, file) in self.files().iter().enumerate() {
            if let Some(pat) = file_pattern {
                if !pat.matches(&file.path) {
                    continue;
                }
            }

            if let Some(content) = self.file_content(file_idx) {
                if case_sensitive {
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
                } else {
                    // Case-insensitive brute force: compare lowercased
                    if let Ok(text) = std::str::from_utf8(content) {
                        let text_lower = text.to_lowercase();
                        let mut pos = 0;
                        while let Some(offset) = text_lower[pos..].find(&*pattern_lower) {
                            // Map back to original byte offset
                            let byte_offset = (pos + offset) as u32;
                            let match_len = pattern_lower.len() as u32;
                            let line_number = self.find_line_number(file_idx, byte_offset);

                            matches.push(Match {
                                file_idx,
                                byte_offset,
                                byte_length: match_len,
                                line_number,
                            });

                            if matches.len() >= max {
                                break;
                            }
                            pos += offset + 1;
                        }
                    }
                }

                if matches.len() >= max {
                    break;
                }
            }
        }

        Ok(matches)
    }

    /// Get candidate file indices from a RegexFilter using trigram posting lists.
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
                }
                result_set.map(|s| s.into_iter().collect())
            }
            RegexFilter::Or(lits) => {
                let mut union_set = BTreeSet::new();
                for lit in lits {
                    if let Some(file_indices) = self.file_indices_for_literal(lit, stats) {
                        union_set.extend(file_indices);
                    } else {
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
            return None;
        }

        let mut best: Option<(Ngram, usize)> = None;
        for ngo in &ngram_offs {
            let freq = self.get_posting_list_size(ngo.ngram);
            stats.ngram_lookups += 1;
            if freq == 0 {
                return Some(Vec::new());
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
}

/// Lowercase all literals in a RegexFilter for case-insensitive matching
fn lowercase_filter(filter: &RegexFilter) -> RegexFilter {
    match filter {
        RegexFilter::BruteForce => RegexFilter::BruteForce,
        RegexFilter::Single(s) => RegexFilter::Single(s.to_lowercase()),
        RegexFilter::And(lits) => RegexFilter::And(lits.iter().map(|s| s.to_lowercase()).collect()),
        RegexFilter::Or(lits) => RegexFilter::Or(lits.iter().map(|s| s.to_lowercase()).collect()),
    }
}

/// Find two most selective trigrams (lowest frequency)
fn find_selective_ngrams(ngram_freqs: &[(NgramOffset, usize)]) -> (NgramOffset, NgramOffset) {
    if ngram_freqs.is_empty() {
        panic!("find_selective_ngrams called with empty list");
    }

    if ngram_freqs.len() == 1 {
        return (ngram_freqs[0].0, ngram_freqs[0].0);
    }

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
        assert_eq!(rune_to_byte_offset(content, 0), Some(0));
        assert_eq!(rune_to_byte_offset(content, 1), Some(3));
        assert_eq!(rune_to_byte_offset(content, 2), Some(6));
        assert_eq!(rune_to_byte_offset(content, 3), Some(9));
    }

    #[test]
    fn test_search_options_default() {
        let opts = SearchOptions::default();
        assert!(opts.case_sensitive);
        assert_eq!(opts.max_matches, 0);
        assert_eq!(opts.context_lines, 0);
        assert!(opts.file_pattern.is_none());
    }

    #[test]
    fn test_lowercase_filter() {
        assert_eq!(
            lowercase_filter(&RegexFilter::Single("Hello".into())),
            RegexFilter::Single("hello".into())
        );
        assert_eq!(
            lowercase_filter(&RegexFilter::And(vec!["Foo".into(), "BAR".into()])),
            RegexFilter::And(vec!["foo".into(), "bar".into()])
        );
        assert_eq!(
            lowercase_filter(&RegexFilter::BruteForce),
            RegexFilter::BruteForce
        );
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
        index.search_literal(pattern, true, 0).unwrap()
    }

    /// Helper to create an index and return the IndexData
    fn create_index(files: &[(&str, &[u8])]) -> (IndexData, tempfile::NamedTempFile) {
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
        (index, temp)
    }

    #[test]
    fn test_debug_posting_list_retrieval() {
        use std::io::Write;
        use tempfile::NamedTempFile;
        use trigxrs_core::split_ngrams;

        let content = b"hello world";

        let mut builder = IndexBuilder::new();
        builder.add_file("test.txt", content).unwrap();

        let mut temp = NamedTempFile::new().unwrap();
        {
            let mut buf = Cursor::new(Vec::new());
            builder.write_shard(&mut buf).unwrap();
            temp.write_all(&buf.into_inner()).unwrap();
        }

        let index = IndexData::open(temp.path()).unwrap();

        eprintln!("Index file count: {}", index.file_count());
        eprintln!("Index ngram count: {}", index.ngram_count());
        eprintln!("Index metadata: {:?}", index.metadata());

        let file_content = index.file_content(0);
        eprintln!("File content: {:?}", file_content.map(|c| std::str::from_utf8(c)));

        let pattern = "world";
        let ngrams = split_ngrams(pattern);
        eprintln!("Pattern '{}' trigrams: {}", pattern, ngrams.len());
        for ngo in &ngrams {
            eprintln!("  Trigram at index {}: 0x{:016x}", ngo.index, ngo.ngram);
            let posting = index.get_posting_list(ngo.ngram);
            eprintln!("    Posting list: {:?}", posting.map(|p| p.len()));
        }

        eprintln!("End runes: {:?}", index.end_runes());

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
        let result = create_index_and_search(
            &[("test.txt", b"fn main() { println!(\"Hello\"); }")],
            "println",
        );

        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].byte_offset, 12);
    }

    #[test]
    fn test_search_short_pattern_brute_force() {
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

    // ── Case-insensitive literal search tests ───────────────────────────

    #[test]
    fn test_case_insensitive_literal_search() {
        let (index, _tmp) = create_index(&[
            ("test.txt", b"Hello World HELLO hello"),
        ]);

        let result = index.search_literal("hello", false, 0).unwrap();
        assert_eq!(result.matches.len(), 3);
    }

    #[test]
    fn test_case_insensitive_literal_search_no_match() {
        let (index, _tmp) = create_index(&[
            ("test.txt", b"Hello World"),
        ]);

        let result = index.search_literal("xyz", false, 0).unwrap();
        assert_eq!(result.matches.len(), 0);
    }

    #[test]
    fn test_case_insensitive_literal_short_pattern() {
        let (index, _tmp) = create_index(&[
            ("test.txt", b"Ab aB AB ab"),
        ]);

        let result = index.search_literal("ab", false, 0).unwrap();
        assert_eq!(result.matches.len(), 4);
    }

    // ── File path filtering tests ───────────────────────────────────────

    #[test]
    fn test_file_pattern_filter_literal() {
        let (index, _tmp) = create_index(&[
            ("src/main.rs", b"fn main() { hello() }"),
            ("src/lib.rs", b"fn hello() {}"),
            ("README.md", b"hello world"),
        ]);

        let opts = SearchOptions {
            file_pattern: Some(glob::Pattern::new("*.rs").unwrap()),
            ..Default::default()
        };

        let result = index.search_literal_opts("hello", &opts).unwrap();
        // Should only find matches in .rs files
        for m in &result.matches {
            let path = &index.files()[m.file_idx].path;
            assert!(path.ends_with(".rs"), "unexpected file: {}", path);
        }
    }

    #[test]
    fn test_file_pattern_filter_regex() {
        let (index, _tmp) = create_index(&[
            ("src/main.rs", b"fn main() { hello() }"),
            ("src/lib.rs", b"fn hello() {}"),
            ("README.md", b"hello world"),
        ]);

        let opts = SearchOptions {
            file_pattern: Some(glob::Pattern::new("*.rs").unwrap()),
            ..Default::default()
        };

        let result = index.search_regex_opts("hello", &opts).unwrap();
        for m in &result.matches {
            let path = &index.files()[m.file_idx].path;
            assert!(path.ends_with(".rs"), "unexpected file: {}", path);
        }
    }

    // ── Context extraction tests ────────────────────────────────────────

    #[test]
    fn test_context_extraction() {
        let (index, _tmp) = create_index(&[
            ("test.txt", b"line1\nline2\nline3 match\nline4\nline5"),
        ]);

        let opts = SearchOptions {
            context_lines: 1,
            ..Default::default()
        };

        let result = index.search_literal_opts("match", &opts).unwrap();
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.contexts.len(), 1);

        let ctx = &result.contexts[0];
        assert_eq!(ctx.line_text, "line3 match");
        assert_eq!(ctx.before, vec!["line2"]);
        assert_eq!(ctx.after, vec!["line4"]);
    }

    #[test]
    fn test_context_at_file_start() {
        let (index, _tmp) = create_index(&[
            ("test.txt", b"match here\nline2\nline3"),
        ]);

        let opts = SearchOptions {
            context_lines: 2,
            ..Default::default()
        };

        let result = index.search_literal_opts("match", &opts).unwrap();
        assert_eq!(result.contexts.len(), 1);

        let ctx = &result.contexts[0];
        assert_eq!(ctx.line_text, "match here");
        assert!(ctx.before.is_empty());
        assert_eq!(ctx.after, vec!["line2", "line3"]);
    }

    #[test]
    fn test_context_at_file_end() {
        let (index, _tmp) = create_index(&[
            ("test.txt", b"line1\nline2\nmatch here"),
        ]);

        let opts = SearchOptions {
            context_lines: 2,
            ..Default::default()
        };

        let result = index.search_literal_opts("match", &opts).unwrap();
        assert_eq!(result.contexts.len(), 1);

        let ctx = &result.contexts[0];
        assert_eq!(ctx.line_text, "match here");
        assert_eq!(ctx.before, vec!["line1", "line2"]);
        assert!(ctx.after.is_empty());
    }

    #[test]
    fn test_no_context_when_zero() {
        let (index, _tmp) = create_index(&[
            ("test.txt", b"line1\nmatch here\nline3"),
        ]);

        let result = index.search_literal("match", true, 0).unwrap();
        assert!(result.contexts.is_empty());
    }

    // ── Case-insensitive regex with trigram filtering ────────────────────

    #[test]
    fn test_case_insensitive_regex_uses_trigram_filtering() {
        let (index, _tmp) = create_index(&[
            ("a.txt", b"Hello World"),
            ("b.txt", b"completely different content here xyz"),
        ]);

        let opts = SearchOptions {
            case_sensitive: false,
            ..Default::default()
        };

        let result = index.search_regex_opts("hello", &opts).unwrap();
        assert_eq!(result.matches.len(), 1);
        assert!(result.stats.used_trigram_filtering);
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
        assert_eq!(parse_and_extract("a.*b"), RegexFilter::BruteForce);
    }

    #[test]
    fn test_extract_char_class_brute_force() {
        assert_eq!(parse_and_extract("[a-z]+"), RegexFilter::BruteForce);
    }

    #[test]
    fn test_extract_capture_group() {
        let filter = parse_and_extract("(foo)bar");
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
        let filter = parse_and_extract("foo+");
        match filter {
            RegexFilter::BruteForce => {}
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
        assert_eq!(result.matches[0].byte_offset, 0);
        assert_eq!(result.matches[1].byte_offset, 6);
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
        assert_eq!(result.matches[0].byte_length, 11);
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
        assert_eq!(result.matches[0].byte_offset, 6);
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
        let result = create_index_and_search_regex(
            &[
                ("a.txt", b"hello"),
                ("b.txt", b"world"),
            ],
            ".*",
            true,
            0,
        );
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
        assert_eq!(result.matches[0].byte_length, 5);
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
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].byte_offset, 0);
    }
}
