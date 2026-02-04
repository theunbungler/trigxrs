# Trigxrs Implementation Status

## Overview

Rust reimplementation of zoekt's core trigram-based code search. This document tracks implementation progress.

**Last Updated:** 2026-02-04
**Tests Passing:** 73 (33 core + 25 index + 10 search + 5 doctests)

---

## Completed Phases

### Phase 1: Foundation ✅

| Component | File | Description |
|-----------|------|-------------|
| Project Setup | `Cargo.toml` | Workspace with 4 crates |
| Trigram Encoding | `trigxrs-core/src/ngram.rs` | 64-bit trigram encoding, `split_ngrams()` |
| Delta-Varint | `trigxrs-core/src/varint.rs` | Posting list compression |
| B+-tree | `trigxrs-core/src/btree.rs` | Ngram lookup tree |

### Phase 2: Index Building ✅

| Component | File | Description |
|-----------|------|-------------|
| Postings Builder | `trigxrs-index/src/postings.rs` | Collects trigram occurrences |
| Index Writer | `trigxrs-index/src/writer.rs` | Tagged section format |
| Index Builder | `trigxrs-index/src/builder.rs` | File processing, extension trait |

### Phase 3: Index Reading ✅

| Component | File | Description |
|-----------|------|-------------|
| Index Reader | `trigxrs-search/src/reader.rs` | Memory-mapped index access |
| Posting Iterator | `trigxrs-search/src/posting.rs` | Compressed posting traversal |

---

## Remaining Phases

### Phase 4: Literal Search (Task #10)

**Reference:** `zoekt/index/indexdata.go:416-500`

Implement `search_literal()`:
1. Extract trigrams from search pattern
2. Find two most selective trigrams (lowest posting frequency)
3. Use `DistanceIterator` to intersect with distance constraint
4. Verify matches against actual content

```rust
// Planned API
pub fn search_literal(&self, pattern: &str, case_sensitive: bool) -> Vec<Match>;
```

### Phase 5: Regex Search (Tasks #11)

**Reference:** `zoekt/internal/syntaxutil/regexp.go`

1. Use `regex-syntax` crate to parse regex AST
2. Extract literal substrings from regex
3. Select trigrams from literals
4. Filter candidates, verify with full regex

### Phase 6: Filters & Context (Tasks #12, #13)

1. **File path filtering** - glob patterns for include/exclude
2. **Context extraction** - lines before/after matches
3. **Unicode case folding** - full SimpleFold support

### Phase 7: CLI & API (Tasks #14, #15)

1. **CLI commands:**
   - `trigxrs index <dir> -o <output.zrst>`
   - `trigxrs search <index> <pattern>`

2. **Library API:**
   ```rust
   pub struct Searcher { ... }
   impl Searcher {
       pub fn open(paths: &[PathBuf]) -> Result<Self>;
       pub fn search(&self, query: &Query) -> Result<SearchResults>;
   }
   ```

### Phase 8: Integration Testing & Optimization

1. End-to-end tests
2. Comparison tests against Go zoekt
3. Performance benchmarks
4. Optimizations (SIMD, parallel search)

---

## Crate Structure

```
trigxrs/
├── Cargo.toml              # Workspace
├── trigxrs-core/           # Core algorithms (no I/O)
│   └── src/
│       ├── lib.rs
│       ├── ngram.rs        # Trigram encoding
│       ├── varint.rs       # Delta-varint compression
│       ├── btree.rs        # B+-tree index
│       └── error.rs
├── trigxrs-index/          # Index building
│   └── src/
│       ├── lib.rs
│       ├── postings.rs     # Posting list builder
│       ├── writer.rs       # Index file writer
│       ├── builder.rs      # IndexBuilder + extensions
│       └── error.rs
├── trigxrs-search/         # Search execution
│   └── src/
│       ├── lib.rs
│       ├── reader.rs       # Memory-mapped index reader
│       ├── posting.rs      # Posting iterators
│       └── error.rs
└── trigxrs-cli/            # Command-line tools
    └── src/
        └── main.rs         # CLI (stub)
```

---

## Key Design Decisions

1. **Index Format:** Custom Rust format (not Go-compatible), tagged sections for extensibility
2. **Extension Trait:** `IndexExtension` allows bolt-on features without modifying core
3. **Memory Mapping:** Uses `memmap2` for efficient index access
4. **Posting Format:** Delta-varint encoded rune offsets (matches zoekt)

---

## How to Continue

1. **Run tests:** `cargo test`
2. **Check current tasks:** See task list below
3. **Next implementation:** Phase 4 literal search in `trigxrs-search/src/search.rs`

### Remaining Tasks

```
#10. [pending] Implement literal search with trigram filtering
#11. [pending] Implement regex search with literal extraction
#12. [pending] Implement filters and context extraction
#13. [pending] Implement Unicode case folding
#14. [pending] Implement CLI tools
#15. [pending] Implement library API with Searcher
```

---

## Reference Files in zoekt/

| Purpose | Go File | Notes |
|---------|---------|-------|
| Search execution | `index/indexdata.go` | Main search logic |
| Hit iteration | `index/hititer.go` | Posting intersection |
| Content provider | `index/contentprovider.go` | Context extraction |
| Regex literals | `internal/syntaxutil/regexp.go` | Literal extraction |
| Case folding | `index/bits.go:26-47` | Unicode SimpleFold |
