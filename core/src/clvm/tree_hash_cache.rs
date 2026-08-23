//! Within-block puzzle-reveal tree-hash dedup.
//!
//! A pointer-identity mirror of chia_rs `clvm-utils/src/tree_hash.rs`
//! (`TreeCache` + `tree_hash_cached`, chia_rs 0.47.0): in
//! `chia-consensus/src/run_block_generator.rs::run_block_generator2` chia runs a
//! `visit_tree` pre-pass over every puzzle reveal and then hashes each spend with
//! `tree_hash_cached`, memoizing the hash of every subtree that appears more than
//! once. chia_rs keys that cache on `NodePtr` identity, which works because
//! `node_from_bytes_backrefs` materializes every back-reference as the SAME node —
//! a dust block that spends one puzzle hundreds of times tree-hashes it once.
//!
//! Our parser (`sexp_from_bytes_backrefs`) preserves exactly the same sharing as
//! `Arc` identity: a back-reference clones an `SExp` value whose CHILDREN remain
//! the original `Arc`s, so every shared subtree has one stable address for the
//! lifetime of the parsed generator tree. This cache keys on those addresses
//! (`&SExp as *const _`). Address identity is sound here because the cache never
//! outlives the tree it walks (one cache per generator run — see
//! `execute_block_generator_result`): nodes cannot be dropped and re-allocated
//! while the cache is live, so equal addresses imply the same live node.
//!
//! The cache can never change a hash — a hit returns the hash the same subtree
//! produced before; a miss computes exactly what `SExp::tree_hash` computes
//! (sha256(1 ‖ atom) / sha256(2 ‖ h(first) ‖ h(rest)), chia's `sha256tree`). The
//! differential suite (`core/tests/tree_hash_dedup.rs`) proves cached ≡ naive over
//! randomized shared trees and the real corpus.

use crate::blockchain::sized_bytes::Bytes32;
use crate::clvm::sexp::SExp;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// Node address: identity key for a live `SExp` node reached through the parsed
/// generator tree. Stable while the tree is alive (the cache's whole lifetime).
fn addr(node: &SExp) -> usize {
    std::ptr::from_ref(node) as usize
}

fn atom_hash(atom: &[u8]) -> Bytes32 {
    let mut hasher = Sha256::new();
    hasher.update([1u8]);
    hasher.update(atom);
    let out: [u8; 32] = hasher.finalize().into();
    out.into()
}

fn pair_hash(first: &Bytes32, rest: &Bytes32) -> Bytes32 {
    let mut hasher = Sha256::new();
    hasher.update([2u8]);
    hasher.update(first);
    hasher.update(rest);
    let out: [u8; 32] = hasher.finalize().into();
    out.into()
}

enum TreeOp<'a> {
    Node(&'a SExp<'a>),
    Cons,
    ConsCache(usize),
}

/// Per-block tree-hash memoizer. Mirrors chia_rs `TreeCache` semantics: the visit
/// pass counts how many times each PAIR node is reachable across the reveal set
/// (descending into a subtree only on first visit), and the hash pass memoizes
/// exactly the nodes seen more than once.
#[derive(Default)]
pub struct TreeHashCache {
    /// visit counts, saturating at 2 — 2 means "shared, memoize its hash".
    counts: HashMap<usize, u8>,
    hashes: HashMap<usize, Bytes32>,
}

impl TreeHashCache {
    /// Pre-pass: mark every pair node reachable from `node`, counting repeats.
    /// Mirrors chia_rs `TreeCache::visit_tree` — a repeated subtree is counted at
    /// its root and NOT re-descended, so interior nodes of a shared subtree stay
    /// at count 1 unless they are also shared through another path.
    pub fn visit_tree(&mut self, node: &SExp) {
        let mut stack: Vec<&SExp> = vec![node];
        while let Some(n) = stack.pop() {
            if let SExp::Pair(p) = n {
                let c = self.counts.entry(addr(n)).or_insert(0);
                if *c >= 1 {
                    // Revisited through sharing: mark memoize-worthy, don't re-descend.
                    *c = 2;
                    continue;
                }
                *c = 1;
                stack.push(p.first());
                stack.push(p.rest());
            }
        }
    }

    /// `sha256tree` of `node`, memoized on shared subtrees. Identical output to
    /// `SExp::tree_hash` by construction (same leaf/pair hashing, chia_rs
    /// `tree_hash_cached` op machine).
    #[must_use]
    pub fn tree_hash(&mut self, node: &SExp) -> Bytes32 {
        let mut hashes: Vec<Bytes32> = Vec::new();
        let mut ops: Vec<TreeOp> = vec![TreeOp::Node(node)];
        while let Some(op) = ops.pop() {
            match op {
                TreeOp::Node(n) => match n {
                    SExp::Atom(a) => hashes.push(atom_hash(a.as_ref())),
                    SExp::Pair(p) => {
                        let key = addr(n);
                        if let Some(h) = self.hashes.get(&key) {
                            hashes.push(*h);
                        } else {
                            if self.counts.get(&key).copied().unwrap_or(0) >= 2 {
                                ops.push(TreeOp::ConsCache(key));
                            } else {
                                ops.push(TreeOp::Cons);
                            }
                            ops.push(TreeOp::Node(p.first()));
                            ops.push(TreeOp::Node(p.rest()));
                        }
                    }
                },
                TreeOp::Cons => {
                    let first = hashes.pop().expect("tree-hash op machine invariant");
                    let rest = hashes.pop().expect("tree-hash op machine invariant");
                    hashes.push(pair_hash(&first, &rest));
                }
                TreeOp::ConsCache(key) => {
                    let first = hashes.pop().expect("tree-hash op machine invariant");
                    let rest = hashes.pop().expect("tree-hash op machine invariant");
                    let h = pair_hash(&first, &rest);
                    self.hashes.insert(key, h);
                    hashes.push(h);
                }
            }
        }
        debug_assert_eq!(hashes.len(), 1);
        hashes.pop().expect("tree-hash op machine yields one hash")
    }
}
