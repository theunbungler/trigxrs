//! Delta-varint encoding for posting lists
//!
//! Posting lists store sorted offsets that are often close together.
//! Delta encoding stores the difference between consecutive values,
//! and varint encoding uses fewer bytes for smaller values.
//!
//! Format:
//! - Size prefix (varint): number of values
//! - Delta-encoded values (varints): each value is the delta from the previous

use crate::error::{Error, Result};

/// Maximum bytes needed for a single varint (u64)
pub const MAX_VARINT_LEN: usize = 10;

/// Encode a u64 as a varint into a buffer
///
/// Returns the number of bytes written.
#[inline]
pub fn encode_varint(mut value: u64, buf: &mut [u8]) -> usize {
    let mut i = 0;
    while value >= 0x80 {
        buf[i] = (value as u8) | 0x80;
        value >>= 7;
        i += 1;
    }
    buf[i] = value as u8;
    i + 1
}

/// Decode a varint from a buffer
///
/// Returns (value, bytes_consumed) or error if invalid.
#[inline]
pub fn decode_varint(buf: &[u8]) -> Result<(u64, usize)> {
    let mut value: u64 = 0;
    let mut shift = 0;

    for (i, &byte) in buf.iter().enumerate() {
        if shift >= 64 {
            return Err(Error::VarIntOverflow);
        }

        value |= ((byte & 0x7F) as u64) << shift;

        if byte & 0x80 == 0 {
            return Ok((value, i + 1));
        }

        shift += 7;
    }

    Err(Error::VarIntUnexpectedEnd)
}

/// Encode a slice of u32 offsets using delta-varint encoding
///
/// The offsets should be sorted in ascending order. The output format is:
/// - Size (varint): number of offsets
/// - Deltas (varints): each delta is the difference from the previous offset
///
/// # Example
/// ```
/// use trigxrs_core::encode_deltas;
/// let offsets = vec![10, 15, 20, 100];
/// let encoded = encode_deltas(&offsets);
/// // Contains: size=4, deltas=[10, 5, 5, 80]
/// ```
pub fn encode_deltas(offsets: &[u32]) -> Vec<u8> {
    let mut buf = [0u8; MAX_VARINT_LEN];
    let mut result = Vec::with_capacity(offsets.len() * 2);

    // Write size prefix
    let n = encode_varint(offsets.len() as u64, &mut buf);
    result.extend_from_slice(&buf[..n]);

    // Write delta-encoded values
    let mut last = 0u32;
    for &offset in offsets {
        let delta = offset.saturating_sub(last);
        last = offset;

        let n = encode_varint(delta as u64, &mut buf);
        result.extend_from_slice(&buf[..n]);
    }

    result
}

/// Decode delta-varint encoded offsets
///
/// Returns the decoded offsets.
///
/// # Example
/// ```
/// use trigxrs_core::{encode_deltas, decode_deltas};
/// let offsets = vec![10, 15, 20, 100];
/// let encoded = encode_deltas(&offsets);
/// let decoded = decode_deltas(&encoded).unwrap();
/// assert_eq!(offsets, decoded);
/// ```
pub fn decode_deltas(data: &[u8]) -> Result<Vec<u32>> {
    if data.is_empty() {
        return Ok(Vec::new());
    }

    let (size, mut pos) = decode_varint(data)?;
    let mut result = Vec::with_capacity(size as usize);
    let mut last = 0u32;

    while pos < data.len() {
        let (delta, n) = decode_varint(&data[pos..])?;
        pos += n;

        let offset = last.saturating_add(delta as u32);
        last = offset;
        result.push(offset);
    }

    Ok(result)
}

/// Decode deltas into a provided buffer to avoid allocation
///
/// Returns the number of values decoded.
pub fn decode_deltas_into(data: &[u8], output: &mut Vec<u32>) -> Result<usize> {
    output.clear();

    if data.is_empty() {
        return Ok(0);
    }

    let (size, mut pos) = decode_varint(data)?;
    output.reserve(size as usize);
    let mut last = 0u32;

    while pos < data.len() {
        let (delta, n) = decode_varint(&data[pos..])?;
        pos += n;

        let offset = last.saturating_add(delta as u32);
        last = offset;
        output.push(offset);
    }

    Ok(output.len())
}

/// Iterator over delta-encoded values without allocating
pub struct DeltaIterator<'a> {
    data: &'a [u8],
    pos: usize,
    last: u32,
    remaining: usize,
}

impl<'a> DeltaIterator<'a> {
    /// Create a new iterator over delta-encoded data
    pub fn new(data: &'a [u8]) -> Result<Self> {
        if data.is_empty() {
            return Ok(Self {
                data,
                pos: 0,
                last: 0,
                remaining: 0,
            });
        }

        let (size, pos) = decode_varint(data)?;
        Ok(Self {
            data,
            pos,
            last: 0,
            remaining: size as usize,
        })
    }

    /// Get the number of remaining values
    pub fn remaining(&self) -> usize {
        self.remaining
    }
}

impl Iterator for DeltaIterator<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 || self.pos >= self.data.len() {
            return None;
        }

        let (delta, n) = decode_varint(&self.data[self.pos..]).ok()?;
        self.pos += n;
        self.remaining -= 1;

        let offset = self.last.saturating_add(delta as u32);
        self.last = offset;
        Some(offset)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for DeltaIterator<'_> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_varint_small() {
        let mut buf = [0u8; MAX_VARINT_LEN];
        let n = encode_varint(127, &mut buf);
        assert_eq!(n, 1);
        assert_eq!(buf[0], 127);

        let (value, consumed) = decode_varint(&buf[..n]).unwrap();
        assert_eq!(value, 127);
        assert_eq!(consumed, 1);
    }

    #[test]
    fn test_varint_medium() {
        let mut buf = [0u8; MAX_VARINT_LEN];
        let n = encode_varint(300, &mut buf);
        assert_eq!(n, 2);

        let (value, consumed) = decode_varint(&buf[..n]).unwrap();
        assert_eq!(value, 300);
        assert_eq!(consumed, 2);
    }

    #[test]
    fn test_varint_large() {
        let mut buf = [0u8; MAX_VARINT_LEN];
        let test_values = [0, 1, 127, 128, 255, 256, 16383, 16384, u32::MAX as u64, u64::MAX];

        for &value in &test_values {
            let n = encode_varint(value, &mut buf);
            let (decoded, consumed) = decode_varint(&buf[..n]).unwrap();
            assert_eq!(decoded, value, "Failed for value {}", value);
            assert_eq!(consumed, n);
        }
    }

    #[test]
    fn test_deltas_empty() {
        let encoded = encode_deltas(&[]);
        let decoded = decode_deltas(&encoded).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn test_deltas_single() {
        let offsets = vec![42];
        let encoded = encode_deltas(&offsets);
        let decoded = decode_deltas(&encoded).unwrap();
        assert_eq!(offsets, decoded);
    }

    #[test]
    fn test_deltas_sequential() {
        let offsets: Vec<u32> = (0..100).collect();
        let encoded = encode_deltas(&offsets);
        let decoded = decode_deltas(&encoded).unwrap();
        assert_eq!(offsets, decoded);
    }

    #[test]
    fn test_deltas_sparse() {
        let offsets = vec![10, 100, 1000, 10000, 100000];
        let encoded = encode_deltas(&offsets);
        let decoded = decode_deltas(&encoded).unwrap();
        assert_eq!(offsets, decoded);
    }

    #[test]
    fn test_deltas_compression() {
        // Sequential offsets should compress well
        let offsets: Vec<u32> = (0..1000).collect();
        let encoded = encode_deltas(&offsets);
        // Each delta is 1, which takes 1 byte, plus size prefix
        assert!(encoded.len() < offsets.len() * 4); // Much better than 4 bytes per value
    }

    #[test]
    fn test_delta_iterator() {
        let offsets = vec![5, 10, 15, 100, 200];
        let encoded = encode_deltas(&offsets);
        let iter = DeltaIterator::new(&encoded).unwrap();
        let decoded: Vec<u32> = iter.collect();
        assert_eq!(offsets, decoded);
    }

    #[test]
    fn test_delta_iterator_size_hint() {
        let offsets = vec![1, 2, 3, 4, 5];
        let encoded = encode_deltas(&offsets);
        let iter = DeltaIterator::new(&encoded).unwrap();
        assert_eq!(iter.size_hint(), (5, Some(5)));
    }

    #[test]
    fn test_decode_deltas_into() {
        let offsets = vec![10, 20, 30];
        let encoded = encode_deltas(&offsets);
        let mut output = Vec::new();
        let count = decode_deltas_into(&encoded, &mut output).unwrap();
        assert_eq!(count, 3);
        assert_eq!(output, offsets);
    }

    #[test]
    fn test_varint_decode_error_empty() {
        let result = decode_varint(&[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_varint_decode_error_incomplete() {
        // High bit set but no continuation
        let result = decode_varint(&[0x80]);
        assert!(result.is_err());
    }
}
