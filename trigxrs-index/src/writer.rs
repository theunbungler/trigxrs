//! Index writer with tagged sections for extensibility
//!
//! The index format uses tagged sections to allow forward compatibility.
//! Unknown tags are skipped when reading, enabling extensions to add
//! custom sections without breaking older readers.
//!
//! Format:
//! ```text
//! Header (32 bytes):
//!   magic: [u8; 4]         // "ZRST"
//!   version: u32
//!   flags: u32
//!   reserved: [u8; 20]
//!
//! Sections (variable):
//!   For each section:
//!     tag_len: varint
//!     tag: [u8; tag_len]
//!     section_len: u64
//!     section_data: [u8; section_len]
//!
//! Footer:
//!   toc_offset: u64        // Offset to table of contents
//!   magic: [u8; 4]         // "ZRST" (repeated for validation)
//! ```

use std::collections::HashMap;
use std::io::{self, Seek, Write};

use trigxrs_core::{encode_varint, MAX_VARINT_LEN};

use crate::error::{Error, Result};

/// Magic bytes for index files
pub const INDEX_MAGIC: &[u8; 4] = b"ZRST";

/// Current index format version
pub const INDEX_VERSION: u32 = 1;

/// Header size in bytes
pub const HEADER_SIZE: usize = 32;

/// Footer size in bytes
pub const FOOTER_SIZE: usize = 12;

/// Section tags for core index components
pub mod tags {
    pub const TRIGRAMS: &str = "trigrams";
    pub const POSTINGS: &str = "postings";
    pub const POSTING_INDEX: &str = "posting_index";
    pub const CONTENTS: &str = "contents";
    pub const CONTENT_BOUNDS: &str = "content_bounds";
    pub const FILES: &str = "files";
    pub const NEWLINES: &str = "newlines";
    pub const METADATA: &str = "metadata";
    pub const BTREE: &str = "btree";
    pub const RUNE_OFFSETS: &str = "rune_offsets";
    pub const END_RUNES: &str = "end_runes";
}

/// Index header
#[derive(Debug, Clone)]
pub struct IndexHeader {
    pub version: u32,
    pub flags: u32,
}

impl IndexHeader {
    pub fn new() -> Self {
        Self {
            version: INDEX_VERSION,
            flags: 0,
        }
    }

    pub fn write<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        writer.write_all(INDEX_MAGIC)?;
        writer.write_all(&self.version.to_le_bytes())?;
        writer.write_all(&self.flags.to_le_bytes())?;
        writer.write_all(&[0u8; 20])?; // reserved
        Ok(())
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_SIZE {
            return Err(Error::InvalidIndex("header too short".into()));
        }

        if &bytes[0..4] != INDEX_MAGIC {
            return Err(Error::InvalidIndex("invalid magic bytes".into()));
        }

        let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        let flags = u32::from_le_bytes(bytes[8..12].try_into().unwrap());

        Ok(Self { version, flags })
    }
}

impl Default for IndexHeader {
    fn default() -> Self {
        Self::new()
    }
}

/// Entry in the table of contents
#[derive(Debug, Clone)]
pub struct TocEntry {
    pub tag: String,
    pub offset: u64,
    pub length: u64,
}

/// Table of contents for the index
#[derive(Debug, Clone, Default)]
pub struct TableOfContents {
    pub entries: Vec<TocEntry>,
}

impl TableOfContents {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn add(&mut self, tag: String, offset: u64, length: u64) {
        self.entries.push(TocEntry { tag, offset, length });
    }

    pub fn get(&self, tag: &str) -> Option<&TocEntry> {
        self.entries.iter().find(|e| e.tag == tag)
    }

    /// Serialize the TOC
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut varint_buf = [0u8; MAX_VARINT_LEN];

        // Write entry count
        let n = encode_varint(self.entries.len() as u64, &mut varint_buf);
        buf.extend_from_slice(&varint_buf[..n]);

        for entry in &self.entries {
            // Write tag length and tag
            let n = encode_varint(entry.tag.len() as u64, &mut varint_buf);
            buf.extend_from_slice(&varint_buf[..n]);
            buf.extend_from_slice(entry.tag.as_bytes());

            // Write offset and length
            buf.extend_from_slice(&entry.offset.to_le_bytes());
            buf.extend_from_slice(&entry.length.to_le_bytes());
        }

        buf
    }

    /// Deserialize the TOC
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let mut entries = Vec::new();
        let mut pos = 0;

        // Read entry count
        let (count, n) = trigxrs_core::decode_varint(&data[pos..])?;
        pos += n;

        for _ in 0..count {
            // Read tag length
            let (tag_len, n) = trigxrs_core::decode_varint(&data[pos..])?;
            pos += n;

            // Read tag
            let tag_end = pos + tag_len as usize;
            if tag_end > data.len() {
                return Err(Error::InvalidIndex("truncated TOC tag".into()));
            }
            let tag = String::from_utf8_lossy(&data[pos..tag_end]).into_owned();
            pos = tag_end;

            // Read offset and length
            if pos + 16 > data.len() {
                return Err(Error::InvalidIndex("truncated TOC entry".into()));
            }
            let offset = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
            let length = u64::from_le_bytes(data[pos + 8..pos + 16].try_into().unwrap());
            pos += 16;

            entries.push(TocEntry { tag, offset, length });
        }

        Ok(Self { entries })
    }
}

/// Helper for writing sections to an index
pub struct SectionWriter<W: Write + Seek> {
    writer: W,
    toc: TableOfContents,
    section_starts: HashMap<String, u64>,
}

impl<W: Write + Seek> SectionWriter<W> {
    /// Create a new section writer
    pub fn new(mut writer: W) -> io::Result<Self> {
        // Write header
        let header = IndexHeader::new();
        header.write(&mut writer)?;

        Ok(Self {
            writer,
            toc: TableOfContents::new(),
            section_starts: HashMap::new(),
        })
    }

    /// Start a new section with the given tag
    pub fn start_section(&mut self, tag: &str) -> io::Result<()> {
        let offset = self.writer.stream_position()?;
        self.section_starts.insert(tag.to_string(), offset);
        Ok(())
    }

    /// End the current section
    pub fn end_section(&mut self, tag: &str) -> io::Result<()> {
        let end = self.writer.stream_position()?;
        if let Some(start) = self.section_starts.remove(tag) {
            let length = end - start;
            self.toc.add(tag.to_string(), start, length);
        }
        Ok(())
    }

    /// Write a complete section with data
    pub fn write_section(&mut self, tag: &str, data: &[u8]) -> io::Result<()> {
        self.start_section(tag)?;
        self.writer.write_all(data)?;
        self.end_section(tag)?;
        Ok(())
    }

    /// Get mutable access to the underlying writer
    pub fn writer_mut(&mut self) -> &mut W {
        &mut self.writer
    }

    /// Get the current write position
    pub fn position(&mut self) -> io::Result<u64> {
        self.writer.stream_position()
    }

    /// Finalize the index, writing TOC and footer
    pub fn finish(mut self) -> io::Result<W> {
        // Write TOC
        let toc_offset = self.writer.stream_position()?;
        let toc_data = self.toc.to_bytes();
        self.writer.write_all(&toc_data)?;

        // Write footer
        self.writer.write_all(&toc_offset.to_le_bytes())?;
        self.writer.write_all(INDEX_MAGIC)?;

        Ok(self.writer)
    }

    /// Get the current TOC (for inspection)
    pub fn toc(&self) -> &TableOfContents {
        &self.toc
    }
}

/// Helper for reading sections from an index
pub struct SectionReader<'a> {
    data: &'a [u8],
    toc: TableOfContents,
}

impl<'a> SectionReader<'a> {
    /// Create a new section reader from memory-mapped data
    pub fn new(data: &'a [u8]) -> Result<Self> {
        if data.len() < HEADER_SIZE + FOOTER_SIZE {
            return Err(Error::InvalidIndex("file too small".into()));
        }

        // Validate header
        let _header = IndexHeader::from_bytes(&data[..HEADER_SIZE])?;

        // Read footer
        let footer_start = data.len() - FOOTER_SIZE;
        let toc_offset = u64::from_le_bytes(data[footer_start..footer_start + 8].try_into().unwrap());

        // Validate footer magic
        if &data[footer_start + 8..] != INDEX_MAGIC {
            return Err(Error::InvalidIndex("invalid footer magic".into()));
        }

        // Read TOC
        let toc_start = toc_offset as usize;
        let toc_end = footer_start;
        if toc_start >= toc_end || toc_start >= data.len() {
            return Err(Error::InvalidIndex("invalid TOC offset".into()));
        }

        let toc = TableOfContents::from_bytes(&data[toc_start..toc_end])?;

        Ok(Self { data, toc })
    }

    /// Get a section by tag
    pub fn get_section(&self, tag: &str) -> Option<&'a [u8]> {
        let entry = self.toc.get(tag)?;
        let start = entry.offset as usize;
        let end = start + entry.length as usize;

        if end <= self.data.len() {
            Some(&self.data[start..end])
        } else {
            None
        }
    }

    /// Get the table of contents
    pub fn toc(&self) -> &TableOfContents {
        &self.toc
    }

    /// List all section tags
    pub fn tags(&self) -> Vec<&str> {
        self.toc.entries.iter().map(|e| e.tag.as_str()).collect()
    }

    /// Get the header
    pub fn header(&self) -> Result<IndexHeader> {
        IndexHeader::from_bytes(&self.data[..HEADER_SIZE])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_header_roundtrip() {
        let header = IndexHeader::new();
        let mut buf = Vec::new();
        header.write(&mut buf).unwrap();

        assert_eq!(buf.len(), HEADER_SIZE);
        assert_eq!(&buf[0..4], INDEX_MAGIC);

        let parsed = IndexHeader::from_bytes(&buf).unwrap();
        assert_eq!(parsed.version, header.version);
        assert_eq!(parsed.flags, header.flags);
    }

    #[test]
    fn test_toc_roundtrip() {
        let mut toc = TableOfContents::new();
        toc.add("section1".to_string(), 100, 50);
        toc.add("section2".to_string(), 150, 75);
        toc.add("unicode_тест".to_string(), 225, 25);

        let bytes = toc.to_bytes();
        let parsed = TableOfContents::from_bytes(&bytes).unwrap();

        assert_eq!(parsed.entries.len(), 3);
        assert_eq!(parsed.entries[0].tag, "section1");
        assert_eq!(parsed.entries[0].offset, 100);
        assert_eq!(parsed.entries[0].length, 50);
        assert_eq!(parsed.entries[2].tag, "unicode_тест");
    }

    #[test]
    fn test_section_writer_reader() {
        let mut buf = Cursor::new(Vec::new());

        // Write sections
        {
            let mut writer = SectionWriter::new(&mut buf).unwrap();
            writer.write_section("test1", b"hello").unwrap();
            writer.write_section("test2", b"world").unwrap();
            writer.finish().unwrap();
        }

        // Read sections
        let data = buf.into_inner();
        let reader = SectionReader::new(&data).unwrap();

        assert_eq!(reader.tags().len(), 2);
        assert_eq!(reader.get_section("test1"), Some(b"hello".as_slice()));
        assert_eq!(reader.get_section("test2"), Some(b"world".as_slice()));
        assert_eq!(reader.get_section("nonexistent"), None);
    }

    #[test]
    fn test_empty_index() {
        let mut buf = Cursor::new(Vec::new());

        {
            let writer = SectionWriter::new(&mut buf).unwrap();
            writer.finish().unwrap();
        }

        let data = buf.into_inner();
        let reader = SectionReader::new(&data).unwrap();

        assert!(reader.tags().is_empty());
    }

    #[test]
    fn test_large_section() {
        let mut buf = Cursor::new(Vec::new());
        let large_data: Vec<u8> = (0..10000).map(|i| (i % 256) as u8).collect();

        {
            let mut writer = SectionWriter::new(&mut buf).unwrap();
            writer.write_section("large", &large_data).unwrap();
            writer.finish().unwrap();
        }

        let data = buf.into_inner();
        let reader = SectionReader::new(&data).unwrap();

        assert_eq!(reader.get_section("large"), Some(large_data.as_slice()));
    }

    #[test]
    fn test_incremental_section() {
        let mut buf = Cursor::new(Vec::new());

        {
            let mut writer = SectionWriter::new(&mut buf).unwrap();

            writer.start_section("incremental").unwrap();
            writer.writer_mut().write_all(b"part1").unwrap();
            writer.writer_mut().write_all(b"part2").unwrap();
            writer.end_section("incremental").unwrap();

            writer.finish().unwrap();
        }

        let data = buf.into_inner();
        let reader = SectionReader::new(&data).unwrap();

        assert_eq!(
            reader.get_section("incremental"),
            Some(b"part1part2".as_slice())
        );
    }
}
