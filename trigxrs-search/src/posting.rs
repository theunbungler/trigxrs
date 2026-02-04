//! Posting list iteration for search
//!
//! Posting lists store rune offsets where trigrams occur.
//! This module provides iterators for:
//! - Single posting lists (compressed, delta-encoded)
//! - Distance-constrained intersection of two posting lists
//! - Merging multiple posting lists (OR operation)

use trigxrs_core::decode_varint;

/// Maximum value indicating end of iteration
pub const MAX_OFFSET: u32 = u32::MAX;

/// Hit iterator trait for finding potential matches
pub trait HitIterator {
    /// Return the current hit offset, or MAX_OFFSET if exhausted
    fn first(&self) -> u32;

    /// Advance past the given limit
    fn next(&mut self, limit: u32);

    /// Get statistics about bytes loaded
    fn bytes_loaded(&self) -> usize;
}

/// Iterator over a compressed, delta-encoded posting list
#[derive(Debug)]
pub struct CompressedPostingIterator<'a> {
    /// Remaining posting data
    data: &'a [u8],
    /// Current position in data
    pos: usize,
    /// Current offset value
    current: u32,
    /// Total bytes loaded
    bytes_loaded: usize,
}

impl<'a> CompressedPostingIterator<'a> {
    /// Create a new iterator over compressed posting data
    pub fn new(data: &'a [u8]) -> Self {
        if data.is_empty() {
            return Self {
                data,
                pos: 0,
                current: MAX_OFFSET,
                bytes_loaded: 0,
            };
        }

        // Read first value
        match decode_varint(data) {
            Ok((delta, n)) => Self {
                data,
                pos: n,
                current: delta as u32,
                bytes_loaded: n,
            },
            Err(_) => Self {
                data,
                pos: data.len(),
                current: MAX_OFFSET,
                bytes_loaded: 0,
            },
        }
    }

    /// Check if iterator is exhausted
    pub fn is_exhausted(&self) -> bool {
        self.current == MAX_OFFSET
    }
}

impl HitIterator for CompressedPostingIterator<'_> {
    fn first(&self) -> u32 {
        self.current
    }

    fn next(&mut self, limit: u32) {
        if limit == MAX_OFFSET {
            self.current = MAX_OFFSET;
            self.pos = self.data.len();
            return;
        }

        while self.current <= limit && self.pos < self.data.len() {
            match decode_varint(&self.data[self.pos..]) {
                Ok((delta, n)) => {
                    self.current = self.current.saturating_add(delta as u32);
                    self.pos += n;
                    self.bytes_loaded += n;
                }
                Err(_) => {
                    self.current = MAX_OFFSET;
                    break;
                }
            }
        }

        if self.current <= limit && self.pos >= self.data.len() {
            self.current = MAX_OFFSET;
        }
    }

    fn bytes_loaded(&self) -> usize {
        self.bytes_loaded
    }
}

/// Iterator that finds hits at a fixed distance apart
#[derive(Debug)]
pub struct DistanceIterator<I1, I2>
where
    I1: HitIterator,
    I2: HitIterator,
{
    iter1: I1,
    iter2: I2,
    distance: u32,
}

impl<I1, I2> DistanceIterator<I1, I2>
where
    I1: HitIterator,
    I2: HitIterator,
{
    /// Create a new distance iterator
    ///
    /// Finds positions where iter1 and iter2 have hits exactly `distance` apart.
    pub fn new(iter1: I1, iter2: I2, distance: u32) -> Self {
        let mut s = Self {
            iter1,
            iter2,
            distance,
        };
        s.find_next();
        s
    }

    fn find_next(&mut self) {
        loop {
            let p1 = self.iter1.first();
            let p2 = self.iter2.first();

            if p1 == MAX_OFFSET || p2 == MAX_OFFSET {
                self.iter1.next(MAX_OFFSET);
                break;
            }

            let target2 = p1.saturating_add(self.distance);

            if target2 < p2 {
                // iter1 is too far behind, advance it
                self.iter1.next(p2.saturating_sub(self.distance).saturating_sub(1));
            } else if target2 > p2 {
                // iter2 is too far behind, advance it
                self.iter2.next(target2.saturating_sub(1));
            } else {
                // Found a match!
                break;
            }
        }
    }
}

impl<I1, I2> HitIterator for DistanceIterator<I1, I2>
where
    I1: HitIterator,
    I2: HitIterator,
{
    fn first(&self) -> u32 {
        self.iter1.first()
    }

    fn next(&mut self, limit: u32) {
        self.iter1.next(limit);

        let limit2 = if limit == MAX_OFFSET {
            MAX_OFFSET
        } else {
            limit.saturating_add(self.distance)
        };
        self.iter2.next(limit2);

        self.find_next();
    }

    fn bytes_loaded(&self) -> usize {
        self.iter1.bytes_loaded() + self.iter2.bytes_loaded()
    }
}

/// Merging iterator for OR operation across multiple posting lists
#[derive(Debug)]
pub struct MergingIterator<I: HitIterator> {
    iters: Vec<I>,
}

impl<I: HitIterator> MergingIterator<I> {
    /// Create a new merging iterator
    pub fn new(iters: Vec<I>) -> Self {
        Self { iters }
    }
}

impl<I: HitIterator> HitIterator for MergingIterator<I> {
    fn first(&self) -> u32 {
        let mut min = MAX_OFFSET;
        for iter in &self.iters {
            let f = iter.first();
            if f < min {
                min = f;
            }
        }
        min
    }

    fn next(&mut self, limit: u32) {
        for iter in &mut self.iters {
            iter.next(limit);
        }
    }

    fn bytes_loaded(&self) -> usize {
        self.iters.iter().map(|i| i.bytes_loaded()).sum()
    }
}

/// In-memory iterator over a slice of offsets
#[derive(Debug)]
pub struct InMemoryIterator<'a> {
    postings: &'a [u32],
    pos: usize,
}

impl<'a> InMemoryIterator<'a> {
    pub fn new(postings: &'a [u32]) -> Self {
        Self { postings, pos: 0 }
    }
}

impl HitIterator for InMemoryIterator<'_> {
    fn first(&self) -> u32 {
        if self.pos < self.postings.len() {
            self.postings[self.pos]
        } else {
            MAX_OFFSET
        }
    }

    fn next(&mut self, limit: u32) {
        if limit == MAX_OFFSET {
            self.pos = self.postings.len();
            return;
        }

        while self.pos < self.postings.len() && self.postings[self.pos] <= limit {
            self.pos += 1;
        }
    }

    fn bytes_loaded(&self) -> usize {
        0 // In-memory, no disk reads
    }
}

/// Collect all hits from an iterator up to a limit
pub fn collect_hits<I: HitIterator>(iter: &mut I, max_hits: usize) -> Vec<u32> {
    let mut hits = Vec::with_capacity(max_hits.min(1000));

    loop {
        let hit = iter.first();
        if hit == MAX_OFFSET || hits.len() >= max_hits {
            break;
        }
        hits.push(hit);
        iter.next(hit);
    }

    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_compressed(offsets: &[u32]) -> Vec<u8> {
        // Manual delta + varint encoding (matching the posting format)
        let mut buf = Vec::new();
        let mut last = 0u32;
        for &offset in offsets {
            let delta = offset - last;
            last = offset;

            // Encode as varint
            let mut value = delta as u64;
            loop {
                let byte = (value & 0x7F) as u8;
                value >>= 7;
                if value == 0 {
                    buf.push(byte);
                    break;
                } else {
                    buf.push(byte | 0x80);
                }
            }
        }
        buf
    }

    #[test]
    fn test_compressed_iterator_empty() {
        let iter = CompressedPostingIterator::new(&[]);
        assert_eq!(iter.first(), MAX_OFFSET);
    }

    #[test]
    fn test_compressed_iterator_single() {
        let data = make_compressed(&[42]);
        let iter = CompressedPostingIterator::new(&data);
        assert_eq!(iter.first(), 42);
    }

    #[test]
    fn test_compressed_iterator_multiple() {
        let data = make_compressed(&[10, 20, 30, 100]);
        let mut iter = CompressedPostingIterator::new(&data);

        assert_eq!(iter.first(), 10);
        iter.next(10);
        assert_eq!(iter.first(), 20);
        iter.next(20);
        assert_eq!(iter.first(), 30);
        iter.next(50); // Skip past 30
        assert_eq!(iter.first(), 100);
        iter.next(100);
        assert_eq!(iter.first(), MAX_OFFSET);
    }

    #[test]
    fn test_compressed_iterator_skip() {
        let data = make_compressed(&[10, 20, 30, 40, 50]);
        let mut iter = CompressedPostingIterator::new(&data);

        assert_eq!(iter.first(), 10);
        iter.next(35); // Skip past 10, 20, 30
        assert_eq!(iter.first(), 40);
    }

    #[test]
    fn test_in_memory_iterator() {
        let postings = vec![5, 10, 15, 20];
        let mut iter = InMemoryIterator::new(&postings);

        assert_eq!(iter.first(), 5);
        iter.next(5);
        assert_eq!(iter.first(), 10);
        iter.next(12);
        assert_eq!(iter.first(), 15);
    }

    #[test]
    fn test_distance_iterator() {
        let data1 = make_compressed(&[0, 10, 20, 30]);
        let data2 = make_compressed(&[5, 15, 25, 35]);

        let iter1 = CompressedPostingIterator::new(&data1);
        let iter2 = CompressedPostingIterator::new(&data2);

        let mut dist_iter = DistanceIterator::new(iter1, iter2, 5);

        // Should find matches at: 0+5=5, 10+5=15, 20+5=25, 30+5=35
        assert_eq!(dist_iter.first(), 0);
        dist_iter.next(0);
        assert_eq!(dist_iter.first(), 10);
        dist_iter.next(10);
        assert_eq!(dist_iter.first(), 20);
        dist_iter.next(20);
        assert_eq!(dist_iter.first(), 30);
    }

    #[test]
    fn test_distance_iterator_no_match() {
        let data1 = make_compressed(&[0, 10, 20]);
        let data2 = make_compressed(&[3, 13, 23]); // Distance 3, not 5

        let iter1 = CompressedPostingIterator::new(&data1);
        let iter2 = CompressedPostingIterator::new(&data2);

        let dist_iter = DistanceIterator::new(iter1, iter2, 5);
        assert_eq!(dist_iter.first(), MAX_OFFSET);
    }

    #[test]
    fn test_merging_iterator() {
        let data1 = make_compressed(&[5, 15, 25]);
        let data2 = make_compressed(&[10, 20, 30]);

        let iter1 = CompressedPostingIterator::new(&data1);
        let iter2 = CompressedPostingIterator::new(&data2);

        let mut merge = MergingIterator::new(vec![iter1, iter2]);

        // Should return min of all iterators
        assert_eq!(merge.first(), 5);
        merge.next(5);
        assert_eq!(merge.first(), 10);
        merge.next(10);
        assert_eq!(merge.first(), 15);
    }

    #[test]
    fn test_collect_hits() {
        let data = make_compressed(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        let mut iter = CompressedPostingIterator::new(&data);

        let hits = collect_hits(&mut iter, 5);
        assert_eq!(hits, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_collect_all_hits() {
        let data = make_compressed(&[1, 2, 3]);
        let mut iter = CompressedPostingIterator::new(&data);

        let hits = collect_hits(&mut iter, 100);
        assert_eq!(hits, vec![1, 2, 3]);
    }
}
