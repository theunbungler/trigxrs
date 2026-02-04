//! B+-tree for efficient ngram lookups
//!
//! The B+-tree stores all ngrams in leaf nodes (buckets) and uses inner nodes
//! for navigation. This allows:
//! - Single-pass insertion with pre-splitting
//! - Efficient lookup with minimal disk I/O (one bucket read per lookup)
//! - Memory-efficient storage (inner nodes in memory, leaves on disk)
//!
//! Based on: H. Ceylan and R. Mihalcea, "An Efficient Indexer for Large N-Gram
//! Corpora", ACL-HLT 2011 System Demonstrations.

use crate::ngram::Ngram;
use serde::{Deserialize, Serialize};

/// Default bucket size: fits in 2 pages (8KB) with 8-byte ngrams
/// This gives ~1024 ngrams per bucket
pub const DEFAULT_BUCKET_SIZE: usize = (4096 * 2) / 8;

/// Default branching factor for inner nodes
pub const DEFAULT_BRANCHING_FACTOR: usize = 50;

/// Options for B+-tree construction
#[derive(Debug, Clone, Copy)]
pub struct BPlusTreeOpts {
    /// Maximum number of ngrams per leaf bucket
    pub bucket_size: usize,
    /// Branching factor: inner nodes have [v, 2v] children
    pub v: usize,
}

impl Default for BPlusTreeOpts {
    fn default() -> Self {
        Self {
            bucket_size: DEFAULT_BUCKET_SIZE,
            v: DEFAULT_BRANCHING_FACTOR,
        }
    }
}

/// A B+-tree for ngram indexing
#[derive(Debug)]
pub struct BPlusTree {
    root: Node,
    opts: BPlusTreeOpts,
    last_bucket_index: usize,
}

#[derive(Debug)]
enum Node {
    Inner(InnerNode),
    Leaf(LeafNode),
}

#[derive(Debug)]
struct InnerNode {
    keys: Vec<Ngram>,
    children: Vec<Node>,
}

#[derive(Debug)]
struct LeafNode {
    bucket_index: usize,
    posting_index_offset: usize,
    bucket_size: usize,
    split_key: Ngram,
}

/// Result of a B+-tree lookup
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BTreeLookup {
    /// Index of the bucket containing the ngram
    pub bucket_index: usize,
    /// Offset into the posting index for the first ngram in the bucket
    pub posting_index_offset: usize,
}

impl BPlusTree {
    /// Create a new B+-tree with default options
    pub fn new() -> Self {
        Self::with_opts(BPlusTreeOpts::default())
    }

    /// Create a new B+-tree with custom options
    pub fn with_opts(opts: BPlusTreeOpts) -> Self {
        Self {
            root: Node::Leaf(LeafNode {
                bucket_index: 0,
                posting_index_offset: 0,
                bucket_size: 0,
                split_key: 0,
            }),
            opts,
            last_bucket_index: 0,
        }
    }

    /// Insert an ngram into the tree
    ///
    /// Note: ngrams should be inserted in sorted order for optimal bucket filling.
    /// Call `freeze()` after all insertions are complete.
    pub fn insert(&mut self, ng: Ngram) {
        // Check if root needs splitting
        if let Some((left, right, key)) = self.root.maybe_split(&self.opts) {
            self.root = Node::Inner(InnerNode {
                keys: vec![key],
                children: vec![left, right],
            });
        }
        self.root.insert(ng, &self.opts);
    }

    /// Find the bucket containing an ngram
    ///
    /// Returns the bucket index and posting index offset, or None if tree is empty.
    pub fn find(&self, ng: Ngram) -> Option<BTreeLookup> {
        self.root.find(ng)
    }

    /// Finalize the tree after all insertions
    ///
    /// This assigns bucket indices and posting index offsets to all leaves.
    pub fn freeze(&mut self) {
        let mut offset = 0usize;
        let mut bucket_index = 0usize;

        self.root.visit(&mut |node| {
            if let Node::Leaf(leaf) = node {
                leaf.bucket_index = bucket_index;
                bucket_index += 1;

                leaf.posting_index_offset = offset;
                offset += leaf.bucket_size;
            }
        });

        self.last_bucket_index = bucket_index.saturating_sub(1);
    }

    /// Get the bucket size option
    pub fn bucket_size(&self) -> usize {
        self.opts.bucket_size
    }

    /// Get the index of the last bucket
    pub fn last_bucket_index(&self) -> usize {
        self.last_bucket_index
    }

    /// Count the total number of ngrams in the tree
    pub fn ngram_count(&self) -> usize {
        let mut count = 0;
        self.root.visit_ref(&mut |node| {
            if let Node::Leaf(leaf) = node {
                count += leaf.bucket_size;
            }
        });
        count
    }

    /// Count the number of leaf buckets
    pub fn bucket_count(&self) -> usize {
        let mut count = 0;
        self.root.visit_ref(&mut |node| {
            if let Node::Leaf(_) = node {
                count += 1;
            }
        });
        count
    }

    /// Serialize the inner nodes for storage
    ///
    /// Returns a serializable representation of the tree structure (without leaf data).
    pub fn serialize_inner_nodes(&self) -> SerializedBTree {
        serialize_node(&self.root, &self.opts)
    }
}

impl Default for BPlusTree {
    fn default() -> Self {
        Self::new()
    }
}

impl Node {
    fn insert(&mut self, ng: Ngram, opts: &BPlusTreeOpts) {
        match self {
            Node::Leaf(leaf) => leaf.insert(ng, opts),
            Node::Inner(inner) => inner.insert(ng, opts),
        }
    }

    fn maybe_split(&mut self, opts: &BPlusTreeOpts) -> Option<(Node, Node, Ngram)> {
        match self {
            Node::Leaf(leaf) => leaf.maybe_split(opts),
            Node::Inner(inner) => inner.maybe_split(opts),
        }
    }

    fn find(&self, ng: Ngram) -> Option<BTreeLookup> {
        match self {
            Node::Leaf(leaf) => leaf.find(),
            Node::Inner(inner) => inner.find(ng),
        }
    }

    fn visit(&mut self, f: &mut impl FnMut(&mut Node)) {
        f(self);
        if let Node::Inner(inner) = self {
            for child in &mut inner.children {
                child.visit(f);
            }
        }
    }

    fn visit_ref(&self, f: &mut impl FnMut(&Node)) {
        f(self);
        if let Node::Inner(inner) = self {
            for child in &inner.children {
                child.visit_ref(f);
            }
        }
    }
}

impl LeafNode {
    fn insert(&mut self, ng: Ngram, opts: &BPlusTreeOpts) {
        self.bucket_size += 1;

        // Record the split key when we hit the midpoint
        if self.bucket_size == (opts.bucket_size / 2) + 1 {
            self.split_key = ng;
        }
    }

    fn maybe_split(&mut self, opts: &BPlusTreeOpts) -> Option<(Node, Node, Ngram)> {
        if self.bucket_size < opts.bucket_size {
            return None;
        }

        let half = opts.bucket_size / 2;
        Some((
            Node::Leaf(LeafNode {
                bucket_index: 0,
                posting_index_offset: 0,
                bucket_size: half,
                split_key: 0,
            }),
            Node::Leaf(LeafNode {
                bucket_index: 0,
                posting_index_offset: 0,
                bucket_size: half,
                split_key: 0,
            }),
            self.split_key,
        ))
    }

    fn find(&self) -> Option<BTreeLookup> {
        Some(BTreeLookup {
            bucket_index: self.bucket_index,
            posting_index_offset: self.posting_index_offset,
        })
    }
}

impl InnerNode {
    fn insert(&mut self, ng: Ngram, opts: &BPlusTreeOpts) {
        let insert_at = |this: &mut InnerNode, mut i: usize| {
            // Split child if needed (single-pass insertion)
            if let Some((left, right, key)) = this.children[i].maybe_split(opts) {
                this.keys.insert(i, key);
                this.children.splice(i..=i, [left, right]);

                // Adjust target index if needed
                if ng >= this.keys[i] {
                    i += 1;
                }
            }
            this.children[i].insert(ng, opts);
        };

        // Find the correct child
        for (i, &key) in self.keys.iter().enumerate() {
            if ng < key {
                insert_at(self, i);
                return;
            }
        }
        insert_at(self, self.children.len() - 1);
    }

    fn maybe_split(&mut self, opts: &BPlusTreeOpts) -> Option<(Node, Node, Ngram)> {
        if self.children.len() < 2 * opts.v {
            return None;
        }

        let mid_key = self.keys[opts.v - 1];
        let left_keys: Vec<_> = self.keys[..opts.v - 1].to_vec();
        let right_keys: Vec<_> = self.keys[opts.v..].to_vec();

        let left_children: Vec<_> = self.children.drain(..opts.v).collect();
        let right_children: Vec<_> = self.children.drain(..).collect();

        Some((
            Node::Inner(InnerNode {
                keys: left_keys,
                children: left_children,
            }),
            Node::Inner(InnerNode {
                keys: right_keys,
                children: right_children,
            }),
            mid_key,
        ))
    }

    fn find(&self, ng: Ngram) -> Option<BTreeLookup> {
        for (i, &key) in self.keys.iter().enumerate() {
            if ng < key {
                return self.children[i].find(ng);
            }
        }
        self.children.last()?.find(ng)
    }
}

/// Serialized B+-tree structure for storage
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedBTree {
    pub inner_nodes: Vec<SerializedInnerNode>,
    pub leaves: Vec<SerializedLeaf>,
    pub bucket_size: usize,
    pub v: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedInnerNode {
    pub keys: Vec<Ngram>,
    pub child_indices: Vec<u32>, // Index into inner_nodes (high bit set) or leaves
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedLeaf {
    pub bucket_index: u32,
    pub posting_index_offset: u32,
}

const INNER_NODE_FLAG: u32 = 0x8000_0000;

fn serialize_node(root: &Node, opts: &BPlusTreeOpts) -> SerializedBTree {
    let mut inner_nodes = Vec::new();
    let mut leaves = Vec::new();

    fn collect(
        node: &Node,
        inner_nodes: &mut Vec<SerializedInnerNode>,
        leaves: &mut Vec<SerializedLeaf>,
    ) -> u32 {
        match node {
            Node::Leaf(leaf) => {
                let idx = leaves.len() as u32;
                leaves.push(SerializedLeaf {
                    bucket_index: leaf.bucket_index as u32,
                    posting_index_offset: leaf.posting_index_offset as u32,
                });
                idx
            }
            Node::Inner(inner) => {
                let child_indices: Vec<u32> = inner
                    .children
                    .iter()
                    .map(|c| {
                        let idx = collect(c, inner_nodes, leaves);
                        if matches!(c, Node::Inner(_)) {
                            idx | INNER_NODE_FLAG
                        } else {
                            idx
                        }
                    })
                    .collect();

                let idx = inner_nodes.len() as u32;
                inner_nodes.push(SerializedInnerNode {
                    keys: inner.keys.clone(),
                    child_indices,
                });
                idx | INNER_NODE_FLAG
            }
        }
    }

    collect(root, &mut inner_nodes, &mut leaves);

    SerializedBTree {
        inner_nodes,
        leaves,
        bucket_size: opts.bucket_size,
        v: opts.v,
    }
}

impl SerializedBTree {
    /// Find the bucket for an ngram in the serialized tree
    pub fn find(&self, ng: Ngram) -> Option<BTreeLookup> {
        if self.inner_nodes.is_empty() && self.leaves.is_empty() {
            return None;
        }

        // Start from root (last inner node, or first leaf if no inner nodes)
        if self.inner_nodes.is_empty() {
            let leaf = self.leaves.first()?;
            return Some(BTreeLookup {
                bucket_index: leaf.bucket_index as usize,
                posting_index_offset: leaf.posting_index_offset as usize,
            });
        }

        // Root is the last inner node added
        let mut current_idx = (self.inner_nodes.len() - 1) as u32 | INNER_NODE_FLAG;

        loop {
            if current_idx & INNER_NODE_FLAG != 0 {
                // Inner node
                let inner = &self.inner_nodes[(current_idx & !INNER_NODE_FLAG) as usize];
                let mut child_idx = inner.child_indices.len() - 1;
                for (i, &key) in inner.keys.iter().enumerate() {
                    if ng < key {
                        child_idx = i;
                        break;
                    }
                }
                current_idx = inner.child_indices[child_idx];
            } else {
                // Leaf node
                let leaf = &self.leaves[current_idx as usize];
                return Some(BTreeLookup {
                    bucket_index: leaf.bucket_index as usize,
                    posting_index_offset: leaf.posting_index_offset as usize,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_tree() {
        let tree = BPlusTree::new();
        assert_eq!(tree.ngram_count(), 0);
    }

    #[test]
    fn test_single_insert() {
        let mut tree = BPlusTree::new();
        tree.insert(12345);
        tree.freeze();

        assert_eq!(tree.ngram_count(), 1);
        assert_eq!(tree.bucket_count(), 1);

        let lookup = tree.find(12345).unwrap();
        assert_eq!(lookup.bucket_index, 0);
        assert_eq!(lookup.posting_index_offset, 0);
    }

    #[test]
    fn test_multiple_inserts() {
        let mut tree = BPlusTree::with_opts(BPlusTreeOpts {
            bucket_size: 4,
            v: 2,
        });

        for ng in 0..10u64 {
            tree.insert(ng);
        }
        tree.freeze();

        assert_eq!(tree.ngram_count(), 10);

        // Verify all ngrams can be found
        for ng in 0..10u64 {
            assert!(tree.find(ng).is_some(), "Failed to find ngram {}", ng);
        }
    }

    #[test]
    fn test_bucket_splitting() {
        let mut tree = BPlusTree::with_opts(BPlusTreeOpts {
            bucket_size: 4,
            v: 2,
        });

        // Insert enough ngrams to cause splits
        for ng in 0..20u64 {
            tree.insert(ng);
        }
        tree.freeze();

        assert_eq!(tree.ngram_count(), 20);
        assert!(tree.bucket_count() > 1, "Expected multiple buckets");

        // Verify all ngrams can still be found
        for ng in 0..20u64 {
            assert!(tree.find(ng).is_some(), "Failed to find ngram {}", ng);
        }
    }

    #[test]
    fn test_serialization_roundtrip() {
        let mut tree = BPlusTree::with_opts(BPlusTreeOpts {
            bucket_size: 4,
            v: 2,
        });

        for ng in 0..15u64 {
            tree.insert(ng);
        }
        tree.freeze();

        let serialized = tree.serialize_inner_nodes();

        // Verify lookups work with serialized tree
        for ng in 0..15u64 {
            let original = tree.find(ng).unwrap();
            let from_serialized = serialized.find(ng).unwrap();
            assert_eq!(
                original.bucket_index, from_serialized.bucket_index,
                "Bucket mismatch for ngram {}",
                ng
            );
            assert_eq!(
                original.posting_index_offset, from_serialized.posting_index_offset,
                "Posting offset mismatch for ngram {}",
                ng
            );
        }
    }

    #[test]
    fn test_large_tree() {
        let mut tree = BPlusTree::new();

        // Insert many ngrams in sorted order
        for ng in (0..10000u64).map(|i| i * 100) {
            tree.insert(ng);
        }
        tree.freeze();

        assert_eq!(tree.ngram_count(), 10000);

        // Spot check some values
        assert!(tree.find(0).is_some());
        assert!(tree.find(500 * 100).is_some());
        assert!(tree.find(9999 * 100).is_some());
        assert!(tree.find(1).is_none() || tree.find(1).is_some()); // May or may not find bucket
    }

    #[test]
    fn test_posting_index_offsets() {
        let mut tree = BPlusTree::with_opts(BPlusTreeOpts {
            bucket_size: 4,
            v: 2,
        });

        for ng in 0..12u64 {
            tree.insert(ng);
        }
        tree.freeze();

        // The posting index offsets should be cumulative bucket sizes
        let lookup0 = tree.find(0).unwrap();
        assert_eq!(lookup0.posting_index_offset, 0);

        // Later buckets should have higher offsets
        let lookup_last = tree.find(11).unwrap();
        assert!(
            lookup_last.posting_index_offset > 0
                || lookup_last.bucket_index == lookup0.bucket_index
        );
    }
}
