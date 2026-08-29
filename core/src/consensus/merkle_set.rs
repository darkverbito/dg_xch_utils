//! The wallet-facing `MerkleSet` — the tree construction and INCLUSION/EXCLUSION proof
//! generation behind `request_additions` / `request_removals`, whose proofs a light wallet
//! verifies against the block header's foliage `additions_root` / `removals_root`.
//!
//! The proof byte encoding is consensus-frozen; byte-parity is pinned by
//! `core/tests/merkle_set_proofs.rs` and the block-5,000,000 fixtures in `full-node/tests`.
//!
//! The producer/validator-side ROOT computation (`block_generator.rs::merkle_set_root`) is the
//! collapsed single-pass recursion over the same node hashing; this module additionally retains
//! the tree (`nodes_vec`) so proofs can be generated. `canonical_removals_root(x)` and
//! `MerkleSet::from_leafs(x).get_root()` agree bit-for-bit (cross-checked in tests).
//!
//! Proof wire format (node-type prefixed pre-order): `EMPTY=0`, `TERMINAL=1` + 32-byte leaf,
//! `MIDDLE=2` + left + right, `TRUNCATED=3` + 32-byte subtree hash. Proofs include every tree
//! layer down to the leaf pair (no collapsing — `pad_middles_for_proof_gen` re-expands the
//! collapsed levels), while node HASHES are computed as-if collapsed (the `MidDbl` propagation).

use crate::utils::hash_256;

/// The 30-byte zero prefix + two node-type bytes that domain-separate a middle-node hash:
/// `Sha256(0u8*30 || encode_type(l) || encode_type(r) || l || r)`.
const HASH_PREFIX: [u8; 30] = [0u8; 30];

/// The empty-set root / empty-subtree hash (all zeros).
pub const BLANK: [u8; 32] = [0u8; 32];

// sha256(bytes([0] * 32)) — the nodes_vec placeholder hash for inserted Empty nodes.
// Never enters a proof or a root.
const EMPTY_NODE_HASH: [u8; 32] = [
    0x66, 0x68, 0x7a, 0xad, 0xf8, 0x62, 0xbd, 0x77, 0x6c, 0x8f, 0xc1, 0x8b, 0x8e, 0x9f, 0x8e, 0x20,
    0x08, 0x97, 0x14, 0x85, 0x6e, 0xe2, 0x33, 0xb3, 0x90, 0x2a, 0x59, 0x1d, 0x0d, 0x5f, 0x29, 0x25,
];

// Proof byte codes.
const EMPTY: u8 = 0;
const TERMINAL: u8 = 1;
const MIDDLE: u8 = 2;
const TRUNCATED: u8 = 3;

/// Node classification for hash computation. `MidDbl` is a middle node whose subtree
/// collapses to a terminal pair (both children terminals, or a one-sided chain ending in
/// such a pair); it decides where Empty nodes must be inserted.
#[derive(PartialEq, Eq, Copy, Clone, Debug)]
enum NodeType {
    Empty,
    Term,
    Mid,
    MidDbl,
}

fn encode_type(t: NodeType) -> u8 {
    match t {
        NodeType::Empty => 0,
        NodeType::Term => 1,
        NodeType::Mid | NodeType::MidDbl => 2,
    }
}

// the domain-separated middle-node hash
fn hash(ltype: NodeType, rtype: NodeType, left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut buf = Vec::with_capacity(30 + 2 + 32 + 32);
    buf.extend_from_slice(&HASH_PREFIX);
    buf.push(encode_type(ltype));
    buf.push(encode_type(rtype));
    buf.extend_from_slice(left);
    buf.extend_from_slice(right);
    hash_256(buf)
}

// the single-leaf root is Sha256(TERMINAL || leaf)
fn hash_leaf(leaf: &[u8; 32]) -> [u8; 32] {
    let mut buf = Vec::with_capacity(33);
    buf.push(NodeType::Term as u8);
    buf.extend_from_slice(leaf);
    hash_256(buf)
}

fn get_bit(val: &[u8; 32], bit: u8) -> bool {
    (val[(bit / 8) as usize] & (0x80 >> (bit & 7))) != 0
}

/// The stored node shape (indexes into `nodes_vec`).
#[derive(PartialEq, Debug, Copy, Clone)]
enum ArrayTypes {
    Leaf,
    Middle(u32, u32),
    Empty,
    Truncated,
}

impl From<ArrayTypes> for NodeType {
    fn from(val: ArrayTypes) -> NodeType {
        match val {
            ArrayTypes::Empty => NodeType::Empty,
            ArrayTypes::Leaf => NodeType::Term,
            ArrayTypes::Middle(_, _) | ArrayTypes::Truncated => NodeType::Mid,
        }
    }
}

/// A malformed proof (bad node code, truncated bytes, mis-positioned leaf, trailing bytes, or
/// proof-of-a-truncated-subtree).
#[derive(Debug, PartialEq, Eq)]
pub struct SetError;

impl std::fmt::Display for SetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid merkle set / proof")
    }
}

impl std::error::Error for SetError {}

/// A retained merkle set over 32-byte leaves: all nodes in a vec, root last.
#[derive(PartialEq, Debug, Clone, Default)]
pub struct MerkleSet {
    nodes_vec: Vec<(ArrayTypes, [u8; 32])>,
    // True when rebuilt from a proof: the tree may contain truncated subtrees, so newly generated
    // proofs don't round-trip — generate_proof then reports inclusion with an EMPTY proof.
    from_proof: bool,
}

impl MerkleSet {
    /// Build the set from (already-hashed) 32-byte leaves; order and duplicates do not
    /// affect the tree.
    #[must_use]
    pub fn from_leafs(leafs: &mut [[u8; 32]]) -> MerkleSet {
        let mut merkle_tree = MerkleSet {
            from_proof: false,
            ..Default::default()
        };
        if leafs.is_empty() {
            merkle_tree.nodes_vec.push((ArrayTypes::Empty, BLANK));
            return merkle_tree;
        }
        merkle_tree.generate_merkle_tree_recurse(leafs, 0);
        merkle_tree
    }

    /// Rebuild a (possibly truncated) tree from proof bytes.
    ///
    /// # Errors
    /// Returns [`SetError`] on any malformed proof (never panics on attacker bytes).
    pub fn from_proof(proof: &[u8]) -> Result<MerkleSet, SetError> {
        let mut merkle_tree = MerkleSet {
            from_proof: true,
            ..Default::default()
        };
        merkle_tree.deserialize_proof_impl(proof)?;
        Ok(merkle_tree)
    }

    // Iterative pre-order parse with a leaf position audit (each TERMINAL's bits must match
    // the branch route that reaches it).
    fn deserialize_proof_impl(&mut self, proof: &[u8]) -> Result<(), SetError> {
        enum ParseOp {
            Node,
            Middle,
        }

        fn read_exact<'a>(
            proof: &'a [u8],
            pos: &mut usize,
            n: usize,
        ) -> Result<&'a [u8], SetError> {
            let end = pos.checked_add(n).ok_or(SetError)?;
            let out = proof.get(*pos..end).ok_or(SetError)?;
            *pos = end;
            Ok(out)
        }
        let mut pos = 0usize;

        let mut values = Vec::<(u32, NodeType)>::new();
        let mut ops = vec![ParseOp::Node];
        let mut depth = 0u32;
        let mut bits_stack: Vec<Vec<bool>> = vec![Vec::new()];

        while let Some(op) = ops.pop() {
            let Some(bits) = bits_stack.pop() else {
                return Err(SetError);
            };
            match op {
                ParseOp::Node => {
                    let b = read_exact(proof, &mut pos, 1)?[0];
                    match b {
                        EMPTY => {
                            values.push((self.nodes_vec.len() as u32, NodeType::Empty));
                            self.nodes_vec.push((ArrayTypes::Empty, BLANK));
                        }
                        TERMINAL => {
                            let mut leaf = [0u8; 32];
                            leaf.copy_from_slice(read_exact(proof, &mut pos, 32)?);
                            // audit the leaf is correctly positioned: its bits must retrace the route
                            for (p, v) in bits.iter().enumerate() {
                                if get_bit(&leaf, p as u8) != *v {
                                    return Err(SetError);
                                }
                            }
                            values.push((self.nodes_vec.len() as u32, NodeType::Term));
                            self.nodes_vec.push((ArrayTypes::Leaf, leaf));
                        }
                        TRUNCATED => {
                            let mut th = [0u8; 32];
                            th.copy_from_slice(read_exact(proof, &mut pos, 32)?);
                            values.push((self.nodes_vec.len() as u32, NodeType::Mid));
                            self.nodes_vec.push((ArrayTypes::Truncated, th));
                        }
                        MIDDLE => {
                            if depth > 256 {
                                return Err(SetError);
                            }
                            ops.push(ParseOp::Middle);
                            ops.push(ParseOp::Node);
                            ops.push(ParseOp::Node);

                            bits_stack.push(Vec::new()); // mid is not audited: placeholder
                            let mut new_bits = bits.clone();
                            new_bits.push(true); // processed second => the right branch
                            bits_stack.push(new_bits);
                            let mut new_bits = bits.clone();
                            new_bits.push(false); // processed first => the left branch
                            bits_stack.push(new_bits);

                            depth += 1;
                        }
                        _ => return Err(SetError),
                    }
                }
                ParseOp::Middle => {
                    let right = values.pop().ok_or(SetError)?;
                    let left = values.pop().ok_or(SetError)?;

                    // Proofs carry every tree layer (no collapsing), but node hashes are computed
                    // as-if collapsed: propagate MidDbl up so the collapse points are known.
                    let new_node_type = match (left.1, right.1) {
                        (NodeType::Term, NodeType::Term)
                        | (NodeType::Empty, NodeType::MidDbl)
                        | (NodeType::MidDbl, NodeType::Empty) => NodeType::MidDbl,
                        (_, _) => NodeType::Mid,
                    };

                    let node_hash = match (left.1, right.1) {
                        // A collapsed layer: copy the double-terminal child's hash upward.
                        (NodeType::Empty, NodeType::MidDbl) => {
                            values.push(right);
                            self.nodes_vec[right.0 as usize].1
                        }
                        (NodeType::MidDbl, NodeType::Empty) => {
                            values.push(left);
                            self.nodes_vec[left.0 as usize].1
                        }
                        // Not collapsed: hash the pair.
                        (_, _) => {
                            values.push((self.nodes_vec.len() as u32, new_node_type));
                            hash(
                                self.nodes_vec[left.0 as usize].0.into(),
                                self.nodes_vec[right.0 as usize].0.into(),
                                &self.nodes_vec[left.0 as usize].1,
                                &self.nodes_vec[right.0 as usize].1,
                            )
                        }
                    };
                    self.nodes_vec
                        .push((ArrayTypes::Middle(left.0, right.0), node_hash));
                    depth -= 1;
                }
            }
        }
        if pos == proof.len() {
            Ok(())
        } else {
            Err(SetError)
        }
    }

    /// The set's root hash: empty → all-zeros, single leaf → `Sha256(1 || leaf)`.
    #[must_use]
    pub fn get_root(&self) -> [u8; 32] {
        let Some(last) = self.nodes_vec.last() else {
            return BLANK;
        };
        match last.0 {
            ArrayTypes::Leaf => hash_leaf(&last.1),
            ArrayTypes::Middle(_, _) | ArrayTypes::Truncated => last.1,
            ArrayTypes::Empty => BLANK,
        }
    }

    /// Produce the proof that `leaf` is / is not in the set. `true` = proof-of-INCLUSION,
    /// `false` = proof-of-EXCLUSION; the proof bytes verify against [`MerkleSet::get_root`]
    /// via [`validate_merkle_proof`]. On a tree rebuilt `from_proof` the proof bytes come
    /// back empty (truncated subtrees don't round-trip).
    ///
    /// # Errors
    /// Returns [`SetError`] if the lookup walks into a truncated subtree.
    pub fn generate_proof(&self, leaf: &[u8; 32]) -> Result<(bool, Vec<u8>), SetError> {
        let mut proof = Vec::new();
        let included = self.generate_proof_impl(
            self.nodes_vec.len().checked_sub(1).ok_or(SetError)?,
            leaf,
            &mut proof,
            0,
        )?;
        if self.from_proof {
            Ok((included, vec![]))
        } else {
            Ok((included, proof))
        }
    }

    fn generate_proof_impl(
        &self,
        current_node_index: usize,
        leaf: &[u8; 32],
        proof: &mut Vec<u8>,
        depth: u8,
    ) -> Result<bool, SetError> {
        match self.nodes_vec[current_node_index].0 {
            ArrayTypes::Empty => {
                proof.push(EMPTY);
                Ok(false)
            }
            ArrayTypes::Leaf => {
                proof.push(TERMINAL);
                proof.extend_from_slice(&self.nodes_vec[current_node_index].1);
                Ok(&self.nodes_vec[current_node_index].1 == leaf)
            }
            ArrayTypes::Middle(left, right) => {
                if matches!(
                    (
                        self.nodes_vec[left as usize].0,
                        self.nodes_vec[right as usize].0
                    ),
                    (ArrayTypes::Leaf, ArrayTypes::Leaf)
                ) {
                    pad_middles_for_proof_gen(
                        proof,
                        &self.nodes_vec[left as usize].1,
                        &self.nodes_vec[right as usize].1,
                        depth,
                    );
                    return Ok(&self.nodes_vec[left as usize].1 == leaf
                        || &self.nodes_vec[right as usize].1 == leaf);
                }

                proof.push(MIDDLE);
                if get_bit(leaf, depth) {
                    // bit 1: truncate the left branch, search the right
                    self.other_included(left as usize, proof);
                    self.generate_proof_impl(right as usize, leaf, proof, depth + 1)
                } else {
                    // bit 0: search the left, truncate the right
                    let r = self.generate_proof_impl(left as usize, leaf, proof, depth + 1)?;
                    self.other_included(right as usize, proof);
                    Ok(r)
                }
            }
            ArrayTypes::Truncated => Err(SetError),
        }
    }

    // The not-traversed sibling subtree, as needed to recompute the root: Empty stays a code,
    // a leaf is TERMINAL (double-terminal collapse needs the real leaf), else TRUNCATED + hash.
    fn other_included(&self, current_node_index: usize, proof: &mut Vec<u8>) {
        match self.nodes_vec[current_node_index].0 {
            ArrayTypes::Empty => proof.push(EMPTY),
            ArrayTypes::Middle(_, _) | ArrayTypes::Truncated => {
                proof.push(TRUNCATED);
                proof.extend_from_slice(&self.nodes_vec[current_node_index].1);
            }
            ArrayTypes::Leaf => {
                proof.push(TERMINAL);
                proof.extend_from_slice(&self.nodes_vec[current_node_index].1);
            }
        }
    }

    // The radix sort that also retains every node (the proof-capable sibling of
    // block_generator.rs::merkle_set_recurse).
    fn generate_merkle_tree_recurse(
        &mut self,
        range: &mut [[u8; 32]],
        depth: u8,
    ) -> ([u8; 32], NodeType) {
        assert!(!range.is_empty(), "empty range in merkle tree recursion");

        if range.len() == 1 {
            self.nodes_vec.push((ArrayTypes::Leaf, range[0]));
            return (range[0], NodeType::Term);
        }

        // Partition on the bit at `depth`: 0-bits left, 1-bits right.
        let mut left: i32 = 0;
        let mut right = range.len() as i32 - 1;
        while left <= right {
            let left_bit = get_bit(&range[left as usize], depth);
            let right_bit = get_bit(&range[right as usize], depth);
            if left_bit && !right_bit {
                range.swap(left as usize, right as usize);
                left += 1;
                right -= 1;
            } else {
                if !left_bit {
                    left += 1;
                }
                if right_bit {
                    right -= 1;
                }
            }
        }

        let left_empty = left == 0;
        let right_empty = right == range.len() as i32 - 1;

        if left_empty || right_empty {
            if depth == 255 {
                // All 256 bits identical: a duplicate value, collapsed to one leaf (it's a set).
                debug_assert!(range.len() > 1);
                debug_assert!(range[0] == range[1]);
                self.nodes_vec.push((ArrayTypes::Leaf, range[0]));
                (range[0], NodeType::Term)
            } else {
                // One-sided at this level: forward the child, inserting an Empty node only when
                // the child is a (non-collapsing) Mid.
                let (child_hash, child_type) = self.generate_merkle_tree_recurse(range, depth + 1);
                if child_type == NodeType::Mid {
                    self.nodes_vec.push((ArrayTypes::Empty, EMPTY_NODE_HASH));
                    let node_length = self.nodes_vec.len() as u32;
                    if left_empty {
                        let node_hash = hash(NodeType::Empty, child_type, &BLANK, &child_hash);
                        self.nodes_vec.push((
                            ArrayTypes::Middle(node_length - 1, node_length - 2),
                            node_hash,
                        ));
                        (node_hash, NodeType::Mid)
                    } else {
                        let node_hash = hash(child_type, NodeType::Empty, &child_hash, &BLANK);
                        self.nodes_vec.push((
                            ArrayTypes::Middle(node_length - 2, node_length - 1),
                            node_hash,
                        ));
                        (node_hash, NodeType::Mid)
                    }
                } else {
                    (child_hash, child_type)
                }
            }
        } else if depth == 255 {
            // Bottom-of-tree split of the last distinct pair (u8 depth would overflow).
            debug_assert!(range.len() > 1);
            debug_assert!(left < range.len() as i32);
            self.nodes_vec.push((ArrayTypes::Leaf, range[0]));
            self.nodes_vec
                .push((ArrayTypes::Leaf, range[left as usize]));
            let nodes_len = self.nodes_vec.len() as u32;
            let node_hash = hash(
                NodeType::Term,
                NodeType::Term,
                &range[0],
                &range[left as usize],
            );
            self.nodes_vec
                .push((ArrayTypes::Middle(nodes_len - 2, nodes_len - 1), node_hash));
            (node_hash, NodeType::MidDbl)
        } else {
            // A middle node proper: recurse both sides.
            let (left_hash, left_type) =
                self.generate_merkle_tree_recurse(&mut range[..left as usize], depth + 1);
            let left_child_index = self.nodes_vec.len() as u32 - 1;
            let (right_hash, right_type) =
                self.generate_merkle_tree_recurse(&mut range[left as usize..], depth + 1);

            let node_hash = hash(left_type, right_type, &left_hash, &right_hash);
            let node_type = if left_type == NodeType::Term && right_type == NodeType::Term {
                NodeType::MidDbl
            } else {
                NodeType::Mid
            };
            self.nodes_vec.push((
                ArrayTypes::Middle(left_child_index, self.nodes_vec.len() as u32 - 1),
                node_hash,
            ));
            (node_hash, node_type)
        }
    }
}

// Re-introduce the collapsed one-sided levels between `depth` and the first bit where the
// two leaves of a double-terminal subtree diverge, so the proof's path structure exactly
// matches the leaves' bits.
fn pad_middles_for_proof_gen(proof: &mut Vec<u8>, left: &[u8; 32], right: &[u8; 32], depth: u8) {
    let left_bit = get_bit(left, depth);
    let right_bit = get_bit(right, depth);
    proof.push(MIDDLE);
    if left_bit != right_bit {
        proof.push(TERMINAL);
        proof.extend_from_slice(left);
        proof.push(TERMINAL);
        proof.extend_from_slice(right);
    } else if left_bit {
        proof.push(EMPTY);
        pad_middles_for_proof_gen(proof, left, right, depth + 1);
    } else {
        pad_middles_for_proof_gen(proof, left, right, depth + 1);
        proof.push(EMPTY);
    }
}

/// Verify `proof` against `root`: `Ok(true)` proves `item` IS in the set, `Ok(false)` proves
/// it is NOT — what a wallet runs against the foliage root.
///
/// # Errors
/// Returns [`SetError`] if the proof is malformed or does not hash to `root`.
pub fn validate_merkle_proof(
    proof: &[u8],
    item: &[u8; 32],
    root: &[u8; 32],
) -> Result<bool, SetError> {
    let tree = MerkleSet::from_proof(proof)?;
    if tree.get_root() != *root {
        return Err(SetError);
    }
    Ok(tree.generate_proof(item)?.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hx(s: &str) -> [u8; 32] {
        let v = hex::decode(s).expect("hex");
        let mut out = [0u8; 32];
        out.copy_from_slice(&v);
        out
    }

    fn h2(buf1: &[u8], buf2: &[u8]) -> [u8; 32] {
        let mut buf = buf1.to_vec();
        buf.extend_from_slice(buf2);
        hash_256(buf)
    }

    fn hashdown(types: &[u8; 2], buf2: &[u8], buf3: &[u8]) -> [u8; 32] {
        let mut buf = vec![0u8; 30];
        buf.extend_from_slice(types);
        buf.extend_from_slice(buf2);
        buf.extend_from_slice(buf3);
        hash_256(buf)
    }

    // Root corpus: duplicates, rotations, singles/pairs/triples/quads, and the special-shape
    // trees, asserted against MerkleSet::from_leafs().get_root().
    #[allow(clippy::many_single_char_names)]
    fn merkle_set_test_cases() -> Vec<([u8; 32], Vec<[u8; 32]>)> {
        let a = hx("7000000000000000000000000000000000000000000000000000000000000000");
        let b = hx("7100000000000000000000000000000000000000000000000000000000000000");
        let c = hx("8000000000000000000000000000000000000000000000000000000000000000");
        let d = hx("8100000000000000000000000000000000000000000000000000000000000000");

        let root4 = hashdown(
            &[2, 2],
            &hashdown(&[1, 1], &a, &b),
            &hashdown(&[1, 1], &c, &d),
        );
        let root3 = hashdown(&[2, 1], &hashdown(&[1, 1], &a, &b), &c);

        // merkle_tree_5
        let e5 = hx("5800000000000000000000000000000000000000000000000000000000000000");
        let b5 = hx("2300000000000000000000000000000000000000000000000000000000000000");
        let c5 = hx("2100000000000000000000000000000000000000000000000000000000000000");
        let d5 = hx("ca00000000000000000000000000000000000000000000000000000000000000");
        let a5 = hx("2000000000000000000000000000000000000000000000000000000000000000");
        let mut expected5 = hashdown(&[1, 1], &a5, &c5);
        expected5 = hashdown(&[2, 1], &expected5, &b5);
        expected5 = hashdown(&[2, 0], &expected5, &BLANK);
        expected5 = hashdown(&[2, 0], &expected5, &BLANK);
        expected5 = hashdown(&[2, 0], &expected5, &BLANK);
        expected5 = hashdown(&[0, 2], &BLANK, &expected5);
        expected5 = hashdown(&[2, 1], &expected5, &e5);
        expected5 = hashdown(&[2, 1], &expected5, &d5);
        let tree5 = (expected5, vec![e5, b5, c5, d5, a5]);

        // merkle_tree_left_edge
        let la = hx("8000000000000000000000000000000000000000000000000000000000000000");
        let lb = hx("0000000000000000000000000000000000000000000000000000000000000001");
        let lc = hx("0000000000000000000000000000000000000000000000000000000000000002");
        let ld = hx("0000000000000000000000000000000000000000000000000000000000000003");
        let mut le = hashdown(&[1, 1], &lc, &ld);
        le = hashdown(&[1, 2], &lb, &le);
        for _ in 0..253 {
            le = hashdown(&[2, 0], &le, &BLANK);
        }
        le = hashdown(&[2, 1], &le, &la);
        let left_edge = (le, vec![la, lb, lc, ld]);
        let left_edge_dups = (le, vec![la, lb, lc, ld, la, lb, lc, ld]);

        // merkle_tree_right_edge
        let ra = hx("4000000000000000000000000000000000000000000000000000000000000000");
        let rb = hx("ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff");
        let rc = hx("fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe");
        let rd = hx("fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffd");
        let mut re = hashdown(&[1, 1], &rc, &rb);
        re = hashdown(&[1, 2], &rd, &re);
        for _ in 0..253 {
            re = hashdown(&[0, 2], &BLANK, &re);
        }
        re = hashdown(&[1, 2], &ra, &re);
        let right_edge = (re, vec![ra, rb, rc, rd]);

        vec![
            (BLANK, vec![]),
            (h2(&[1u8], &a), vec![a, a]),
            (h2(&[1u8], &a), vec![a, a, a, a]),
            (root4, vec![a, b, c, d, a]),
            (root4, vec![b, c, d, a, a]),
            (root4, vec![c, d, a, b, a]),
            (root4, vec![d, a, b, c, a]),
            (root4, vec![d, c, b, a, a]),
            (root4, vec![c, b, a, d, a]),
            (root4, vec![b, a, d, c, a]),
            (root4, vec![a, d, c, b, a]),
            (root4, vec![c, a, d, b, a]),
            (h2(&[1u8], &a), vec![a]),
            (h2(&[1u8], &b), vec![b]),
            (h2(&[1u8], &c), vec![c]),
            (h2(&[1u8], &d), vec![d]),
            (hashdown(&[1, 1], &a, &b), vec![a, b]),
            (hashdown(&[1, 1], &a, &b), vec![b, a]),
            (hashdown(&[1, 1], &a, &c), vec![a, c]),
            (hashdown(&[1, 1], &a, &c), vec![c, a]),
            (hashdown(&[1, 1], &a, &d), vec![a, d]),
            (hashdown(&[1, 1], &a, &d), vec![d, a]),
            (hashdown(&[1, 1], &b, &c), vec![b, c]),
            (hashdown(&[1, 1], &b, &c), vec![c, b]),
            (hashdown(&[1, 1], &b, &d), vec![b, d]),
            (hashdown(&[1, 1], &b, &d), vec![d, b]),
            (hashdown(&[1, 1], &c, &d), vec![c, d]),
            (hashdown(&[1, 1], &c, &d), vec![d, c]),
            (root3, vec![a, b, c]),
            (root3, vec![a, c, b]),
            (root3, vec![b, a, c]),
            (root3, vec![b, c, a]),
            (root3, vec![c, a, b]),
            (root3, vec![c, b, a]),
            (root4, vec![a, b, c, d]),
            (root4, vec![b, c, d, a]),
            (root4, vec![c, d, a, b]),
            (root4, vec![d, a, b, c]),
            (root4, vec![d, c, b, a]),
            (root4, vec![c, b, a, d]),
            (root4, vec![b, a, d, c]),
            (root4, vec![a, d, c, b]),
            (root4, vec![c, a, d, b]),
            tree5,
            left_edge,
            left_edge_dups,
            right_edge,
        ]
    }

    // Roots + full proof round-trips (inclusion for every leaf, exclusion for probes) over
    // the corpus.
    #[test]
    fn ported_chia_rs_corpus_roots_and_proof_round_trips() {
        for (root, leafs) in merkle_set_test_cases() {
            let tree = MerkleSet::from_leafs(&mut leafs.clone());
            assert_eq!(tree.get_root(), root);

            for item in &leafs {
                let (included, proof) = tree.generate_proof(item).expect("proof");
                assert!(included);
                let rebuilt = MerkleSet::from_proof(&proof).expect("parse proof");
                assert_eq!(rebuilt.get_root(), root);
                let (included, new_proof) = rebuilt.generate_proof(item).expect("re-proof");
                assert!(included);
                assert_eq!(new_proof, Vec::<u8>::new());
                assert!(validate_merkle_proof(&proof, item, &root).expect("validate"));
            }

            // deterministic exclusion probes (xorshift64) — never part of the corpus leaves
            let mut s: u64 = 0xDEAD_BEEF_CAFE_F00D;
            for _ in 0..20 {
                let mut item = [0u8; 32];
                for chunk in item.chunks_mut(8) {
                    s ^= s << 13;
                    s ^= s >> 7;
                    s ^= s << 17;
                    chunk.copy_from_slice(&s.to_be_bytes());
                }
                if leafs.contains(&item) {
                    continue;
                }
                let (included, proof) = tree.generate_proof(&item).expect("proof");
                assert!(!included);
                let rebuilt = MerkleSet::from_proof(&proof).expect("parse proof");
                assert_eq!(rebuilt.get_root(), root);
                assert!(!validate_merkle_proof(&proof, &item, &root).expect("validate"));
            }
        }
    }

    // Pinned proof hex: every level down to the leaf pair is present (no collapsing), for
    // inclusion AND exclusion.
    #[test]
    fn pinned_chia_rs_complete_proof_vector() {
        let a = hx("c000000000000000000000000000000000000000000000000000000000000000");
        let b = hx("c800000000000000000000000000000000000000000000000000000000000000");
        let c = hx("7000000000000000000000000000000000000000000000000000000000000000");
        let expected = "0200020002020201c00000000000000000000000000000000000000000000000000000000000000001c8000000000000000000000000000000000000000000000000000000000000000000";

        let tree = MerkleSet::from_leafs(&mut [a, b]);
        let (included, proof) = tree.generate_proof(&b).expect("proof");
        assert!(included);
        assert_eq!(hex::encode(&proof), expected);

        let (included, proof) = tree.generate_proof(&a).expect("proof");
        assert!(included);
        assert_eq!(hex::encode(&proof), expected);

        // proofs of exclusion are also complete
        let (included, proof) = tree.generate_proof(&c).expect("proof");
        assert!(!included);
        assert_eq!(hex::encode(&proof), expected);
    }

    // A deep MIDDLE-only chain must error (depth bound), not exhaust memory or panic.
    #[test]
    fn malicious_middle_chain_is_rejected() {
        let malicious_proof = vec![MIDDLE; 40000];
        assert!(MerkleSet::from_proof(&malicious_proof).is_err());
    }

    // A TERMINAL leaf on the wrong side of its bit route fails the position audit.
    #[test]
    fn mispositioned_leaf_fails_the_audit() {
        let mut bad_proof: Vec<u8> = Vec::new();
        bad_proof.push(MIDDLE);
        bad_proof.push(TRUNCATED);
        bad_proof.extend_from_slice(&[0x11u8; 32]);
        bad_proof.push(MIDDLE);
        bad_proof.push(TERMINAL);
        // high bit set => belongs on the right, presented on the left
        bad_proof.extend_from_slice(&hx(
            "8000000000000000000000000000000000000000000000000000000000000000",
        ));
        bad_proof.push(TERMINAL);
        bad_proof.extend_from_slice(&[0x00u8; 32]);
        assert_eq!(MerkleSet::from_proof(&bad_proof), Err(SetError));
    }

    // Truncated / trailing byte streams error (never panic), plus arbitrary garbage prefixes.
    #[test]
    fn truncated_and_garbage_proofs_error_not_panic() {
        let a = hx("c000000000000000000000000000000000000000000000000000000000000000");
        let b = hx("c800000000000000000000000000000000000000000000000000000000000000");
        let tree = MerkleSet::from_leafs(&mut [a, b]);
        let (_, proof) = tree.generate_proof(&a).expect("proof");
        // every proper prefix must fail (incomplete parse)
        for cut in 0..proof.len() {
            assert!(MerkleSet::from_proof(&proof[..cut]).is_err(), "cut {cut}");
        }
        // trailing garbage must fail (bytes left over)
        let mut extended = proof.clone();
        extended.push(0);
        assert!(MerkleSet::from_proof(&extended).is_err());
        // deterministic garbage streams
        let mut s: u64 = 0x1234_5678_9ABC_DEF0;
        for len in [1usize, 2, 7, 33, 64, 129] {
            let mut bytes = Vec::with_capacity(len);
            while bytes.len() < len {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                bytes.extend_from_slice(&s.to_be_bytes());
            }
            bytes.truncate(len);
            // must return (parse either way is fine only for a structurally valid stream; the
            // point is: no panic). Err is the overwhelmingly likely outcome.
            let _ = MerkleSet::from_proof(&bytes);
        }
    }

    // The proof-capable tree agrees bit-for-bit with the producer/validator-side collapsed root
    // (block_generator.rs::merkle_set_root via canonical_removals_root) — one encoding, two
    // implementations, zero drift.
    #[cfg(feature = "bls")]
    #[test]
    fn root_matches_the_producer_side_merkle_set_root() {
        use crate::blockchain::sized_bytes::Bytes32;
        use crate::consensus::block_generator::canonical_removals_root;
        use crate::traits::SizedBytes;
        for (_, leafs) in merkle_set_test_cases() {
            let via_producer = canonical_removals_root(
                &leafs.iter().map(|l| Bytes32::new(*l)).collect::<Vec<_>>(),
            );
            let via_tree = MerkleSet::from_leafs(&mut leafs.clone()).get_root();
            assert_eq!(via_producer.bytes(), via_tree);
        }
    }
}
