//! Trigram (ngram) encoding and extraction
//!
//! A trigram is a sequence of 3 Unicode code points (runes) encoded as a 64-bit integer.
//! Each rune uses 21 bits (enough for the full Unicode range), for a total of 63 bits.
//!
//! Encoding: `(rune0 << 42) | (rune1 << 21) | rune2`

/// Size of an ngram in runes
pub const NGRAM_SIZE: usize = 3;

/// Mask for a single rune (21 bits)
pub const RUNE_MASK: u64 = (1 << 21) - 1;

/// A 64-bit encoded trigram (3 runes)
pub type Ngram = u64;

/// An ngram with its byte offset in the source text
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NgramOffset {
    /// The encoded trigram
    pub ngram: Ngram,
    /// Byte offset of the first rune of this trigram in the source
    pub byte_offset: u32,
    /// Index of this ngram in the sequence (for ordering)
    pub index: u32,
}

impl NgramOffset {
    pub fn new(ngram: Ngram, byte_offset: u32, index: u32) -> Self {
        Self {
            ngram,
            byte_offset,
            index,
        }
    }
}

impl PartialOrd for NgramOffset {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for NgramOffset {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.ngram
            .cmp(&other.ngram)
            .then_with(|| self.index.cmp(&other.index))
    }
}

/// Encode 3 runes (Unicode code points) into a 64-bit ngram
///
/// # Example
/// ```
/// use trigxrs_core::runes_to_ngram;
/// let ng = runes_to_ngram(['a', 'b', 'c']);
/// assert_eq!(ng, (('a' as u64) << 42) | (('b' as u64) << 21) | ('c' as u64));
/// ```
#[inline]
pub fn runes_to_ngram(runes: [char; NGRAM_SIZE]) -> Ngram {
    let r0 = runes[0] as u64;
    let r1 = runes[1] as u64;
    let r2 = runes[2] as u64;
    (r0 << 42) | (r1 << 21) | r2
}

/// Decode a 64-bit ngram back into 3 runes
///
/// # Example
/// ```
/// use trigxrs_core::{runes_to_ngram, ngram_to_runes};
/// let runes = ['a', 'b', 'c'];
/// let ng = runes_to_ngram(runes);
/// assert_eq!(ngram_to_runes(ng), runes);
/// ```
#[inline]
pub fn ngram_to_runes(ng: Ngram) -> [char; NGRAM_SIZE] {
    let r0 = ((ng >> 42) & RUNE_MASK) as u32;
    let r1 = ((ng >> 21) & RUNE_MASK) as u32;
    let r2 = (ng & RUNE_MASK) as u32;

    // These are safe because we mask to 21 bits, which is within Unicode range
    [
        char::from_u32(r0).unwrap_or('\u{FFFD}'),
        char::from_u32(r1).unwrap_or('\u{FFFD}'),
        char::from_u32(r2).unwrap_or('\u{FFFD}'),
    ]
}

/// Convert an ngram to a string for display
pub fn ngram_to_string(ng: Ngram) -> String {
    let runes = ngram_to_runes(ng);
    runes.iter().collect()
}

/// Split text into trigrams with their byte offsets
///
/// Returns a vector of (ngram, byte_offset) pairs. The byte_offset points to
/// the first byte of the first rune in each trigram.
///
/// # Example
/// ```
/// use trigxrs_core::split_ngrams;
/// let ngrams = split_ngrams("abcd");
/// assert_eq!(ngrams.len(), 2); // "abc" and "bcd"
/// ```
pub fn split_ngrams(text: &str) -> Vec<NgramOffset> {
    let bytes = text.as_bytes();
    if bytes.len() < 3 {
        return Vec::new();
    }

    let mut result = Vec::with_capacity(bytes.len().saturating_sub(2));
    let mut rune_window: [char; 3] = ['\0', '\0', '\0'];
    let mut offset_window: [u32; 3] = [0, 0, 0];
    let mut rune_count = 0u32;
    let mut byte_pos = 0u32;
    let mut index = 0u32;

    for ch in text.chars() {
        let char_len = ch.len_utf8() as u32;

        // Shift the window
        rune_window[0] = rune_window[1];
        offset_window[0] = offset_window[1];
        rune_window[1] = rune_window[2];
        offset_window[1] = offset_window[2];
        rune_window[2] = ch;
        offset_window[2] = byte_pos;

        byte_pos += char_len;
        rune_count += 1;

        if rune_count < NGRAM_SIZE as u32 {
            continue;
        }

        let ng = runes_to_ngram(rune_window);
        result.push(NgramOffset::new(ng, offset_window[0], index));
        index += 1;
    }

    result
}

/// Split text into trigrams and return just the ngram values (no offsets)
pub fn split_ngrams_simple(text: &str) -> Vec<Ngram> {
    split_ngrams(text).into_iter().map(|no| no.ngram).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runes_to_ngram_ascii() {
        let ng = runes_to_ngram(['a', 'b', 'c']);
        assert_eq!(ng, (97 << 42) | (98 << 21) | 99);
    }

    #[test]
    fn test_ngram_roundtrip_ascii() {
        let runes = ['a', 'b', 'c'];
        let ng = runes_to_ngram(runes);
        assert_eq!(ngram_to_runes(ng), runes);
    }

    #[test]
    fn test_ngram_roundtrip_unicode() {
        let runes = ['日', '本', '語'];
        let ng = runes_to_ngram(runes);
        assert_eq!(ngram_to_runes(ng), runes);
    }

    #[test]
    fn test_ngram_roundtrip_emoji() {
        let runes = ['😀', '🎉', '🚀'];
        let ng = runes_to_ngram(runes);
        assert_eq!(ngram_to_runes(ng), runes);
    }

    #[test]
    fn test_ngram_to_string() {
        let ng = runes_to_ngram(['a', 'b', 'c']);
        assert_eq!(ngram_to_string(ng), "abc");
    }

    #[test]
    fn test_split_ngrams_short() {
        assert!(split_ngrams("").is_empty());
        assert!(split_ngrams("a").is_empty());
        assert!(split_ngrams("ab").is_empty());
    }

    #[test]
    fn test_split_ngrams_exact() {
        let ngrams = split_ngrams("abc");
        assert_eq!(ngrams.len(), 1);
        assert_eq!(ngram_to_string(ngrams[0].ngram), "abc");
        assert_eq!(ngrams[0].byte_offset, 0);
    }

    #[test]
    fn test_split_ngrams_multiple() {
        let ngrams = split_ngrams("abcde");
        assert_eq!(ngrams.len(), 3); // abc, bcd, cde
        assert_eq!(ngram_to_string(ngrams[0].ngram), "abc");
        assert_eq!(ngram_to_string(ngrams[1].ngram), "bcd");
        assert_eq!(ngram_to_string(ngrams[2].ngram), "cde");
        assert_eq!(ngrams[0].byte_offset, 0);
        assert_eq!(ngrams[1].byte_offset, 1);
        assert_eq!(ngrams[2].byte_offset, 2);
    }

    #[test]
    fn test_split_ngrams_unicode() {
        let text = "日本語";
        let ngrams = split_ngrams(text);
        assert_eq!(ngrams.len(), 1);
        assert_eq!(ngram_to_string(ngrams[0].ngram), "日本語");
        assert_eq!(ngrams[0].byte_offset, 0);
    }

    #[test]
    fn test_split_ngrams_unicode_offsets() {
        let text = "日本語文"; // 4 characters, 12 bytes (3 bytes each)
        let ngrams = split_ngrams(text);
        assert_eq!(ngrams.len(), 2);
        assert_eq!(ngrams[0].byte_offset, 0);
        assert_eq!(ngrams[1].byte_offset, 3); // Second ngram starts at byte 3
    }

    #[test]
    fn test_split_ngrams_mixed() {
        let text = "aあb"; // 1 + 3 + 1 = 5 bytes
        let ngrams = split_ngrams(text);
        assert_eq!(ngrams.len(), 1);
        assert_eq!(ngram_to_string(ngrams[0].ngram), "aあb");
        assert_eq!(ngrams[0].byte_offset, 0);
    }

    #[test]
    fn test_ngram_offset_ordering() {
        let ng1 = NgramOffset::new(100, 0, 0);
        let ng2 = NgramOffset::new(200, 0, 0);
        let ng3 = NgramOffset::new(100, 0, 1);

        assert!(ng1 < ng2);
        assert!(ng1 < ng3); // Same ngram, but lower index
    }

    #[test]
    fn test_split_ngrams_indices() {
        let ngrams = split_ngrams("abcdef");
        for (i, ng) in ngrams.iter().enumerate() {
            assert_eq!(ng.index, i as u32);
        }
    }
}
