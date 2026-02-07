# Trigxrs Implementation Status

## Overview

Rust reimplementation of zoekt's core trigram-based code search. This document tracks implementation progress.

**Last Updated:** 2026-02-07
**Tests Passing:** 165 (42 core + 25 index + 64 search + 15 integration + 13 zoekt_compat + 6 doctests)

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

### Phase 4: Literal Search ✅

| Component | File | Description |
|-----------|------|-------------|
| Literal Search | `trigxrs-search/src/search.rs` | Trigram-filtered literal search |
| Match struct | `trigxrs-search/src/search.rs` | Search result representation |
| Brute-force fallback | `trigxrs-search/src/search.rs` | Short pattern search (<3 chars) |

### Phase 5: Regex Search ✅

| Component | File | Description |
|-----------|------|-------------|
| Regex literal extraction | `trigxrs-search/src/search.rs` | `extract_literals_from_hir()` parses regex AST |
| Trigram candidate filtering | `trigxrs-search/src/search.rs` | Filters files via trigram posting lists |
| Regex verification | `trigxrs-search/src/search.rs` | Compiled regex verifies candidates |

**Implementation details:**
- Uses `regex-syntax` to parse regex HIR and extract literal substrings
- Supports `And` (concat), `Or` (alternation), `Single`, and `BruteForce` filter types
- Pure literals delegate to `search_literal` for efficiency
- Case-insensitive regex uses trigram variant expansion (not brute force)

### Phase 6: Filters, Context & Case Folding ✅

| Component | File | Description |
|-----------|------|-------------|
| Case folding | `trigxrs-core/src/case.rs` | `generate_case_ngrams()` for trigram variant expansion |
| SearchOptions | `trigxrs-search/src/search.rs` | Configurable search: case, max_matches, context, file filter |
| MatchContext | `trigxrs-search/src/search.rs` | Context lines before/after matches |
| Case-insensitive literal | `trigxrs-search/src/search.rs` | Trigram variant expansion + case-insensitive verify |
| Case-insensitive regex | `trigxrs-search/src/search.rs` | Lowercased filter with case-variant trigram lookup |
| File path filtering | `trigxrs-search/src/search.rs` | Glob pattern filtering integrated into search |
| CLI integration | `trigxrs-cli/src/main.rs` | Uses `SearchOptions` API, `MatchContext` for display |

**API:**
```rust
pub fn search_literal_opts(&self, pattern: &str, opts: &SearchOptions) -> Result<SearchResult>;
pub fn search_regex_opts(&self, pattern: &str, opts: &SearchOptions) -> Result<SearchResult>;

// Legacy wrappers (still work, delegate to _opts variants):
pub fn search_literal(&self, pattern: &str, case_sensitive: bool, max_matches: usize) -> Result<SearchResult>;
pub fn search_regex(&self, pattern: &str, case_sensitive: bool, max_matches: usize) -> Result<SearchResult>;
```

**CLI:**
```bash
trigxrs index <dir> -o <output.zrst>
trigxrs search <index> <pattern> [-i] [-c N] [-f "*.rs"] [-r]
```

### Phase 7: Library API ✅

| Component | File | Description |
|-----------|------|-------------|
| Query struct | `trigxrs-search/src/search.rs` | Combines pattern, type, and options |
| Searcher | `trigxrs-search/src/searcher.rs` | Multi-shard parallel search via rayon |
| CLI refactored | `trigxrs-cli/src/main.rs` | Uses `Searcher` API |

**API:**
```rust
pub struct Searcher { ... }
impl Searcher {
    pub fn open(paths: &[impl AsRef<Path>]) -> Result<Self>;
    pub fn search(&self, query: &Query) -> Result<SearchResult>;
    pub fn file_entry(&self, file_idx: usize) -> Option<&FileEntry>;
    pub fn file_content(&self, file_idx: usize) -> Option<&[u8]>;
}
```

### Phase 8: Integration Testing & Optimization ✅

| Component | File | Description |
|-----------|------|-------------|
| E2E integration tests | `trigxrs-search/tests/integration.rs` | 15 end-to-end tests |
| Zoekt compatibility tests | `trigxrs-search/tests/zoekt_compat.rs` | 13 ported zoekt edge-case tests |
| Criterion benchmarks | `trigxrs-search/benches/search_bench.rs` | 8 benchmarks (index build, literal, regex) |
| memchr optimization | `trigxrs-search/src/search.rs` | SIMD-accelerated `memchr::memmem` for byte search |

**Optimizations:**
- Replaced manual `windows().position()` byte search with `memchr::memmem::find` (SIMD-accelerated)
- Brute-force loop uses `memchr::memmem::Finder` to amortize SIMD setup across repeated searches

**Benchmarks:**
```bash
cargo bench -p trigxrs-search  # Run all benchmarks
```

---

## Crate Structure

```
trigxrs/
├── Cargo.toml              # Workspace
├── trigxrs-core/            # Core algorithms (no I/O)
│   └── src/
│       ├── lib.rs
│       ├── ngram.rs         # Trigram encoding
│       ├── varint.rs        # Delta-varint compression
│       ├── btree.rs         # B+-tree index
│       ├── case.rs          # Unicode case folding
│       └── error.rs
├── trigxrs-index/           # Index building
│   └── src/
│       ├── lib.rs
│       ├── postings.rs      # Posting list builder
│       ├── writer.rs        # Index file writer
│       ├── builder.rs       # IndexBuilder + extensions
│       └── error.rs
├── trigxrs-search/          # Search execution
│   ├── src/
│   │   ├── lib.rs
│   │   ├── reader.rs        # Memory-mapped index reader
│   │   ├── posting.rs       # Posting iterators
│   │   ├── search.rs        # Literal + regex search, SearchOptions, MatchContext
│   │   ├── searcher.rs      # Multi-shard parallel Searcher
│   │   └── error.rs
│   ├── tests/
│   │   ├── integration.rs   # E2E integration tests
│   │   └── zoekt_compat.rs  # Zoekt compatibility tests
│   └── benches/
│       └── search_bench.rs  # Criterion benchmarks
└── trigxrs-cli/             # Command-line tools
    └── src/
        └── main.rs          # CLI (index + search subcommands)
```

---

## Key Design Decisions

1. **Index Format:** Custom Rust format (not Go-compatible), tagged sections for extensibility
2. **Extension Trait:** `IndexExtension` allows bolt-on features without modifying core
3. **Memory Mapping:** Uses `memmap2` for efficient index access
4. **Posting Format:** Delta-varint encoded rune offsets (matches zoekt)
5. **Case Folding:** Trigram variant expansion via `generate_case_ngrams()` — no brute force

---

## Reference Files in zoekt/

| Purpose | Go File | Notes |
|---------|---------|-------|
| Search execution | `index/indexdata.go` | Main search logic |
| Hit iteration | `index/hititer.go` | Posting intersection |
| Content provider | `index/contentprovider.go` | Context extraction |
| Regex literals | `internal/syntaxutil/regexp.go` | Literal extraction |
| Case folding | `index/bits.go:26-47` | Unicode SimpleFold |
