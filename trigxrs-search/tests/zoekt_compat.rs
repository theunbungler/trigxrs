//! Zoekt compatibility tests
//!
//! Ported from zoekt/index/index_test.go to verify that trigxrs handles
//! the same edge cases as the Go reference implementation.

use std::io::{Cursor, Write};
use tempfile::NamedTempFile;

use trigxrs_index::IndexBuilder;
use trigxrs_search::{IndexData, SearchOptions};

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

// ── TestBoundary (index_test.go:90) ─────────────────────────────────────
// "there" must NOT match across document boundary: "x the" + "reader"

#[test]
fn test_boundary() {
    let (index, _tmp) = build_index(&[
        ("f1.txt", b"x the"),
        ("f2.txt", b"reader"),
    ]);

    let result = index.search_literal("there", true, 0).unwrap();
    assert_eq!(
        result.matches.len(),
        0,
        "\"there\" should not match across document boundaries"
    );
}

// ── TestBasic (index_test.go:126) ───────────────────────────────────────
// "water" in "to carry water in the ..." → offset 9

#[test]
fn test_basic_literal() {
    let (index, _tmp) = build_index(&[
        ("f1.txt", b"to carry water in the"),
    ]);

    let result = index.search_literal("water", true, 0).unwrap();
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].byte_offset, 9);
    assert_eq!(result.matches[0].byte_length, 5);
}

// ── TestNewlines (index_test.go:204) ────────────────────────────────────
// "ne2" → line 2, correct offset

#[test]
fn test_newlines() {
    let (index, _tmp) = build_index(&[
        ("f1.txt", b"line1\nline2\nline3"),
    ]);

    let result = index.search_literal("ne2", true, 0).unwrap();
    assert_eq!(result.matches.len(), 1);
    // "line1\n" = 6 bytes, then "li" = 2 bytes → "ne2" starts at offset 8
    assert_eq!(result.matches[0].byte_offset, 8);
    assert_eq!(result.matches[0].line_number, Some(2));
}

// ── TestCaseFold (index_test.go:322) — case-sensitive ───────────────────
// "bananas" case-sensitive vs content "BaNaNAS" → 0 matches

#[test]
fn test_case_fold_sensitive() {
    let (index, _tmp) = build_index(&[
        ("f1.txt", b"I love BaNaNAS"),
    ]);

    let result = index.search_literal("bananas", true, 0).unwrap();
    assert_eq!(
        result.matches.len(),
        0,
        "case-sensitive search for \"bananas\" should not match \"BaNaNAS\""
    );
}

// ── TestCaseFold (index_test.go:322) — case-insensitive ─────────────────
// "bananas" case-insensitive vs "BaNaNAS" → 1 match at offset 7

#[test]
fn test_case_fold_insensitive() {
    let (index, _tmp) = build_index(&[
        ("f1.txt", b"I love BaNaNAS"),
    ]);

    let result = index.search_literal("bananas", false, 0).unwrap();
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].byte_offset, 7);
}

// ── TestUnicodeExactMatch (index_test.go:2332) ──────────────────────────
// "néédlÉ" exact match in Unicode content

#[test]
fn test_unicode_exact_match() {
    let content = "hay néédlÉ hay";
    let (index, _tmp) = build_index(&[
        ("f1.txt", content.as_bytes()),
    ]);

    let result = index.search_literal("néédlÉ", true, 0).unwrap();
    assert_eq!(result.matches.len(), 1);

    // Verify the match offsets are correct in bytes
    let byte_offset = result.matches[0].byte_offset as usize;
    let byte_length = result.matches[0].byte_length as usize;
    let matched = &content.as_bytes()[byte_offset..byte_offset + byte_length];
    assert_eq!(matched, "néédlÉ".as_bytes());
}

// ── TestUnicodeCoverContent (index_test.go:2353) ────────────────────────
// Uppercased "NÉÉDLÉ" case-sensitive → 0 matches, insensitive → 1 match

#[test]
fn test_unicode_cover_content() {
    let content = "hay néédlé hay";
    let (index, _tmp) = build_index(&[
        ("f1.txt", content.as_bytes()),
    ]);

    // Case-sensitive: "NÉÉDLÉ" should not match "néédlé"
    let result_sensitive = index.search_literal("NÉÉDLÉ", true, 0).unwrap();
    assert_eq!(
        result_sensitive.matches.len(),
        0,
        "case-sensitive \"NÉÉDLÉ\" should not match \"néédlé\""
    );

    // Case-insensitive: should match
    let result_insensitive = index.search_literal("NÉÉDLÉ", false, 0).unwrap();
    assert_eq!(result_insensitive.matches.len(), 1);
}

// ── TestUnicodeFileStartOffsets (index_test.go:2458) ────────────────────
// "世界" in f1 doesn't corrupt f2 offsets

#[test]
fn test_unicode_file_start_offsets() {
    let (index, _tmp) = build_index(&[
        ("f1.txt", "你好世界".as_bytes()),
        ("f2.txt", b"hello world"),
    ]);

    // Search in f2 should still work correctly
    let result = index.search_literal("world", true, 0).unwrap();
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].file_idx, 1);
    assert_eq!(result.matches[0].byte_offset, 6);

    // Search for Unicode content in f1
    let result_unicode = index.search_literal("世界", true, 0).unwrap();
    assert_eq!(result_unicode.matches.len(), 1);
    assert_eq!(result_unicode.matches[0].file_idx, 0);
}

// ── TestRegexp (index_test.go:1363) ─────────────────────────────────────
// `dle.*bla` → offset 3, length 11

#[test]
fn test_regexp_basic() {
    let (index, _tmp) = build_index(&[
        ("f1.txt", b"needleblabla"),
    ]);

    let result = index.search_regex("dle.*bla", true, 0).unwrap();
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].byte_offset, 3);
    // "dleblabla" = 9 bytes (regex is greedy)
    assert_eq!(result.matches[0].byte_length, 9);
}

// ── TestRegexpOrder (index_test.go:1469) ────────────────────────────────
// `dle.*bla` on "bla...needle" → 0 matches

#[test]
fn test_regexp_order() {
    let (index, _tmp) = build_index(&[
        ("f1.txt", b"blaneedle"),
    ]);

    let result = index.search_regex("dle.*bla", true, 0).unwrap();
    assert_eq!(
        result.matches.len(),
        0,
        "\"dle.*bla\" should not match \"blaneedle\" (wrong order)"
    );
}

// ── TestRegexpCaseSensitive (index_test.go:1651) ────────────────────────
// `func.*Gitiles` case-sensitive → 1 match

#[test]
fn test_regexp_case_sensitive() {
    let (index, _tmp) = build_index(&[
        ("code.go", b"func ServeGitiles() {}"),
    ]);

    let opts = SearchOptions {
        case_sensitive: true,
        ..Default::default()
    };

    let result = index.search_regex_opts("func.*Gitiles", &opts).unwrap();
    assert_eq!(result.matches.len(), 1);
}

// ── TestRegexpCaseFolding (index_test.go:1685) ──────────────────────────
// `func.*GITILES` case-insensitive → 1 match

#[test]
fn test_regexp_case_folding() {
    let (index, _tmp) = build_index(&[
        ("code.go", b"func ServeGitiles() {}"),
    ]);

    let opts = SearchOptions {
        case_sensitive: false,
        ..Default::default()
    };

    let result = index.search_regex_opts("func.*GITILES", &opts).unwrap();
    assert_eq!(result.matches.len(), 1);
}

// ── TestCaseRegexp (index_test.go:1701) ─────────────────────────────────
// `[xb][xl][xa]` on "BLABLABLA" case-sensitive → 0

#[test]
fn test_case_regexp_no_match() {
    let (index, _tmp) = build_index(&[
        ("f1.txt", b"BLABLABLA"),
    ]);

    let opts = SearchOptions {
        case_sensitive: true,
        ..Default::default()
    };

    let result = index.search_regex_opts("[xb][xl][xa]", &opts).unwrap();
    assert_eq!(
        result.matches.len(),
        0,
        "case-sensitive \"[xb][xl][xa]\" should not match uppercase \"BLABLABLA\""
    );
}
