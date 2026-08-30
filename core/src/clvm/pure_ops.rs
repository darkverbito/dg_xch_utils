//! Operators as pure functions that cannot allocate.
//!
//! An operator today takes `&mut Arena` because it both reads its arguments and writes its result.
//! That is what forces the allocator through the whole operator surface — and it is not a style
//! choice: a value type bundling the arena with a node handle cannot exist, because holding a
//! mutable borrow of the arena inside an object that is also read immutably is what the borrow
//! checker exists to prevent.
//!
//! But an operator needs the arena mutably only to *write its result*, and every read helper
//! already takes `&Arena`. Separating the two phases dissolves the conflict:
//!
//!   1. the operator reads its arguments through `&Arena` and returns an owned description of its
//!      result — it has no way to allocate, because it never holds a mutable borrow;
//!   2. the caller, holding `&mut Arena`, materializes that description.
//!
//! The description is the point. A naive version would return `Vec<u8>` and pay a copy for every
//! computed atom, which would make the abstraction cost real performance. Instead each variant
//! names the work so the caller can do it directly into the arena: `Same` costs nothing at all,
//! `Concat` hands over the source nodes and total length so the bytes go straight into the heap
//! with no intermediate buffer, exactly as the arena-threading version does today.
//!
//! This module is a prototype covering four operators chosen to span the shapes: one that returns
//! an existing node, one that builds a pair, one that computes a small atom, and `concat` — the
//! worst case, whose output size is not known until its arguments have been walked.

use crate::clvm::arena::{Arena, ArgCursor, NodePtr};
use crate::clvm::dialect::Dialect;
use crate::clvm::sexp_ext::SExpNumber;
use crate::clvm::utils::{check_arg_count, check_cost, number_with_len, split};
use crate::errors::ClvmError;

/// What an operator produced, described rather than allocated.
pub enum OpOut {
    /// An argument returned unchanged. No allocation happens at all — the structure operators
    /// (`f`, `r`, `i`) are pure selection and should cost nothing to express.
    Same(NodePtr),
    /// A pair of two existing nodes.
    Pair(NodePtr, NodePtr),
    /// A computed number. Encoding it is the arena's job, exactly as in the arena-threading
    /// version, so the canonical byte form is produced by the same code path and cannot drift.
    /// The malloc surcharge depends on the encoded length, so the caller adds it after writing.
    /// Boxed: it is the rare variant, and inlining it would widen every return in the hot path.
    Number(Box<SExpNumber>),
    /// The concatenation of existing atoms, with the total length already known. The caller writes
    /// the bytes straight into the arena heap, so a large result costs no intermediate buffer.
    Concat(Vec<NodePtr>, usize),
}

impl OpOut {
    /// Write the described result into the arena. The only place in the operator path that holds a
    /// mutable borrow.
    #[inline]
    pub fn materialize(self, arena: &mut Arena) -> Result<NodePtr, ClvmError> {
        match self {
            OpOut::Same(node) => Ok(node),
            OpOut::Pair(first, rest) => arena.new_pair(first, rest),
            OpOut::Number(n) => arena.new_number(&n),
            OpOut::Concat(nodes, total) => arena.new_concat(total, &nodes),
        }
    }
}

/// A pure operator: reads through `&Arena`, cannot allocate, describes its result.
pub type PureOpFn =
    fn(&Arena, NodePtr, u64, &dyn Dialect) -> Result<(u64, OpOut), ClvmError>;

const FIRST_COST: u64 = 30;
const CONS_COST: u64 = 50;
const ARITH_BASE_COST: u64 = 99;
const ARITH_COST_PER_ARG: u64 = 320;
const ARITH_COST_PER_BYTE: u64 = 3;
const MALLOC_COST_PER_BYTE: u64 = 10;
const CONCAT_BASE_COST: u64 = 142;
const CONCAT_COST_PER_ARG: u64 = 135;
const CONCAT_COST_PER_BYTE: u64 = 3;

/// `f` — the zero-allocation shape. Returns an argument untouched.
#[inline]
pub fn op_first(
    arena: &Arena,
    args: NodePtr,
    _max_cost: u64,
    _dialect: &dyn Dialect,
) -> Result<(u64, OpOut), ClvmError> {
    check_arg_count(arena, args, 1, "f")?;
    let (a0, _) = split(arena, args)?;
    let (first, _) = split(arena, a0)?;
    Ok((FIRST_COST, OpOut::Same(first)))
}

/// `c` — builds structure from existing nodes without inspecting them.
#[inline]
pub fn op_cons(
    arena: &Arena,
    args: NodePtr,
    _max_cost: u64,
    _dialect: &dyn Dialect,
) -> Result<(u64, OpOut), ClvmError> {
    check_arg_count(arena, args, 2, "c")?;
    let (a0, rest) = split(arena, args)?;
    let (a1, _) = split(arena, rest)?;
    Ok((CONS_COST, OpOut::Pair(a0, a1)))
}

/// `+` — computes a small atom. The result is bytes the operator had to produce anyway.
#[inline]
pub fn op_add(
    arena: &Arena,
    args: NodePtr,
    max_cost: u64,
    _dialect: &dyn Dialect,
) -> Result<(u64, OpOut), ClvmError> {
    let mut cost = ARITH_BASE_COST;
    let mut byte_count: usize = 0;
    let mut total = SExpNumber::I128(0);
    let mut cursor = ArgCursor::new(args);
    while let Some(blob) = cursor.next(arena) {
        cost += ARITH_COST_PER_ARG;
        check_cost(cost + (byte_count as u64 * ARITH_COST_PER_BYTE), max_cost)?;
        let num_with_len = number_with_len(arena, blob)?;
        total += num_with_len.0;
        byte_count += num_with_len.1;
    }
    cost += byte_count as u64 * ARITH_COST_PER_BYTE;
    Ok((cost, OpOut::Number(Box::new(total))))
}

/// `concat` — the worst case for a non-allocating operator: the output size is unknown until every
/// argument has been walked, and the result can be large. Describing it as its source nodes plus a
/// total length lets the caller write the bytes once, directly into the arena heap.
#[inline]
pub fn op_concat(
    arena: &Arena,
    args: NodePtr,
    max_cost: u64,
    _dialect: &dyn Dialect,
) -> Result<(u64, OpOut), ClvmError> {
    let mut cost = CONCAT_BASE_COST;
    let mut total_size: usize = 0;
    let mut terms = Vec::<NodePtr>::new();
    let mut cursor = ArgCursor::new(args);
    while let Some(arg) = cursor.next(arena) {
        cost += CONCAT_COST_PER_ARG;
        check_cost(cost + total_size as u64 * CONCAT_COST_PER_BYTE, max_cost)?;
        match arena.atom_len(arg) {
            None => {
                return Err(ClvmError::Unsupported(format!(
                    "concat on list: {}",
                    arena.debug_fmt(arg)
                )));
            }
            Some(len) => total_size += len,
        }
        terms.push(arg);
    }
    cost += total_size as u64 * CONCAT_COST_PER_BYTE;
    cost += total_size as u64 * MALLOC_COST_PER_BYTE;
    check_cost(cost, max_cost)?;
    Ok((cost, OpOut::Concat(terms, total_size)))
}

/// Run a pure operator and write its result. The read phase completes and yields an owned `OpOut`
/// before the mutable borrow is taken, which is what makes the arena-free signature legal.
#[inline]
pub fn apply_pure(
    f: PureOpFn,
    arena: &mut Arena,
    args: NodePtr,
    max_cost: u64,
    dialect: &dyn Dialect,
) -> Result<(u64, NodePtr), ClvmError> {
    let (cost, out) = f(arena, args, max_cost, dialect)?;
    // `Number` is priced after encoding because the surcharge depends on the canonical length —
    // the same order the arena-threading version uses (`malloc_number` then `malloc_cost`).
    let needs_malloc_cost = matches!(out, OpOut::Number(_));
    let node = out.materialize(arena)?;
    if needs_malloc_cost {
        let len = arena
            .atom_len(node)
            .ok_or_else(|| ClvmError::ExpectedAtomGotPair(arena.display(node)))?;
        return Ok((cost + len as u64 * MALLOC_COST_PER_BYTE, node));
    }
    Ok((cost, node))
}
