//! The operator result protocol: operators describe, the runtime materializes.
//!
//! An operator needs the arena mutably only to write its result; every read helper takes
//! `&Arena`. Separating the two phases lets every operator take `&Arena` — it has no way to
//! allocate, because it never holds a mutable borrow — and moves all allocation to a single
//! materialization site in the eval loop. A `Node { allocator, ptr }` bundle cannot exist under
//! the borrow rules; splitting read from write can.
//!
//! The variants name the work so the caller does it without intermediate buffers: `Same` costs
//! nothing, `Concat` and `Substr` write through the arena's existing zero-copy paths, `Small`
//! carries fixed-size computed bytes (digests, curve points) inline, and `Number` defers encoding
//! to the arena so the canonical byte form is produced by the same code path everywhere. `Number`
//! is boxed and `Small` capped at 96 bytes so the enum stays narrow — every operator return moves
//! one of these.

use crate::clvm::arena::{Arena, NodePtr};
use crate::clvm::sexp_ext::SExpNumber;
use crate::errors::ClvmError;

/// The malloc surcharge per byte of a computed result, shared with the operator cost models.
pub const MALLOC_COST_PER_BYTE: u64 = 10;

/// What an operator produced, described rather than allocated.
pub enum OpOut {
    /// An existing node returned unchanged (`f`, `r`, `i`, the ONE/NIL constants).
    Same(NodePtr),
    /// A pair of two existing nodes (`c`, `divmod`).
    Pair(NodePtr, NodePtr),
    /// A computed number. The arena encodes it, so the canonical byte form comes from the same
    /// code path as every other number. Its malloc surcharge depends on the encoded length, so
    /// the materialization site adds it after writing — the same order the previous
    /// `malloc_number` helper used. Boxed: it is not the hot variant, and inline it would widen
    /// every operator return.
    Number(Box<SExpNumber>),
    /// Fixed-size computed bytes — sha256/coinid digests (32), G1 points (48), G2 points (96).
    /// The operator already added the malloc surcharge, since the length is a constant there.
    Small([u8; 96], u8),
    /// The concatenation of existing atoms with the total length already known; written straight
    /// into the arena heap with no intermediate buffer.
    Concat(Vec<NodePtr>, usize),
    /// A zero-copy sub-span of an existing atom.
    Substr(NodePtr, u32, u32),
    /// Two computed numbers returned as a pair (`divmod`). Both are priced together after
    /// encoding, matching the previous behaviour of allocating each and summing their lengths.
    NumberPair(Box<(SExpNumber, SExpNumber)>),
}

impl OpOut {
    /// Fixed-size computed bytes; the 96-byte cap is the largest any operator produces (a G2
    /// point).
    pub fn small(bytes: &[u8]) -> OpOut {
        let mut buf = [0u8; 96];
        buf[..bytes.len()].copy_from_slice(bytes);
        OpOut::Small(buf, bytes.len() as u8)
    }

    /// Write the described result into the arena and settle any length-dependent cost. The only
    /// place in the operator path that holds a mutable borrow.
    #[inline]
    pub fn materialize(self, arena: &mut Arena, cost: u64) -> Result<(u64, NodePtr), ClvmError> {
        match self {
            OpOut::Same(node) => Ok((cost, node)),
            OpOut::Pair(first, rest) => Ok((cost, arena.new_pair(first, rest)?)),
            OpOut::Number(n) => {
                let node = arena.new_number(&n)?;
                let len = arena
                    .atom_len(node)
                    .ok_or_else(|| ClvmError::ExpectedAtomGotPair(arena.display(node)))?;
                Ok((cost + len as u64 * MALLOC_COST_PER_BYTE, node))
            }
            OpOut::Small(buf, len) => Ok((cost, arena.new_atom(&buf[..len as usize])?)),
            OpOut::Concat(nodes, total) => Ok((cost, arena.new_concat(total, &nodes)?)),
            OpOut::Substr(node, start, end) => Ok((cost, arena.new_substr(node, start, end)?)),
            OpOut::NumberPair(qr) => {
                let (q, r) = *qr;
                let q1 = arena.new_number(&q)?;
                let r1 = arena.new_number(&r)?;
                let c = (arena.atom_len(q1).unwrap_or(0) + arena.atom_len(r1).unwrap_or(0)) as u64
                    * MALLOC_COST_PER_BYTE;
                Ok((cost + c, arena.new_pair(q1, r1)?))
            }
        }
    }
}
