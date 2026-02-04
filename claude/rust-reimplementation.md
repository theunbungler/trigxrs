# Zoekt Rust Reimplementation Plan

This document captures the assessment and plan for reimplementing zoekt's indexing and regex searching in Rust.

## Scope

**In scope:**
- Trigram-based indexing (positional trigrams)
- Regex searching with trigram acceleration
- Simple filters via API/CLI (repo, branch, file path)

**Out of scope:**
- Binary compatibility with Go zoekt indexes
- Full query string parsing (the `query/parse.go` complexity)
- Complex boolean query combinations parsed from strings

## Effort Estimate

**13-20 person-weeks (3-5 months)** for one experienced Rust developer.

| Component | Effort | Notes |
|-----------|--------|-------|
| Trigram indexing + B+-tree | 2-3 weeks | Core algorithm |
| Index format (custom) | 1-2 weeks | Design for Rust/mmap |
| Index building | 3-4 weeks | Document processing, shard writing |
| Regex trigram extraction | 1-2 weeks | Parse regex AST for literals |
| Search + verification | 2-3 weeks | Posting intersection, content matching |
| Filter application | 1 week | Repo, branch, file path |
| CLI/API layer | 1 week | |
| Testing | 2-3 weeks | |

## Core Algorithm: Positional Trigrams

Zoekt's key innovation is storing **byte offsets** with each trigram occurrence:

```
"banana" → {"ban": [0], "ana": [1,3], "nan": [2]}
```

To search for "anana":
1. Look up "ana" and "nan" posting lists
2. Find where "nan" appears at offset +1 from "ana"
3. Verify with actual string comparison

This allows fast substring search without scanning all content.

## Architecture

```
┌─────────────────────────────────────────┐
│  CLI / Library API                      │
│  - file patterns (glob/regex)           │
│  - repo filter, branch filter           │
│  - search regex pattern                 │
└────────────────┬────────────────────────┘
                 │
┌────────────────▼────────────────────────┐
│  Search Engine                          │
│  1. Extract trigrams from regex         │
│  2. Query posting lists                 │
│  3. Apply filters (file/repo/branch)    │
│  4. Verify matches with full regex      │
│  5. Extract context (lines around match)│
└────────────────┬────────────────────────┘
                 │
┌────────────────▼────────────────────────┐
│  Index (custom Rust format)             │
│  - Trigram B+-tree                      │
│  - Posting lists (doc_id, byte_offset)  │
│  - Compressed file contents             │
│  - Metadata (repo, branch, path)        │
└─────────────────────────────────────────┘
```

## Key Components to Implement

### 1. Trigram Encoding (Reference: `index/bits.go`)

```rust
// Trigram as u32 (3 bytes)
fn trigram(a: u8, b: u8, c: u8) -> u32 {
    ((a as u32) << 16) | ((b as u32) << 8) | (c as u32)
}

// Extract trigrams from content
fn extract_trigrams(content: &[u8]) -> HashMap<u32, Vec<u32>> {
    let mut result = HashMap::new();
    for i in 0..content.len().saturating_sub(2) {
        let ng = trigram(content[i], content[i+1], content[i+2]);
        result.entry(ng).or_default().push(i as u32);
    }
    result
}
```

### 2. Regex Literal Extraction (Reference: `internal/syntaxutil/`)

Use `regex-syntax` crate to parse regex and extract required literal strings:

```rust
use regex_syntax::ast::{Ast, parse};

fn extract_literals(pattern: &str) -> Vec<String> {
    let ast = parse::Parser::new().parse(pattern).unwrap();
    // Walk AST, collect Literal nodes
    // Handle Concat (combine), Alternation (intersection), etc.
    // Return strings of length >= 3 for trigram matching
}
```

This is ~300-500 lines of AST walking logic.

### 3. Posting List Intersection

```rust
fn intersect_with_offset(
    list_a: &[u32],  // positions of trigram A
    list_b: &[u32],  // positions of trigram B
    offset: u32,     // B should appear at A + offset
) -> Vec<u32> {
    // Two-pointer merge finding positions where both match
    let mut result = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < list_a.len() && j < list_b.len() {
        let target = list_a[i] + offset;
        if list_b[j] == target {
            result.push(list_a[i]);
            i += 1;
            j += 1;
        } else if list_b[j] < target {
            j += 1;
        } else {
            i += 1;
        }
    }
    result
}
```

### 4. Index Format (Custom)

Design a simple mmap-friendly format:

```
Header:
  magic: [u8; 4]       // "ZRST"
  version: u32
  num_files: u32
  trigram_index_offset: u64
  content_offset: u64
  metadata_offset: u64

Trigram Index (B+-tree or sorted array):
  For each trigram:
    trigram: u32
    posting_list_offset: u64
    posting_list_len: u32

Posting Lists:
  Varint-encoded arrays of (file_id: u32, byte_offset: u32)

File Contents:
  Compressed content blocks (zstd)

Metadata:
  JSON or bincode: file paths, repos, branches, sizes
```

## Useful Rust Crates

| Need | Crate |
|------|-------|
| Regex matching | `regex` |
| Regex AST parsing | `regex-syntax` |
| Memory mapping | `memmap2` |
| Varint encoding | `integer-encoding` or custom |
| Compression | `zstd` |
| Serialization | `serde`, `bincode` |
| CLI | `clap` |
| Roaring bitmaps (optional) | `roaring` |

## Reference Files in Go Codebase

Key files to study:

| File | Purpose |
|------|---------|
| `index/bits.go` | Trigram encoding, case folding |
| `index/btree.go` | B+-tree for trigram index |
| `index/builder.go` | Index construction |
| `index/read.go` | Index reading, mmap |
| `index/eval.go` | Search execution |
| `index/contentprovider.go` | Match extraction, context |
| `internal/syntaxutil/regexp.go` | Regex literal extraction |
| `doc/design.md` | Algorithm documentation |

## Implementation Phases

### Phase 1: Index Building (Weeks 1-5)
- [ ] Trigram extraction from files
- [ ] In-memory trigram → posting list map
- [ ] Serialize to custom index format
- [ ] Basic CLI: `zoekt-index <dir> -o index.zrst`

### Phase 2: Basic Search (Weeks 6-9)
- [ ] Load index via mmap
- [ ] Literal string search (no regex yet)
- [ ] Posting list intersection
- [ ] Content verification
- [ ] CLI: `zoekt-search <index> "literal"`

### Phase 3: Regex Search (Weeks 10-13)
- [ ] Regex literal extraction using `regex-syntax`
- [ ] Trigram selection (pick rarest trigrams)
- [ ] Full regex verification on candidates
- [ ] CLI: `zoekt-search <index> "foo.*bar"`

### Phase 4: Filters & Polish (Weeks 14-17)
- [ ] File path filtering (glob patterns)
- [ ] Repository/branch metadata
- [ ] Context lines around matches
- [ ] Performance optimization
- [ ] Library API for embedding

## Performance Considerations

1. **Trigram selection**: Choose trigrams with smallest posting lists
2. **Early termination**: Stop after N results if limit specified
3. **Memory mapping**: Avoid loading entire index into memory
4. **Parallel search**: Search multiple shards concurrently
5. **Content compression**: zstd provides good speed/ratio tradeoff

## Open Questions

1. **Shard size**: How large should each index shard be? (Go uses ~1GB content limit)
2. **Unicode handling**: Store byte offsets or rune offsets? (Go uses runes with mapping table)
3. **Incremental indexing**: Support updating index without full rebuild?
4. **Symbol support**: Include ctags integration or defer?
