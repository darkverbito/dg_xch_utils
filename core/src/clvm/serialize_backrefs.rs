//! CLVM back-reference SERIALIZATION — the compressed producer-side encoder, the inverse of the
//! `sexp_from_bytes_backrefs` decoder in [`crate::clvm::parser`].
//!
//! A faithful port of clvmr's `serde::ser_br::node_to_stream_backrefs` +
//! `serde::read_cache_lookup::ReadCacheLookup` (Chia-Network/clvm_rs, the crate chia_rs 0.42.1
//! depends on). The serializer walks the tree exactly as the plain serializer does, but before
//! emitting any node it asks a running `ReadCacheLookup` whether an already-serialized node with the
//! SAME sha256 tree-hash is reachable by a shorter CLVM path than re-serializing this subtree costs.
//! If so it emits a back-reference (`0xfe` + the path atom) instead. The dedup key is the tree HASH,
//! so two structurally-identical subtrees with distinct in-memory addresses still compress — which is
//! the whole point when a block spends the same puzzle many times.
//!
//! Byte-parity is pinned two ways, both against committed chia_rs 0.42.1 test vectors (no oracle
//! needed): the `l3` fixture from clvm_rs `ser_br.rs` (`serialize_backrefs_l3_byte_parity`), and — at
//! the generator level — `solution_generator_backrefs`'s `test_solution_generator_backre` vector (see
//! `consensus::block_generator`). This encoder is the compressed sibling of [`sexp_to_bytes`]; the
//! two agree byte-for-byte on any tree with no repeated ≥4-byte subtree (compression is a no-op).
//!
//! [`sexp_to_bytes`]: crate::clvm::parser::sexp_to_bytes

use crate::blockchain::sized_bytes::Bytes32;
use crate::clvm::program::SerializedProgram;
use crate::clvm::sexp::SExp;
use dg_xch_serialize::{CONS_BOX_MARKER, MAX_SINGLE_BYTE, encode_size};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{self, Cursor, Write};

const BACK_REFERENCE: u8 = 0xfe;

/// Node address: identity key for a live `SExp` node reached through the tree being serialized.
/// Stable for the whole serialization (the tree outlives every memo). Same soundness argument as
/// [`crate::clvm::tree_hash_cache`]: the memo never outlives the tree, so equal addresses imply the
/// same live node. Used ONLY to memoize tree-hash / serialized-length; the dedup itself keys on the
/// hash VALUE, so distinct-address identical-content subtrees still compress.
fn addr(node: &SExp) -> usize {
    std::ptr::from_ref(node) as usize
}

fn atom_tree_hash(atom: &[u8]) -> Bytes32 {
    let mut hasher = Sha256::new();
    hasher.update([1u8]);
    hasher.update(atom);
    let out: [u8; 32] = hasher.finalize().into();
    out.into()
}

fn pair_tree_hash(first: &Bytes32, rest: &Bytes32) -> Bytes32 {
    let mut hasher = Sha256::new();
    hasher.update([2u8]);
    hasher.update(first);
    hasher.update(rest);
    let out: [u8; 32] = hasher.finalize().into();
    out.into()
}

/// Serialized length (WITHOUT back-references) of a single atom — clvmr
/// `serde::serialized_length::serialized_length_atom`. Used to decide whether a back-reference is
/// actually smaller than re-serializing the node.
fn serialized_length_atom(buf: &[u8]) -> u64 {
    let lb = buf.len() as u64;
    if lb == 0 || (lb == 1 && buf[0] < 128) {
        1
    } else if lb < 0x40 {
        1 + lb
    } else if lb < 0x2000 {
        2 + lb
    } else if lb < 0x0010_0000 {
        3 + lb
    } else if lb < 0x0800_0000 {
        4 + lb
    } else {
        5 + lb
    }
}

/// Given an atom with `num_bits` significant bits (from the most-significant set bit), the number of
/// bytes it serializes to — clvmr `serde::serialized_length::atom_length_bits`. `None` when the atom
/// would be too large to serialize.
fn atom_length_bits(num_bits: u64) -> Option<u64> {
    if num_bits < 8 {
        return Some(1);
    }
    let num_bytes = num_bits.div_ceil(8);
    if num_bytes < 0x40 {
        Some(1 + num_bytes)
    } else if num_bytes < 0x2000 {
        Some(2 + num_bytes)
    } else if num_bytes < 0x0010_0000 {
        Some(3 + num_bytes)
    } else if num_bytes < 0x0800_0000 {
        Some(4 + num_bytes)
    } else if num_bytes < 0x0004_0000_0000 {
        Some(5 + num_bytes)
    } else {
        None
    }
}

/// Write a single atom in canonical CLVM form — clvmr `serde::write_atom::write_atom` (identical to
/// the atom arm of [`crate::clvm::parser::sexp_to_bytes`]): the nil/one-byte forms verbatim,
/// otherwise a minimal length prefix (`encode_size`) then the bytes.
fn write_atom<W: Write>(f: &mut W, data: &[u8]) -> io::Result<()> {
    if data.is_empty() {
        f.write_all(&[0x80])?;
    } else if data.len() == 1 && data[0] <= MAX_SINGLE_BYTE {
        f.write_all(&[data[0]])?;
    } else {
        encode_size(f, data.len() as u64)?;
        f.write_all(data)?;
    }
    Ok(())
}

/// Turn a reversed list of left(false)/right(true) steps into the `Vec<u8>` CLVM path atom — clvmr
/// `serde::read_cache_lookup::reversed_path_to_vec_u8`. `[]` => `1`; appending a `0` doubles the
/// integer, appending a `1` doubles-and-adds-one; the result is the minimal-length big-endian bytes.
fn reversed_path_to_vec_u8(path: &[bool]) -> Vec<u8> {
    let byte_count = (path.len() + 1 + 7) >> 3;
    let mut v = vec![0u8; byte_count];
    let mut index = byte_count - 1;
    let mut mask: u8 = 1;
    for &p in path.iter().rev() {
        if p {
            v[index] |= mask;
        }
        if mask == 0x80 {
            index -= 1;
            mask = 1;
        } else {
            mask <<= 1;
        }
    }
    v[index] |= mask;
    v
}

/// Tracks, during serialization, the stack of already-emitted objects (as a CLVM cons-list) so a
/// node can be replaced by the shortest CLVM path that reaches an equal-hash node already written.
/// Direct port of clvmr `serde::read_cache_lookup::ReadCacheLookup`. All map keys are sha256 tree
/// hashes.
struct ReadCacheLookup {
    root_hash: Bytes32,
    /// cons cells of the emitted-object stack: `(left_hash, right_hash)`.
    read_stack: Vec<(Bytes32, Bytes32)>,
    count: HashMap<Bytes32, u32>,
    /// tree-hash -> list of `(parent_hash, is_right_child)`.
    parent_lookup: HashMap<Bytes32, Vec<(Bytes32, bool)>>,
}

impl ReadCacheLookup {
    fn new() -> Self {
        let root_hash = atom_tree_hash(&[]);
        let mut count = HashMap::new();
        count.insert(root_hash, 1);
        Self {
            root_hash,
            read_stack: Vec::new(),
            count,
            parent_lookup: HashMap::new(),
        }
    }

    /// Update the cache for pushing an object with tree hash `id` onto the stack.
    fn push(&mut self, id: Bytes32) {
        let new_root_hash = pair_tree_hash(&id, &self.root_hash);
        self.read_stack.push((id, self.root_hash));
        *self.count.entry(id).or_insert(0) += 1;
        *self.count.entry(new_root_hash).or_insert(0) += 1;
        self.parent_lookup
            .entry(id)
            .or_default()
            .push((new_root_hash, false));
        self.parent_lookup
            .entry(self.root_hash)
            .or_default()
            .push((new_root_hash, true));
        self.root_hash = new_root_hash;
    }

    /// Pop the top object; returns `(object_hash, new_root_hash)`.
    fn pop(&mut self) -> (Bytes32, Bytes32) {
        let item = self.read_stack.pop().expect("read stack empty");
        if let Some(c) = self.count.get_mut(&item.0) {
            *c = c.saturating_sub(1);
        }
        if let Some(c) = self.count.get_mut(&self.root_hash) {
            *c = c.saturating_sub(1);
        }
        self.root_hash = item.1;
        item
    }

    /// The pop/pop/cons the serializer performs after writing a pair's two children.
    fn pop2_and_cons(&mut self) {
        let right = self.pop();
        let left = self.pop();
        *self.count.entry(left.0).or_insert(0) += 1;
        *self.count.entry(right.0).or_insert(0) += 1;
        let new_root_hash = pair_tree_hash(&left.0, &right.0);
        self.parent_lookup
            .entry(left.0)
            .or_default()
            .push((new_root_hash, false));
        self.parent_lookup
            .entry(right.0)
            .or_default()
            .push((new_root_hash, true));
        self.push(new_root_hash);
    }

    /// All minimal-length paths to `id` that serialize no larger than `serialized_length` bytes.
    fn find_paths(&self, id: &Bytes32, serialized_length: u64) -> Vec<Vec<u8>> {
        if serialized_length < 4 {
            return vec![];
        }
        let mut possible_responses = Vec::new();
        let mut seen_ids: HashSet<Bytes32> = HashSet::new();
        let max_bytes_for_path_encoding = serialized_length - 1; // 1 byte for 0xfe
        let max_path_length: usize = max_bytes_for_path_encoding
            .saturating_mul(8)
            .saturating_sub(1)
            .try_into()
            .unwrap_or(usize::MAX);
        seen_ids.insert(*id);
        let mut partial_paths: Vec<(Bytes32, Vec<bool>)> = vec![(*id, Vec::new())];

        while !partial_paths.is_empty() {
            let mut new_partial_paths: Vec<(Bytes32, Vec<bool>)> = Vec::new();
            for (node, path) in &mut partial_paths {
                if *node == self.root_hash {
                    // reversed_path_to_vec_u8 adds the terminator bit, so the encoded atom has
                    // path.len()+1 significant bits.
                    if let Some(path_len) = atom_length_bits(path.len() as u64 + 1)
                        && path_len <= max_bytes_for_path_encoding
                    {
                        possible_responses.push(reversed_path_to_vec_u8(path));
                    }
                    continue;
                }
                if let Some(items) = self.parent_lookup.get(node) {
                    for (parent, direction) in items {
                        if self.count.get(parent).copied().unwrap_or(0) > 0
                            && !seen_ids.contains(parent)
                        {
                            if path.len() > max_path_length {
                                return possible_responses;
                            }
                            if path.len() < max_path_length {
                                let mut new_path = path.clone();
                                new_path.push(*direction);
                                new_partial_paths.push((*parent, new_path));
                            }
                        }
                        seen_ids.insert(*parent);
                    }
                }
            }
            if !possible_responses.is_empty() {
                break;
            }
            partial_paths = new_partial_paths;
        }
        possible_responses
    }

    /// The lexicographically-smallest minimal-length path to `id`, if a shorter-than-`serialized_length`
    /// back-reference exists.
    fn find_path(&self, id: &Bytes32, serialized_length: u64) -> Option<Vec<u8>> {
        let mut paths = self.find_paths(id, serialized_length);
        if paths.is_empty() {
            None
        } else {
            paths.sort();
            paths.into_iter().next()
        }
    }
}

/// Post-order pass filling per-node tree-hash and (plain) serialized-length memos, keyed on node
/// address. Shared subtrees (equal address) are visited once. Iterative to avoid deep recursion on
/// long spend lists.
fn compute_memos(root: &SExp) -> (HashMap<usize, Bytes32>, HashMap<usize, u64>) {
    enum Op<'a> {
        Enter(&'a SExp<'a>),
        Exit(&'a SExp<'a>),
    }
    let mut hashes: HashMap<usize, Bytes32> = HashMap::new();
    let mut lens: HashMap<usize, u64> = HashMap::new();
    let mut stack: Vec<Op> = vec![Op::Enter(root)];
    while let Some(op) = stack.pop() {
        match op {
            Op::Enter(node) => {
                let key = addr(node);
                if hashes.contains_key(&key) {
                    continue;
                }
                match node {
                    SExp::Atom(atom) => {
                        hashes.insert(key, atom_tree_hash(atom.as_ref()));
                        lens.insert(key, serialized_length_atom(atom.as_ref()));
                    }
                    SExp::Pair(pair) => {
                        stack.push(Op::Exit(node));
                        stack.push(Op::Enter(pair.first()));
                        stack.push(Op::Enter(pair.rest()));
                    }
                }
            }
            Op::Exit(node) => {
                if let SExp::Pair(pair) = node {
                    let key = addr(node);
                    let lh = hashes[&addr(pair.first())];
                    let rh = hashes[&addr(pair.rest())];
                    hashes.insert(key, pair_tree_hash(&lh, &rh));
                    let ll = lens[&addr(pair.first())];
                    let rl = lens[&addr(pair.rest())];
                    lens.insert(key, 1u64.saturating_add(ll).saturating_add(rl));
                }
            }
        }
    }
    (hashes, lens)
}

fn node_to_stream_backrefs<W: Write>(root: &SExp, f: &mut W) -> io::Result<()> {
    #[derive(PartialEq, Eq)]
    enum ReadOp {
        Parse,
        Cons,
    }
    let (hashes, lens) = compute_memos(root);
    let mut read_op_stack: Vec<ReadOp> = vec![ReadOp::Parse];
    let mut write_stack: Vec<&SExp> = vec![root];
    let mut rcl = ReadCacheLookup::new();

    while let Some(node) = write_stack.pop() {
        let op = read_op_stack.pop();
        debug_assert!(op == Some(ReadOp::Parse));
        let key = addr(node);
        let node_hash = hashes[&key];
        let node_len = lens[&key];
        match rcl.find_path(&node_hash, node_len) {
            Some(path) => {
                f.write_all(&[BACK_REFERENCE])?;
                write_atom(f, &path)?;
                rcl.push(node_hash);
            }
            None => match node {
                SExp::Pair(pair) => {
                    f.write_all(&[CONS_BOX_MARKER])?;
                    write_stack.push(pair.rest());
                    write_stack.push(pair.first());
                    read_op_stack.push(ReadOp::Cons);
                    read_op_stack.push(ReadOp::Parse);
                    read_op_stack.push(ReadOp::Parse);
                }
                SExp::Atom(atom) => {
                    write_atom(f, atom.as_ref())?;
                    rcl.push(node_hash);
                }
            },
        }
        while let Some(ReadOp::Cons) = read_op_stack.last() {
            read_op_stack.pop();
            rcl.pop2_and_cons();
        }
    }
    Ok(())
}

/// Serialize `sexp` with CLVM back-reference compression — the inverse of
/// [`crate::clvm::parser::sexp_from_bytes_backrefs`]. Byte-identical to clvmr `node_to_bytes_backrefs`
/// (and to [`crate::clvm::parser::sexp_to_bytes`] whenever the tree has no repeated ≥4-byte subtree).
///
/// # Errors
/// Propagates the underlying `io::Error` (an in-memory `Cursor` write only fails on allocation).
pub fn sexp_to_bytes_backrefs(sexp: &SExp) -> io::Result<SerializedProgram> {
    let mut buffer = Cursor::new(Vec::new());
    node_to_stream_backrefs(sexp, &mut buffer)?;
    Ok(buffer.into_inner().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clvm::parser::{is_canonical_serialization, sexp_from_bytes_backrefs};
    use crate::clvm::sexp::{AtomBuf, PairBuf};
    use std::sync::Arc;

    fn atom(bytes: &[u8]) -> SExp<'static> {
        SExp::Atom(AtomBuf::Owned(Arc::new(bytes.to_vec())))
    }
    fn cons(first: SExp<'static>, rest: SExp<'static>) -> SExp<'static> {
        SExp::Pair(PairBuf::Owned((Arc::new(first), Arc::new(rest))))
    }

    // clvm_rs `ser_br.rs::test_serialize_limit`: leaf = [1,2,3,4,5]; l1=(leaf.leaf);
    // l2=(l1.l1); l3=(l2.l2). node_to_bytes_backrefs(l3) is the committed vector below — three
    // conses, the leaf once, then three `fe 02` back-references (path [2] = the sibling just
    // serialized). This is the canonical byte-parity anchor, straight from clvm_rs source.
    #[test]
    fn serialize_backrefs_l3_byte_parity() {
        let leaf = atom(&[1, 2, 3, 4, 5]);
        let l1 = cons(leaf.clone(), leaf.clone());
        let l2 = cons(l1.clone(), l1.clone());
        let l3 = cons(l2.clone(), l2);
        let out = sexp_to_bytes_backrefs(&l3).expect("serialize");
        assert_eq!(
            out.as_ref(),
            &[255, 255, 255, 133, 1, 2, 3, 4, 5, 254, 2, 254, 2, 254, 2],
            "node_to_bytes_backrefs(l3) must match clvm_rs's committed vector"
        );
    }

    // The compressed encoding must round-trip through the back-reference DECODER to the identical
    // tree, and be canonical.
    #[test]
    fn serialize_backrefs_round_trips_through_decoder() {
        let leaf = atom(&[9u8; 8]);
        let l1 = cons(leaf.clone(), leaf.clone());
        let l2 = cons(l1.clone(), l1.clone());
        let l3 = cons(l2.clone(), l2);
        let compressed = sexp_to_bytes_backrefs(&l3).expect("serialize");
        assert!(
            is_canonical_serialization(compressed.as_ref()),
            "back-ref serialization must be canonical"
        );
        let decoded =
            sexp_from_bytes_backrefs(&mut Cursor::new(compressed.as_ref())).expect("decode");
        assert_eq!(decoded, l3, "compressed must decode to the identical tree");
        assert_eq!(decoded.tree_hash(), l3.tree_hash());
    }

    // No repeated ≥4-byte subtree ⇒ compression is a byte-for-byte no-op: the back-ref encoder
    // agrees with the plain encoder.
    #[test]
    fn serialize_backrefs_matches_plain_when_no_repeats() {
        use crate::clvm::parser::sexp_to_bytes;
        let tree = cons(
            atom(&[0xaa; 32]),
            cons(atom(&[0xbb; 31]), cons(atom(&[1]), atom(&[]))),
        );
        let plain = sexp_to_bytes(&tree).expect("plain");
        let compressed = sexp_to_bytes_backrefs(&tree).expect("compressed");
        assert_eq!(
            compressed.as_ref(),
            plain.as_ref(),
            "no repeats ⇒ back-ref encoder equals plain encoder"
        );
    }

    // The compressed form must never be larger than the plain form, and must always round-trip.
    #[test]
    fn serialize_backrefs_never_larger_and_round_trips_fuzz() {
        use crate::clvm::parser::sexp_to_bytes;
        let mut state: u32 = 0x2b3c_4d5e;
        let mut rng = || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            state
        };
        for _ in 0..500 {
            // build a small random tree that deliberately reuses a couple of leaf atoms
            let shared_a = atom(&[(rng() as u8); 6]);
            let shared_b = atom(&[(rng() as u8); 10]);
            let mut node = atom(&[]);
            for _ in 0..(rng() % 12) {
                let pick = rng() % 4;
                let leaf = match pick {
                    0 => shared_a.clone(),
                    1 => shared_b.clone(),
                    2 => atom(&(rng().to_le_bytes())),
                    _ => cons(shared_a.clone(), shared_b.clone()),
                };
                node = cons(leaf, node);
            }
            let plain = sexp_to_bytes(&node).expect("plain");
            let compressed = sexp_to_bytes_backrefs(&node).expect("compressed");
            assert!(
                compressed.as_ref().len() <= plain.as_ref().len(),
                "compressed must never exceed plain"
            );
            let decoded =
                sexp_from_bytes_backrefs(&mut Cursor::new(compressed.as_ref())).expect("decode");
            assert_eq!(decoded, node, "compressed must round-trip to the same tree");
        }
    }
}
