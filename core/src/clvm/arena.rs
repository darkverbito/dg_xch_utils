//! Compact CLVM node arena — u32 handles into typed pools, mirroring clvm_rs 0.17.7
//! `src/allocator.rs` (`Allocator { u8_vec, atom_vec, pair_vec }`, `NodePtr` with a 6-bit
//! object tag over a 26-bit index, small positive integers encoded inline in the handle, and
//! ghost accounting so the inline optimization cannot change the consensus atom/pair limits).
//!
//! This replaces the bumpalo arena + per-op-result deep-copy representation: an eval
//! intermediate now costs 8 bytes (a pair) or its atom bytes exactly once — never a deep
//! clone of the accumulated tree. On a dust-era ROM bootstrap run the old representation
//! retained 429 of 430 MiB of peak heap in eval intermediates (pre-arena baseline).
//!
//! Consensus limits (`MAX_NUM_ATOMS` / `MAX_NUM_PAIRS` = 62,500,000, 4 GiB heap ceiling)
//! and the small-atom canonical-form test (`fits_in_small_atom`) are verbatim from
//! clvm_rs 0.17.7, the representation proven at mainnet scale.

use crate::clvm::sexp::{AtomBuf, PairBuf, SExp};
use crate::clvm::sexp_ext::SExpNumber;
use crate::errors::ClvmError;
use crate::formatting::{bigint_to_bytes, number_from_slice};
use num_bigint::{BigInt, Sign};
use std::fmt;
use std::sync::Arc;

// clvm_rs 0.17.7 allocator.rs consensus limits.
const MAX_NUM_ATOMS: usize = 62_500_000;
const MAX_NUM_PAIRS: usize = 62_500_000;
const NODE_PTR_IDX_BITS: u32 = 26;
const NODE_PTR_IDX_MASK: u32 = (1 << NODE_PTR_IDX_BITS) - 1;
// Handles are 32-bit; the byte heap is addressed by u32 spans.
const HEAP_LIMIT: usize = u32::MAX as usize;

/// A compact handle to a node in an [`Arena`]. The top 6 bits carry the object type, the low
/// 26 bits an index (pair pool, atom pool) or the small-atom value itself, exactly as
/// clvm_rs 0.17.7 `NodePtr`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodePtr(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectType {
    /// Low bits index `pair_vec`.
    Pair,
    /// Low bits index `atom_vec`.
    Bytes,
    /// Low bits are the atom value itself (canonical unsigned integer, ≤ 26 bits).
    SmallAtom,
}

impl NodePtr {
    pub const NIL: Self = Self::new(ObjectType::SmallAtom, 0);
    pub const ONE: Self = Self::new(ObjectType::SmallAtom, 1);

    #[allow(clippy::cast_possible_truncation)]
    const fn new(object_type: ObjectType, index: usize) -> Self {
        debug_assert!(index <= NODE_PTR_IDX_MASK as usize);
        NodePtr(((object_type as u32) << NODE_PTR_IDX_BITS) | (index as u32))
    }

    #[must_use]
    pub fn object_type(self) -> ObjectType {
        match self.0 >> NODE_PTR_IDX_BITS {
            0 => ObjectType::Pair,
            1 => ObjectType::Bytes,
            2 => ObjectType::SmallAtom,
            _ => unreachable!(),
        }
    }

    #[must_use]
    pub fn index(self) -> u32 {
        self.0 & NODE_PTR_IDX_MASK
    }

    #[must_use]
    pub fn is_atom(self) -> bool {
        !self.is_pair()
    }

    #[must_use]
    pub fn is_pair(self) -> bool {
        self.object_type() == ObjectType::Pair
    }
}

impl Default for NodePtr {
    fn default() -> Self {
        Self::NIL
    }
}

impl fmt::Debug for NodePtr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("NodePtr")
            .field(&self.object_type())
            .field(&self.index())
            .finish()
    }
}

/// Shape of a node: an atom, or a pair of child handles. Mirrors clvm_rs `SExp` (renamed to
/// avoid colliding with the crate's tree-owning [`SExp`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Atom,
    Pair(NodePtr, NodePtr),
}

/// Atom bytes: borrowed from the arena heap, or the canonical encoding of an inline small
/// atom materialized into a 4-byte buffer. Mirrors clvm_rs `Atom`.
#[derive(Debug, Clone, Copy)]
pub enum Atom<'a> {
    Borrowed(&'a [u8]),
    U32([u8; 4], usize),
}

impl AsRef<[u8]> for Atom<'_> {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Borrowed(bytes) => bytes,
            Self::U32(bytes, len) => &bytes[4 - len..],
        }
    }
}

impl Atom<'_> {
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Borrowed(bytes) => bytes.len(),
            Self::U32(_, len) => *len,
        }
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Clone, Copy, Debug)]
struct AtomSpan {
    start: u32,
    end: u32,
}

impl AtomSpan {
    fn len(&self) -> usize {
        (self.end - self.start) as usize
    }
}

#[derive(Clone, Copy, Debug)]
struct IntPair {
    first: NodePtr,
    rest: NodePtr,
}

/// Returns the inline value if `v` is the canonical encoding of an unsigned integer that
/// fits in 26 bits. Verbatim logic from clvm_rs 0.17.7 `fits_in_small_atom`.
#[must_use]
pub fn fits_in_small_atom(v: &[u8]) -> Option<u32> {
    if !v.is_empty()
        && (v.len() > 4
        || (v.len() == 1 && v[0] == 0)
        // a 1-byte buffer of 0 is not the canonical representation of 0
        || (v[0] & 0x80) != 0
        // if the top bit is set, it's a negative number (i.e. not positive)
        || (v[0] == 0 && (v[1] & 0x80) == 0)
        // a leading zero is only canonical when it protects a set high bit
        || (v.len() == 4 && v[0] > 0x03))
    {
        None
    } else {
        let mut ret: u32 = 0;
        for b in v {
            ret <<= 8;
            ret |= u32::from(*b);
        }
        Some(ret)
    }
}

/// Length of the canonical encoding of a small-atom value. Verbatim from clvm_rs 0.17.7.
#[must_use]
pub fn len_for_value(val: u32) -> usize {
    if val == 0 {
        0
    } else if val < 0x80 {
        1
    } else if val < 0x8000 {
        2
    } else if val < 0x0080_0000 {
        3
    } else if val < 0x8000_0000 {
        4
    } else {
        5
    }
}

/// The compact node store. All eval-time allocation goes through this; `reset` truncates the
/// pools without releasing capacity, so a reused runtime performs no steady-state mallocs.
pub struct Arena {
    // grow-only byte heap for atom contents (atoms are immutable once created)
    u8_vec: Vec<u8>,
    // pair pool — 8 bytes per cons cell
    pair_vec: Vec<IntPair>,
    // atom pool — (start, end) spans into u8_vec
    atom_vec: Vec<AtomSpan>,
    // ghost counters account for atoms/pairs/heap-bytes that were optimized out (inline
    // small atoms, zero-copy substr/concat shortcuts) so the consensus limits are unchanged
    // by the optimizations — clvm_rs 0.17.7 semantics.
    ghost_atoms: usize,
    ghost_pairs: usize,
    ghost_heap: usize,
}

impl Default for Arena {
    fn default() -> Self {
        Self::new()
    }
}

impl Arena {
    #[must_use]
    pub fn new() -> Self {
        let mut arena = Self {
            u8_vec: Vec::new(),
            pair_vec: Vec::new(),
            atom_vec: Vec::new(),
            ghost_atoms: 0,
            ghost_pairs: 0,
            ghost_heap: 0,
        };
        arena.u8_vec.reserve(1024 * 1024);
        arena.atom_vec.reserve(256);
        arena.pair_vec.reserve(256);
        arena.reset();
        arena
    }

    /// Truncate all pools (capacity retained) and reset the ghost counters to the clvm_rs
    /// initial state (2 ghost atoms + 1 ghost heap byte, standing in for the nil()/one()
    /// clvm_rs historically allocated).
    pub fn reset(&mut self) {
        self.u8_vec.clear();
        self.pair_vec.clear();
        self.atom_vec.clear();
        self.ghost_atoms = 2;
        self.ghost_pairs = 0;
        self.ghost_heap = 1;
    }

    fn check_atom_limit(&self) -> Result<(), ClvmError> {
        if self.atom_vec.len() + self.ghost_atoms == MAX_NUM_ATOMS {
            Err(ClvmError::TooManyAtoms)
        } else {
            Ok(())
        }
    }

    pub fn new_atom(&mut self, v: &[u8]) -> Result<NodePtr, ClvmError> {
        let start = self.u8_vec.len();
        if start + self.ghost_heap + v.len() > HEAP_LIMIT {
            return Err(ClvmError::OutOfMemory);
        }
        self.check_atom_limit()?;
        if let Some(val) = fits_in_small_atom(v) {
            self.ghost_atoms += 1;
            self.ghost_heap += v.len();
            Ok(NodePtr::new(ObjectType::SmallAtom, val as usize))
        } else {
            let idx = self.atom_vec.len();
            self.u8_vec.extend_from_slice(v);
            #[allow(clippy::cast_possible_truncation)]
            self.atom_vec.push(AtomSpan {
                start: start as u32,
                end: self.u8_vec.len() as u32,
            });
            Ok(NodePtr::new(ObjectType::Bytes, idx))
        }
    }

    pub fn new_pair(&mut self, first: NodePtr, rest: NodePtr) -> Result<NodePtr, ClvmError> {
        let idx = self.pair_vec.len();
        if idx >= MAX_NUM_PAIRS - self.ghost_pairs {
            return Err(ClvmError::TooManyPairs);
        }
        self.pair_vec.push(IntPair { first, rest });
        Ok(NodePtr::new(ObjectType::Pair, idx))
    }

    /// Zero-copy substring view of an atom, clvm_rs 0.17.7 `new_substr`: a new span into the
    /// parent's bytes for pool atoms; re-encode (and re-inline when canonical) for small atoms.
    pub fn new_substr(
        &mut self,
        node: NodePtr,
        start: u32,
        end: u32,
    ) -> Result<NodePtr, ClvmError> {
        self.check_atom_limit()?;
        fn bounds_check(start: u32, end: u32, len: u32) -> Result<(), ClvmError> {
            if start > len {
                return Err(ClvmError::InvalidInput(format!(
                    "substr start out of bounds: {start} is > {len}"
                )));
            }
            if end > len {
                return Err(ClvmError::InvalidInput(format!(
                    "substr end out of bounds: {end} is > {len}"
                )));
            }
            if end < start {
                return Err(ClvmError::InvalidInput(format!(
                    "substr invalid bounds: {start} is > {end}"
                )));
            }
            Ok(())
        }
        match node.object_type() {
            ObjectType::Pair => Err(ClvmError::ExpectedAtomGotPair("substr on pair".to_string())),
            ObjectType::Bytes => {
                let atom = self.atom_vec[node.index() as usize];
                bounds_check(start, end, atom.end - atom.start)?;
                let idx = self.atom_vec.len();
                self.atom_vec.push(AtomSpan {
                    start: atom.start + start,
                    end: atom.start + end,
                });
                Ok(NodePtr::new(ObjectType::Bytes, idx))
            }
            ObjectType::SmallAtom => {
                let val = node.index();
                #[allow(clippy::cast_possible_truncation)]
                let len = len_for_value(val) as u32;
                bounds_check(start, end, len)?;
                let buf: [u8; 4] = val.to_be_bytes();
                let buf = &buf[4 - len as usize..];
                let substr = &buf[start as usize..end as usize];
                if let Some(new_val) = fits_in_small_atom(substr) {
                    self.ghost_atoms += 1;
                    Ok(NodePtr::new(ObjectType::SmallAtom, new_val as usize))
                } else {
                    let heap_start = self.u8_vec.len();
                    let owned = substr.to_vec();
                    self.u8_vec.extend_from_slice(&owned);
                    let idx = self.atom_vec.len();
                    #[allow(clippy::cast_possible_truncation)]
                    self.atom_vec.push(AtomSpan {
                        start: heap_start as u32,
                        end: self.u8_vec.len() as u32,
                    });
                    Ok(NodePtr::new(ObjectType::Bytes, idx))
                }
            }
        }
    }

    /// Concatenation into a single fresh atom, clvm_rs 0.17.7 `new_concat` (including the
    /// zero- and one-term ghost shortcuts, which keep allocation counters consensus-exact).
    pub fn new_concat(
        &mut self,
        new_size: usize,
        nodes: &[NodePtr],
    ) -> Result<NodePtr, ClvmError> {
        self.check_atom_limit()?;
        let start = self.u8_vec.len();
        if start + self.ghost_heap + new_size > HEAP_LIMIT {
            return Err(ClvmError::OutOfMemory);
        }
        if nodes.is_empty() {
            if new_size != 0 {
                return Err(ClvmError::InvalidInput(
                    "concat passed invalid new_size".to_string(),
                ));
            }
            self.ghost_atoms += 1;
            return Ok(NodePtr::NIL);
        }
        if nodes.len() == 1 {
            let Some(len) = self.atom_len(nodes[0]) else {
                return Err(ClvmError::ExpectedAtomGotPair("concat on pair".to_string()));
            };
            if len != new_size {
                return Err(ClvmError::InvalidInput(
                    "concat passed invalid new_size".to_string(),
                ));
            }
            self.ghost_heap += new_size;
            self.ghost_atoms += 1;
            return Ok(nodes[0]);
        }
        self.u8_vec.reserve(new_size);
        let mut counter: usize = 0;
        for node in nodes {
            match node.object_type() {
                ObjectType::Pair => {
                    self.u8_vec.truncate(start);
                    return Err(ClvmError::ExpectedAtomGotPair("concat on pair".to_string()));
                }
                ObjectType::Bytes => {
                    let term = self.atom_vec[node.index() as usize];
                    if counter + term.len() > new_size {
                        self.u8_vec.truncate(start);
                        return Err(ClvmError::InvalidInput(
                            "concat passed invalid new_size".to_string(),
                        ));
                    }
                    self.u8_vec
                        .extend_from_within(term.start as usize..term.end as usize);
                    counter += term.len();
                }
                ObjectType::SmallAtom => {
                    let val = node.index();
                    let len = len_for_value(val);
                    let buf: [u8; 4] = val.to_be_bytes();
                    self.u8_vec.extend_from_slice(&buf[4 - len..]);
                    counter += len;
                }
            }
        }
        if counter != new_size {
            self.u8_vec.truncate(start);
            return Err(ClvmError::InvalidInput(
                "concat passed invalid new_size".to_string(),
            ));
        }
        let idx = self.atom_vec.len();
        #[allow(clippy::cast_possible_truncation)]
        self.atom_vec.push(AtomSpan {
            start: start as u32,
            end: self.u8_vec.len() as u32,
        });
        Ok(NodePtr::new(ObjectType::Bytes, idx))
    }

    /// Encode a number as a fresh atom with the crate's minimal signed big-endian encoding
    /// (identical bytes to the previous `SExp::from(SExpNumber)` construction paths).
    pub fn new_number(&mut self, n: &SExpNumber) -> Result<NodePtr, ClvmError> {
        match n {
            SExpNumber::I128(v) => self.new_i128(*v),
            SExpNumber::BigInt(b) => self.new_bigint(b),
        }
    }

    pub fn new_i128(&mut self, v: i128) -> Result<NodePtr, ClvmError> {
        if v == 0 {
            return self.new_atom(&[]);
        }
        let raw = v.to_be_bytes();
        let mut s: &[u8] = raw.as_slice();
        while s.len() > 1 && s[0] == (u8::from(s[1] & 0x80 > 0) * 0xFF) {
            s = &s[1..];
        }
        self.new_atom(s)
    }

    pub fn new_bigint(&mut self, v: &BigInt) -> Result<NodePtr, ClvmError> {
        let bytes = bigint_to_bytes(v, v.sign() != Sign::NoSign);
        self.new_atom(&bytes)
    }

    /// The atom bytes for `node`, or `None` if it is a pair.
    #[must_use]
    pub fn atom(&self, node: NodePtr) -> Option<Atom<'_>> {
        match node.object_type() {
            ObjectType::Bytes => {
                let atom = self.atom_vec[node.index() as usize];
                Some(Atom::Borrowed(
                    &self.u8_vec[atom.start as usize..atom.end as usize],
                ))
            }
            ObjectType::SmallAtom => {
                let val = node.index();
                Some(Atom::U32(val.to_be_bytes(), len_for_value(val)))
            }
            ObjectType::Pair => None,
        }
    }

    /// The atom length for `node`, or `None` if it is a pair.
    #[must_use]
    pub fn atom_len(&self, node: NodePtr) -> Option<usize> {
        match node.object_type() {
            ObjectType::Bytes => Some(self.atom_vec[node.index() as usize].len()),
            ObjectType::SmallAtom => Some(len_for_value(node.index())),
            ObjectType::Pair => None,
        }
    }

    /// Signed-integer decode of an atom (same semantics as `SExpNumber::from(&AtomBuf)`),
    /// or `None` for a pair.
    #[must_use]
    pub fn number(&self, node: NodePtr) -> Option<SExpNumber> {
        match node.object_type() {
            ObjectType::SmallAtom => Some(SExpNumber::I128(i128::from(node.index()))),
            ObjectType::Bytes => {
                let atom = self.atom_vec[node.index() as usize];
                let buf = &self.u8_vec[atom.start as usize..atom.end as usize];
                Some(match buf.len() {
                    0 => SExpNumber::I128(0),
                    x if x <= 16 => {
                        let fill = if buf[0] & 0x80 != 0 { 0xff } else { 0x00 };
                        let mut int_buf = [fill; 16];
                        int_buf[(16 - x)..].copy_from_slice(buf);
                        SExpNumber::I128(i128::from_be_bytes(int_buf))
                    }
                    _ => SExpNumber::BigInt(number_from_slice(buf)),
                })
            }
            ObjectType::Pair => None,
        }
    }

    #[must_use]
    pub fn node_kind(&self, node: NodePtr) -> NodeKind {
        match node.object_type() {
            ObjectType::Pair => {
                let pair = self.pair_vec[node.index() as usize];
                NodeKind::Pair(pair.first, pair.rest)
            }
            ObjectType::Bytes | ObjectType::SmallAtom => NodeKind::Atom,
        }
    }

    /// `(first, rest)` if `node` is a pair.
    #[must_use]
    pub fn next(&self, node: NodePtr) -> Option<(NodePtr, NodePtr)> {
        match self.node_kind(node) {
            NodeKind::Pair(first, rest) => Some((first, rest)),
            NodeKind::Atom => None,
        }
    }

    #[must_use]
    pub fn nullp(&self, node: NodePtr) -> bool {
        node == NodePtr::NIL || matches!(self.atom_len(node), Some(0))
    }

    #[must_use]
    pub fn non_nil(&self, node: NodePtr) -> bool {
        !self.nullp(node)
    }

    /// Count of list elements (pairs walked down `rest`), stopping early past
    /// `return_early_if_exceeds` — mirrors `SExp::arg_count`.
    #[must_use]
    pub fn arg_count(&self, node: NodePtr, return_early_if_exceeds: usize) -> usize {
        let mut count = 0;
        let mut ptr = node;
        while let Some((_, rest)) = self.next(ptr) {
            ptr = rest;
            count += 1;
            if count > return_early_if_exceeds {
                break;
            }
        }
        count
    }

    /// True when `node` is a proper list of exactly `count` elements — mirrors
    /// `SExp::arg_count_is`.
    #[must_use]
    pub fn arg_count_is(&self, node: NodePtr, mut count: usize) -> bool {
        let mut ptr = node;
        loop {
            if count == 0 {
                return self.nullp(ptr);
            }
            match self.next(ptr) {
                Some((_, rest)) => ptr = rest,
                None => return false,
            }
            count -= 1;
        }
    }

    /// Mirrors `SExp::as_atom_list`: leading atoms of a proper list; an empty vec if the
    /// head of any pair is itself a pair.
    #[must_use]
    pub fn as_atom_list(&self, node: NodePtr) -> Vec<Vec<u8>> {
        let mut rtn: Vec<Vec<u8>> = Vec::new();
        let mut cur = node;
        while let Some((first, rest)) = self.next(cur) {
            match self.atom(first) {
                Some(a) => rtn.push(a.as_ref().to_vec()),
                None => return vec![],
            }
            cur = rest;
        }
        rtn
    }

    /// Deep-copy an owned/borrowed [`SExp`] tree into the arena. Iterative — a long CLVM
    /// list is deep in the `rest` direction and must not recurse.
    pub fn import(&mut self, sexp: &SExp) -> Result<NodePtr, ClvmError> {
        enum Job<'x> {
            Visit(&'x SExp<'x>),
            Build,
        }
        let mut jobs: Vec<Job> = vec![Job::Visit(sexp)];
        let mut out: Vec<NodePtr> = Vec::new();
        while let Some(job) = jobs.pop() {
            match job {
                Job::Visit(SExp::Atom(a)) => out.push(self.new_atom(a.as_ref())?),
                Job::Visit(SExp::Pair(p)) => {
                    jobs.push(Job::Build);
                    jobs.push(Job::Visit(p.rest()));
                    jobs.push(Job::Visit(p.first()));
                }
                Job::Build => {
                    let rest = out.pop().ok_or(ClvmError::ValueStackEmpty)?;
                    let first = out.pop().ok_or(ClvmError::ValueStackEmpty)?;
                    out.push(self.new_pair(first, rest)?);
                }
            }
        }
        out.pop().ok_or(ClvmError::ValueStackEmpty)
    }

    /// Materialize an arena subtree as an owned [`SExp`] tree. Iterative for the same
    /// reason as [`Arena::import`].
    #[must_use]
    pub fn export(&self, node: NodePtr) -> SExp<'static> {
        enum Job {
            Visit(NodePtr),
            Build,
        }
        let mut jobs: Vec<Job> = vec![Job::Visit(node)];
        let mut out: Vec<SExp<'static>> = Vec::new();
        while let Some(job) = jobs.pop() {
            match job {
                Job::Visit(ptr) => match self.node_kind(ptr) {
                    NodeKind::Atom => {
                        let bytes = self
                            .atom(ptr)
                            .expect("node_kind atom has bytes")
                            .as_ref()
                            .to_vec();
                        out.push(SExp::Atom(AtomBuf::new(bytes)));
                    }
                    NodeKind::Pair(first, rest) => {
                        jobs.push(Job::Build);
                        jobs.push(Job::Visit(rest));
                        jobs.push(Job::Visit(first));
                    }
                },
                Job::Build => {
                    let rest = out.pop().expect("build has rest");
                    let first = out.pop().expect("build has first");
                    out.push(SExp::Pair(PairBuf::Owned((Arc::new(first), Arc::new(rest)))));
                }
            }
        }
        out.pop().expect("export produced a node")
    }

    /// Render a subtree with the crate's canonical `SExp` `Display` — error paths only.
    #[must_use]
    pub fn display(&self, node: NodePtr) -> String {
        self.export(node).to_string()
    }

    /// Render a subtree with the crate's canonical `SExp` `Debug` — error paths only.
    #[must_use]
    pub fn debug_fmt(&self, node: NodePtr) -> String {
        format!("{:?}", self.export(node))
    }

    /// Allocation counters (allocated + ghost: atoms, pairs, heap bytes), for probes and
    /// limit diagnostics.
    #[must_use]
    pub fn counters(&self) -> (usize, usize, usize) {
        (
            self.atom_vec.len() + self.ghost_atoms,
            self.pair_vec.len() + self.ghost_pairs,
            self.u8_vec.len() + self.ghost_heap,
        )
    }
}

/// Argument-list cursor with the exact semantics of the tree walker's `SExpIter`: yields
/// each pair's `first`; at a NON-NIL terminal atom yields that atom itself once, then stops
/// (improper tails surface their tail); at nil stops. Holds no borrow between calls so ops
/// can allocate while iterating.
pub struct ArgCursor {
    cur: NodePtr,
    done: bool,
}

impl ArgCursor {
    #[must_use]
    pub fn new(args: NodePtr) -> Self {
        Self {
            cur: args,
            done: false,
        }
    }
    pub fn next(&mut self, arena: &Arena) -> Option<NodePtr> {
        if self.done {
            return None;
        }
        match arena.node_kind(self.cur) {
            NodeKind::Pair(first, rest) => {
                self.cur = rest;
                Some(first)
            }
            NodeKind::Atom => {
                self.done = true;
                if arena.non_nil(self.cur) {
                    Some(self.cur)
                } else {
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Round-trip and representation tests for the compact arena. The deterministic
    //! pseudo-random round-trip is the property test for the import/export boundary: for
    //! every generated tree, import→export must reproduce the exact atom bytes and shape.
    use super::*;
    use crate::constants::{NULL_SEXP, ONE_SEXP};

    #[test]
    fn nil_and_one_are_inline() {
        let mut a = Arena::new();
        assert_eq!(a.new_atom(&[]).unwrap(), NodePtr::NIL);
        assert_eq!(a.new_atom(&[1]).unwrap(), NodePtr::ONE);
        assert!(a.nullp(NodePtr::NIL));
        assert!(a.non_nil(NodePtr::ONE));
        assert_eq!(a.atom(NodePtr::NIL).unwrap().as_ref(), &[] as &[u8]);
        assert_eq!(a.atom(NodePtr::ONE).unwrap().as_ref(), &[1]);
    }

    #[test]
    fn small_atom_canonical_forms_only() {
        assert_eq!(fits_in_small_atom(&[]), Some(0));
        assert_eq!(fits_in_small_atom(&[0x7f]), Some(0x7f));
        assert_eq!(fits_in_small_atom(&[0x00, 0x80]), Some(0x80));
        assert_eq!(
            fits_in_small_atom(&[0x03, 0xff, 0xff, 0xff]),
            Some(0x03ff_ffff)
        );
        assert_eq!(fits_in_small_atom(&[0x00]), None); // non-canonical zero
        assert_eq!(fits_in_small_atom(&[0x80]), None); // negative
        assert_eq!(fits_in_small_atom(&[0x00, 0x7f]), None); // redundant leading zero
        assert_eq!(fits_in_small_atom(&[0x04, 0x00, 0x00, 0x00]), None); // > 26 bits
        assert_eq!(fits_in_small_atom(&[1, 2, 3, 4, 5]), None); // too long
        let mut a = Arena::new();
        for bytes in [
            vec![0x00],
            vec![0x80],
            vec![0x00, 0x7f],
            vec![0xff, 0xff],
            vec![1, 2, 3, 4, 5],
        ] {
            let ptr = a.new_atom(&bytes).unwrap();
            assert_eq!(a.atom(ptr).unwrap().as_ref(), &bytes[..], "{bytes:?}");
            assert_eq!(a.atom_len(ptr).unwrap(), bytes.len());
        }
    }

    #[test]
    fn len_for_value_matches_canonical_encoding() {
        for v in [
            0u32, 1, 0x7f, 0x80, 0x7fff, 0x8000, 0x7f_ffff, 0x80_0000, 0x03ff_ffff,
        ] {
            let mut a = Arena::new();
            let ptr = a.new_i128(i128::from(v)).unwrap();
            assert_eq!(a.atom_len(ptr).unwrap(), len_for_value(v), "{v:#x}");
            match a.number(ptr).unwrap() {
                SExpNumber::I128(got) => assert_eq!(got, i128::from(v)),
                SExpNumber::BigInt(_) => panic!("small value decoded as bigint"),
            }
        }
    }

    #[test]
    fn number_encode_matches_sexp_from() {
        // arena number encoding must be byte-identical to SExp::from's minimal signed
        // big-endian, the encoding the previous representation shipped on the wire.
        let mut a = Arena::new();
        for v in [
            0i64,
            1,
            -1,
            127,
            128,
            -128,
            255,
            256,
            -256,
            65535,
            -65536,
            1 << 25,
            1 << 26,
            i64::MAX,
            i64::MIN,
        ] {
            let ptr = a.new_i128(i128::from(v)).unwrap();
            let expected = SExp::from(v);
            assert_eq!(
                a.atom(ptr).unwrap().as_ref(),
                expected.atom().unwrap().as_ref(),
                "{v}"
            );
        }
        for v in [
            BigInt::from(0),
            BigInt::from(1) << 200,
            -(BigInt::from(1) << 200i32),
        ] {
            let ptr = a.new_bigint(&v).unwrap();
            let expected = SExp::from(&v);
            assert_eq!(
                a.atom(ptr).unwrap().as_ref(),
                expected.atom().unwrap().as_ref(),
                "{v}"
            );
        }
    }

    #[test]
    fn substr_views_and_small_atoms() {
        let mut a = Arena::new();
        let big = a.new_atom(b"hello world").unwrap();
        let sub = a.new_substr(big, 6, 11).unwrap();
        assert_eq!(a.atom(sub).unwrap().as_ref(), b"world");
        let small = a.new_atom(&[0x01, 0x02]).unwrap();
        let sub2 = a.new_substr(small, 1, 2).unwrap();
        assert_eq!(a.atom(sub2).unwrap().as_ref(), &[0x02]);
        let small3 = a.new_atom(&[0x01, 0x00]).unwrap();
        let sub3 = a.new_substr(small3, 1, 2).unwrap();
        assert_eq!(a.atom(sub3).unwrap().as_ref(), &[0x00]);
        assert!(a.new_substr(big, 12, 12).is_err());
        assert!(a.new_substr(big, 3, 2).is_err());
    }

    #[test]
    fn concat_copies_all_terms() {
        let mut a = Arena::new();
        let x = a.new_atom(b"foo").unwrap();
        let y = a.new_atom(b"bar").unwrap();
        let small = a.new_atom(&[0x01]).unwrap();
        let cat = a.new_concat(7, &[x, y, small]).unwrap();
        assert_eq!(a.atom(cat).unwrap().as_ref(), b"foobar\x01");
        let same = a.new_concat(3, &[x]).unwrap();
        assert_eq!(same, x);
        assert!(a.new_concat(5, &[x, y]).is_err());
    }

    #[test]
    fn import_export_round_trips_shape_and_bytes() {
        let tree = SExp::from(vec![
            SExp::from(1),
            SExp::from((2_u8, 3_u8)),
            SExp::from(b"some longer atom content".to_vec()),
            SExp::from(vec![SExp::from(-1), SExp::from(0), SExp::from(128)]),
        ]);
        let mut a = Arena::new();
        let ptr = a.import(&tree).unwrap();
        let back = a.export(ptr);
        assert_eq!(back, tree);
    }

    // Deterministic pseudo-random round-trip property: 2,000 generated trees with mixed
    // atom encodings (canonical, non-canonical, long) and nesting must survive
    // import→export byte-identically.
    #[test]
    fn round_trip_property_pseudo_random() {
        fn xorshift(state: &mut u64) -> u64 {
            let mut x = *state;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *state = x;
            x
        }
        fn gen_tree(state: &mut u64, depth: u32) -> SExp<'static> {
            let r = xorshift(state);
            if depth == 0 || r.is_multiple_of(3) {
                let len = (xorshift(state) % 40) as usize;
                let bytes: Vec<u8> = (0..len).map(|_| (xorshift(state) & 0xff) as u8).collect();
                SExp::Atom(AtomBuf::new(bytes))
            } else {
                let first = gen_tree(state, depth - 1);
                let rest = gen_tree(state, depth - 1);
                SExp::Pair(PairBuf::Owned((Arc::new(first), Arc::new(rest))))
            }
        }
        let mut state = 0x181_cafe_f00d_u64;
        for i in 0..2000 {
            let tree = gen_tree(&mut state, 6);
            let mut a = Arena::new();
            let ptr = a.import(&tree).unwrap();
            assert_eq!(a.export(ptr), tree, "case {i}");
        }
    }

    #[test]
    fn arg_cursor_matches_sexp_iter_semantics() {
        let mut a = Arena::new();
        let list = a
            .import(&SExp::from(vec![
                SExp::from(1),
                SExp::from(2),
                SExp::from(3),
            ]))
            .unwrap();
        let mut cur = ArgCursor::new(list);
        let mut seen = Vec::new();
        while let Some(p) = cur.next(&a) {
            seen.push(a.number(p).unwrap());
        }
        assert_eq!(seen.len(), 3);
        // improper tail (1 2 . 3) yields the tail atom — SExpIter parity
        let improper = a.import(&SExp::from((1_u8, (2_u8, 3_u8)))).unwrap();
        let mut cur = ArgCursor::new(improper);
        let mut count = 0;
        while cur.next(&a).is_some() {
            count += 1;
        }
        assert_eq!(count, 3);
        let nil = a.import(&NULL_SEXP).unwrap();
        let mut cur = ArgCursor::new(nil);
        assert!(cur.next(&a).is_none());
        let one = a.import(&ONE_SEXP).unwrap();
        let mut cur = ArgCursor::new(one);
        assert_eq!(cur.next(&a), Some(NodePtr::ONE));
        assert!(cur.next(&a).is_none());
    }

    #[test]
    fn arg_count_and_atom_list() {
        let mut a = Arena::new();
        let list = a
            .import(&SExp::from(vec![
                SExp::from(1),
                SExp::from(2),
                SExp::from(3),
            ]))
            .unwrap();
        assert_eq!(a.arg_count(list, 10), 3);
        assert!(a.arg_count_is(list, 3));
        assert!(!a.arg_count_is(list, 2));
        assert_eq!(a.as_atom_list(list), vec![vec![1u8], vec![2u8], vec![3u8]]);
        let nested = a
            .import(&SExp::from(vec![
                SExp::from(vec![SExp::from(1)]),
                SExp::from(2),
            ]))
            .unwrap();
        assert!(a.as_atom_list(nested).is_empty());
    }

    #[test]
    fn reset_reclaims_pools() {
        let mut a = Arena::new();
        let _ = a.new_atom(b"some content").unwrap();
        let x = a.new_atom(b"another atom here").unwrap();
        let _ = a.new_pair(x, NodePtr::NIL).unwrap();
        let before = a.counters();
        assert!(before.0 > 2 && before.1 > 0);
        a.reset();
        assert_eq!(a.counters(), (2, 0, 1));
    }
}
