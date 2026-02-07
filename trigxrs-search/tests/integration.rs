//! End-to-end integration tests for trigxrs-search
//!
//! These tests build real indexes via trigxrs-index and verify full
//! search pipelines (literal, regex, case folding, context, filters, multi-shard).

use std::io::{Cursor, Write};
use tempfile::NamedTempFile;

use trigxrs_index::IndexBuilder;
use trigxrs_search::{IndexData, Query, SearchOptions, Searcher};

/// Build an index from files, write to a temp file, and open as IndexData.
fn build_index(files: &[(&str, &[u8])]) -> (IndexData, NamedTempFile) {
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

/// Build an index shard and return the temp file handle (for Searcher tests).
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

// ── Basic literal search ────────────────────────────────────────────────

#[test]
fn test_e2e_basic_literal() {
    let (index, _tmp) = build_index(&[
        ("hello.txt", b"hello world"),
        ("foo.txt", b"foo bar baz"),
        ("greet.txt", b"say hello world again"),
    ]);

    let result = index.search_literal("world", true, 0).unwrap();
    assert_eq!(result.matches.len(), 2);

    // First match: hello.txt, "world" at offset 6
    let m0 = &result.matches[0];
    assert_eq!(m0.file_idx, 0);
    assert_eq!(m0.byte_offset, 6);
    assert_eq!(m0.byte_length, 5);

    // Second match: greet.txt, "world" at offset 10
    let m1 = &result.matches[1];
    assert_eq!(m1.file_idx, 2);
    assert_eq!(m1.byte_offset, 10);
    assert_eq!(m1.byte_length, 5);
}

// ── Basic regex search ──────────────────────────────────────────────────

#[test]
fn test_e2e_basic_regex() {
    let (index, _tmp) = build_index(&[
        ("nums.txt", b"abc 12345 xyz 67 end"),
    ]);

    let result = index.search_regex(r"\d+", true, 0).unwrap();
    assert_eq!(result.matches.len(), 2);
    assert_eq!(result.matches[0].byte_offset, 4);
    assert_eq!(result.matches[0].byte_length, 5);
    assert_eq!(result.matches[1].byte_offset, 14);
    assert_eq!(result.matches[1].byte_length, 2);
}

// ── Case-insensitive literal search ─────────────────────────────────────

#[test]
fn test_e2e_case_insensitive_literal() {
    let (index, _tmp) = build_index(&[
        ("mixed.txt", b"Hello HELLO hello hElLo"),
    ]);

    let result = index.search_literal("hello", false, 0).unwrap();
    assert_eq!(result.matches.len(), 4);
}

// ── Case-insensitive regex search ───────────────────────────────────────

#[test]
fn test_e2e_case_insensitive_regex() {
    let (index, _tmp) = build_index(&[
        ("code.go", b"func ServeGitiles() {}\nfunc serveGITILES() {}"),
    ]);

    let opts = SearchOptions {
        case_sensitive: false,
        ..Default::default()
    };

    let result = index.search_regex_opts(r"func.*Gitiles", &opts).unwrap();
    assert_eq!(result.matches.len(), 2);
}

// ── Context lines ───────────────────────────────────────────────────────

#[test]
fn test_e2e_context_lines() {
    let (index, _tmp) = build_index(&[
        ("five.txt", b"line1\nline2\nline3 target\nline4\nline5"),
    ]);

    let opts = SearchOptions {
        context_lines: 1,
        ..Default::default()
    };

    let result = index.search_literal_opts("target", &opts).unwrap();
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.contexts.len(), 1);

    let ctx = &result.contexts[0];
    assert_eq!(ctx.line_text, "line3 target");
    assert_eq!(ctx.before, vec!["line2"]);
    assert_eq!(ctx.after, vec!["line4"]);
}

// ── File pattern filter ─────────────────────────────────────────────────

#[test]
fn test_e2e_file_pattern_filter() {
    let (index, _tmp) = build_index(&[
        ("src/main.rs", b"fn hello() {}"),
        ("src/lib.rs", b"fn hello() {}"),
        ("README.md", b"hello world docs"),
    ]);

    let opts = SearchOptions {
        file_pattern: Some(glob::Pattern::new("*.rs").unwrap()),
        ..Default::default()
    };

    let result = index.search_literal_opts("hello", &opts).unwrap();
    // Only .rs files should match
    for m in &result.matches {
        let path = &index.files()[m.file_idx].path;
        assert!(path.ends_with(".rs"), "unexpected file: {}", path);
    }
    assert!(result.matches.len() >= 2);
}

// ── Max matches ─────────────────────────────────────────────────────────

#[test]
fn test_e2e_max_matches() {
    let (index, _tmp) = build_index(&[
        ("many.txt", b"abc abc abc abc abc abc abc abc abc abc"),
    ]);

    let opts = SearchOptions {
        max_matches: 3,
        ..Default::default()
    };

    let result = index.search_literal_opts("abc", &opts).unwrap();
    assert_eq!(result.matches.len(), 3);
}

// ── Empty index ─────────────────────────────────────────────────────────

#[test]
fn test_e2e_empty_index() {
    let mut builder = IndexBuilder::new();
    // Add no files

    let mut temp = NamedTempFile::new().unwrap();
    {
        let mut buf = Cursor::new(Vec::new());
        builder.write_shard(&mut buf).unwrap();
        temp.write_all(&buf.into_inner()).unwrap();
    }

    let index = IndexData::open(temp.path()).unwrap();
    let result = index.search_literal("anything", true, 0).unwrap();
    assert_eq!(result.matches.len(), 0);
}

// ── Large file ──────────────────────────────────────────────────────────

#[test]
fn test_e2e_large_file() {
    // Build a ~50KB file with a pattern every ~500 bytes
    let mut content = Vec::with_capacity(50_000);
    let mut expected_count = 0;
    for i in 0..100 {
        let chunk = format!("line {} padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding needle_here more padding\n", i);
        content.extend_from_slice(chunk.as_bytes());
        expected_count += 1;
    }

    let (index, _tmp) = build_index(&[
        ("large.txt", &content),
    ]);

    let result = index.search_literal("needle_here", true, 0).unwrap();
    assert_eq!(result.matches.len(), expected_count);
}

// ── Multi-shard literal search ──────────────────────────────────────────

#[test]
fn test_e2e_multi_shard_searcher() {
    let shard1 = build_shard(&[
        ("shard1/a.txt", b"hello world"),
        ("shard1/b.txt", b"goodbye world"),
    ]);
    let shard2 = build_shard(&[
        ("shard2/c.txt", b"new world order"),
        ("shard2/d.txt", b"nothing here"),
    ]);

    let searcher = Searcher::open(&[shard1.path(), shard2.path()]).unwrap();

    let query = Query {
        pattern: "world".into(),
        is_regex: false,
        options: Default::default(),
    };

    let result = searcher.search(&query).unwrap();
    assert_eq!(result.matches.len(), 3);

    // Verify global file indices: shard1 files are 0,1; shard2 files are 2,3
    let idxs: Vec<usize> = result.matches.iter().map(|m| m.file_idx).collect();
    assert!(idxs.contains(&0));
    assert!(idxs.contains(&1));
    assert!(idxs.contains(&2));
}

// ── Multi-shard regex search ────────────────────────────────────────────

#[test]
fn test_e2e_multi_shard_regex() {
    let shard1 = build_shard(&[
        ("s1/code.rs", b"fn main() { println!(\"hello\"); }"),
    ]);
    let shard2 = build_shard(&[
        ("s2/code.rs", b"fn test() { println!(\"world\"); }"),
    ]);

    let searcher = Searcher::open(&[shard1.path(), shard2.path()]).unwrap();

    let query = Query {
        pattern: r"println!\(.*\)".into(),
        is_regex: true,
        options: Default::default(),
    };

    let result = searcher.search(&query).unwrap();
    assert_eq!(result.matches.len(), 2);
}

// ── Multi-shard with context ────────────────────────────────────────────

#[test]
fn test_e2e_searcher_context() {
    let shard1 = build_shard(&[
        ("a.txt", b"before1\ntarget_line\nafter1"),
    ]);
    let shard2 = build_shard(&[
        ("b.txt", b"before2\ntarget_line\nafter2"),
    ]);

    let searcher = Searcher::open(&[shard1.path(), shard2.path()]).unwrap();

    let query = Query {
        pattern: "target_line".into(),
        is_regex: false,
        options: SearchOptions {
            context_lines: 1,
            ..Default::default()
        },
    };

    let result = searcher.search(&query).unwrap();
    assert_eq!(result.matches.len(), 2);
    assert_eq!(result.contexts.len(), 2);

    // Each context should have 1 before and 1 after
    for ctx in &result.contexts {
        assert_eq!(ctx.line_text, "target_line");
        assert_eq!(ctx.before.len(), 1);
        assert_eq!(ctx.after.len(), 1);
    }
}

// ── Search stats ────────────────────────────────────────────────────────

#[test]
fn test_e2e_search_stats() {
    let (index, _tmp) = build_index(&[
        ("a.txt", b"hello world hello"),
        ("b.txt", b"hello again"),
    ]);

    let result = index.search_literal("hello", true, 0).unwrap();
    assert!(result.stats.matches_found > 0);
    assert_eq!(result.stats.matches_found, result.matches.len());
    assert!(result.stats.ngram_lookups > 0);
    assert!(result.stats.used_trigram_filtering);
}

// ── Short pattern (brute-force fallback) ────────────────────────────────

#[test]
fn test_e2e_short_pattern_brute_force() {
    let (index, _tmp) = build_index(&[
        ("test.txt", b"ab cd ab ef ab"),
    ]);

    let result = index.search_literal("ab", true, 0).unwrap();
    assert_eq!(result.matches.len(), 3);
    // Brute force doesn't use trigram filtering
    assert!(!result.stats.used_trigram_filtering);
}

// ── No match ────────────────────────────────────────────────────────────

#[test]
fn test_e2e_no_match() {
    let (index, _tmp) = build_index(&[
        ("a.txt", b"hello world"),
        ("b.txt", b"foo bar baz"),
    ]);

    let result = index.search_literal("zzzzzzz", true, 0).unwrap();
    assert_eq!(result.matches.len(), 0);
    assert_eq!(result.stats.matches_found, 0);
}
