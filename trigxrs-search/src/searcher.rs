//! High-level multi-shard searcher
//!
//! Provides a unified `Searcher` that opens multiple index shards,
//! searches them in parallel via rayon, and merges results with
//! globally-unique file indices.

use std::path::Path;

use rayon::prelude::*;

use crate::reader::{FileEntry, IndexData};
use crate::search::{Query, SearchResult, SearchStats};
use crate::Result;

/// A searcher over one or more index shards.
///
/// File indices in search results are globally unique across all shards.
/// Use [`file_entry`](Searcher::file_entry) and [`file_content`](Searcher::file_content)
/// to resolve global indices back to shard-local data.
pub struct Searcher {
    shards: Vec<IndexData>,
    /// Cumulative file counts: `shard_offsets[i]` = total files in shards `0..i`
    shard_offsets: Vec<usize>,
}

impl Searcher {
    /// Open multiple index shard files.
    ///
    /// Passing an empty slice is valid and produces a searcher with no results.
    pub fn open(paths: &[impl AsRef<Path>]) -> Result<Self> {
        let mut shards = Vec::with_capacity(paths.len());
        for p in paths {
            shards.push(IndexData::open(p)?);
        }

        let mut shard_offsets = Vec::with_capacity(shards.len());
        let mut cumulative = 0usize;
        for shard in &shards {
            shard_offsets.push(cumulative);
            cumulative += shard.file_count();
        }

        Ok(Self {
            shards,
            shard_offsets,
        })
    }

    /// Search all shards in parallel and merge results.
    pub fn search(&self, query: &Query) -> Result<SearchResult> {
        if self.shards.is_empty() {
            return Ok(SearchResult {
                matches: Vec::new(),
                contexts: Vec::new(),
                stats: SearchStats::default(),
            });
        }

        // Search each shard in parallel
        let shard_results: Vec<Result<SearchResult>> = self
            .shards
            .par_iter()
            .map(|shard| {
                if query.is_regex {
                    shard.search_regex_opts(&query.pattern, &query.options)
                } else {
                    shard.search_literal_opts(&query.pattern, &query.options)
                }
            })
            .collect();

        // Merge results
        let mut all_matches = Vec::new();
        let mut all_contexts = Vec::new();
        let mut merged_stats = SearchStats::default();

        let max = if query.options.max_matches == 0 {
            usize::MAX
        } else {
            query.options.max_matches
        };

        for (shard_idx, result) in shard_results.into_iter().enumerate() {
            let result = result?;
            let offset = self.shard_offsets[shard_idx];

            for mut m in result.matches {
                if all_matches.len() >= max {
                    break;
                }
                m.file_idx += offset;
                all_matches.push(m);
            }

            for mut ctx in result.contexts {
                if all_matches.len() >= max && all_contexts.len() >= max {
                    break;
                }
                ctx.m.file_idx += offset;
                all_contexts.push(ctx);
            }

            merged_stats.ngram_lookups += result.stats.ngram_lookups;
            merged_stats.candidates_checked += result.stats.candidates_checked;
            merged_stats.matches_found += result.stats.matches_found;
            merged_stats.bytes_loaded += result.stats.bytes_loaded;
            if result.stats.used_trigram_filtering {
                merged_stats.used_trigram_filtering = true;
            }
        }

        // Truncate to max_matches if needed
        if all_matches.len() > max {
            all_matches.truncate(max);
        }
        if all_contexts.len() > max {
            all_contexts.truncate(max);
        }
        merged_stats.matches_found = all_matches.len();

        Ok(SearchResult {
            matches: all_matches,
            contexts: all_contexts,
            stats: merged_stats,
        })
    }

    /// Total number of files across all shards.
    pub fn file_count(&self) -> usize {
        match self.shard_offsets.last() {
            Some(&last_offset) => last_offset + self.shards.last().map_or(0, |s| s.file_count()),
            None => 0,
        }
    }

    /// Get a file entry by global file index.
    pub fn file_entry(&self, file_idx: usize) -> Option<&FileEntry> {
        let (shard_idx, local_idx) = self.resolve_file_idx(file_idx)?;
        self.shards[shard_idx].files().get(local_idx)
    }

    /// Get file content by global file index.
    pub fn file_content(&self, file_idx: usize) -> Option<&[u8]> {
        let (shard_idx, local_idx) = self.resolve_file_idx(file_idx)?;
        self.shards[shard_idx].file_content(local_idx)
    }

    /// Resolve a global file index to (shard_index, shard-local file index).
    fn resolve_file_idx(&self, global: usize) -> Option<(usize, usize)> {
        if self.shards.is_empty() {
            return None;
        }

        // Binary search: find the last shard_offset <= global
        let shard_idx = match self.shard_offsets.binary_search(&global) {
            Ok(idx) => idx,
            Err(idx) => idx.checked_sub(1)?,
        };

        if shard_idx >= self.shards.len() {
            return None;
        }

        let local = global - self.shard_offsets[shard_idx];
        if local < self.shards[shard_idx].file_count() {
            Some((shard_idx, local))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use tempfile::NamedTempFile;
    use trigxrs_index::IndexBuilder;

    /// Build an index shard from files, write to a temp file, return the temp file handle.
    fn build_shard(files: &[(&str, &[u8])]) -> NamedTempFile {
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
        temp
    }

    #[test]
    fn test_searcher_single_shard() {
        let shard = build_shard(&[
            ("a.txt", b"hello world"),
            ("b.txt", b"goodbye world"),
        ]);

        let searcher = Searcher::open(&[shard.path()]).unwrap();
        assert_eq!(searcher.file_count(), 2);

        let query = Query {
            pattern: "world".into(),
            is_regex: false,
            options: Default::default(),
        };

        let result = searcher.search(&query).unwrap();
        assert_eq!(result.matches.len(), 2);
        assert_eq!(result.matches[0].file_idx, 0);
        assert_eq!(result.matches[1].file_idx, 1);
    }

    #[test]
    fn test_searcher_single_shard_regex() {
        let shard = build_shard(&[
            ("a.txt", b"hello world"),
            ("b.txt", b"foo bar"),
        ]);

        let searcher = Searcher::open(&[shard.path()]).unwrap();

        let query = Query {
            pattern: "hel.*rld".into(),
            is_regex: true,
            options: Default::default(),
        };

        let result = searcher.search(&query).unwrap();
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].file_idx, 0);
    }

    #[test]
    fn test_searcher_multi_shard() {
        let shard1 = build_shard(&[
            ("shard1/a.txt", b"hello world"),
            ("shard1/b.txt", b"goodbye world"),
        ]);
        let shard2 = build_shard(&[
            ("shard2/c.txt", b"new world order"),
            ("shard2/d.txt", b"nothing here"),
        ]);

        let searcher = Searcher::open(&[shard1.path(), shard2.path()]).unwrap();
        assert_eq!(searcher.file_count(), 4);

        let query = Query {
            pattern: "world".into(),
            is_regex: false,
            options: Default::default(),
        };

        let result = searcher.search(&query).unwrap();
        // shard1: a.txt(0) + b.txt(1); shard2: c.txt(2)
        assert_eq!(result.matches.len(), 3);

        // Verify global file indices span both shards
        let file_idxs: Vec<usize> = result.matches.iter().map(|m| m.file_idx).collect();
        assert!(file_idxs.contains(&0)); // shard1/a.txt
        assert!(file_idxs.contains(&1)); // shard1/b.txt
        assert!(file_idxs.contains(&2)); // shard2/c.txt
    }

    #[test]
    fn test_searcher_file_entry_resolution() {
        let shard1 = build_shard(&[
            ("alpha.txt", b"alpha content"),
        ]);
        let shard2 = build_shard(&[
            ("beta.txt", b"beta content"),
        ]);

        let searcher = Searcher::open(&[shard1.path(), shard2.path()]).unwrap();
        assert_eq!(searcher.file_count(), 2);

        // Global index 0 -> shard1, file 0
        let entry0 = searcher.file_entry(0).unwrap();
        assert_eq!(entry0.path, "alpha.txt");

        let content0 = searcher.file_content(0).unwrap();
        assert_eq!(content0, b"alpha content");

        // Global index 1 -> shard2, file 0
        let entry1 = searcher.file_entry(1).unwrap();
        assert_eq!(entry1.path, "beta.txt");

        let content1 = searcher.file_content(1).unwrap();
        assert_eq!(content1, b"beta content");

        // Out of range
        assert!(searcher.file_entry(2).is_none());
        assert!(searcher.file_content(2).is_none());
    }

    #[test]
    fn test_searcher_max_matches() {
        let shard1 = build_shard(&[
            ("a.txt", b"abc abc abc"),
        ]);
        let shard2 = build_shard(&[
            ("b.txt", b"abc abc abc"),
        ]);

        let searcher = Searcher::open(&[shard1.path(), shard2.path()]).unwrap();

        let query = Query {
            pattern: "abc".into(),
            is_regex: false,
            options: crate::search::SearchOptions {
                max_matches: 2,
                ..Default::default()
            },
        };

        let result = searcher.search(&query).unwrap();
        assert!(result.matches.len() <= 2);
    }

    #[test]
    fn test_searcher_empty() {
        let empty: &[&str] = &[];
        let searcher = Searcher::open(empty).unwrap();
        assert_eq!(searcher.file_count(), 0);

        let query = Query {
            pattern: "anything".into(),
            is_regex: false,
            options: Default::default(),
        };

        let result = searcher.search(&query).unwrap();
        assert!(result.matches.is_empty());
    }
}
