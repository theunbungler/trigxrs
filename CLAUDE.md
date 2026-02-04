# CLAUDE.md

## Project Overview

**trigxrs** is a Rust reimplementation of [zoekt](https://github.com/sourcegraph/zoekt)'s core trigram-based code search. Focus is on indexing and regex search acceleration, not full query grammar or Go index compatibility.

## Quick Start

```bash
# Build
cargo build

# Test (73 tests)
cargo test

# Check specific crate
cargo test -p trigxrs-core
cargo test -p trigxrs-index
cargo test -p trigxrs-search
```

## Project Structure

```
trigxrs-core/     # Pure algorithms: trigram encoding, varint, B+-tree
trigxrs-index/    # Index building: postings, writer, IndexBuilder
trigxrs-search/   # Search execution: reader, posting iterators
trigxrs-cli/      # CLI tools (stub)
```

## Implementation Status

**Check `IMPLEMENTATION_STATUS.md` for current progress and next steps.**

Phases 1-3 are complete (foundation, index building, index reading). Phase 4 (literal search) is next.

## Key Design Decisions

- **Index format:** Custom tagged-section format, not Go-compatible
- **Extension trait:** `IndexExtension` for bolt-on features (symbols, language detection)
- **Posting format:** Delta-varint encoded rune offsets
- **Memory mapping:** `memmap2` crate for index access

## Reference Implementation

The `zoekt/` submodule contains the Go reference implementation. Key files:

| Purpose | File |
|---------|------|
| Trigram encoding | `zoekt/index/bits.go` |
| B+-tree | `zoekt/index/btree.go` |
| Index building | `zoekt/index/shard_builder.go` |
| Index writing | `zoekt/index/write.go` |
| Index reading | `zoekt/index/read.go` |
| Search | `zoekt/index/indexdata.go` |
| Hit iteration | `zoekt/index/hititer.go` |

## Code Style

- Follow Rust idioms
- Write tests alongside implementation
- Use `thiserror` for error types
- Prefer `bincode` for serialization within index sections
