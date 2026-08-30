use crate::clvm::arena::{Arena, NodeKind, NodePtr, Checkpoint};
use crate::clvm::dialect::{ChiaDialect, Dialect};
use crate::clvm::sexp::SExp;
use crate::errors::ClvmError;
use log::debug;
use std::time::Instant;

const QUOTE_COST: u64 = 20;
const APPLY_COST: u64 = 90;
const OP_COST: u64 = 1;
const TRAVERSE_BASE_COST: u64 = 40;
const TRAVERSE_COST_PER_ZERO_BYTE: u64 = 4;
const TRAVERSE_COST_PER_BIT: u64 = 4;

#[repr(u8)]
enum Operation {
    Apply,
    Cons,
    Eval,
    SwapEval,
}

/// The CLVM evaluator over the compact node [`Arena`]: every eval intermediate lives in
/// the arena's typed pools as a u32 handle — `cons` is an 8-byte pair-pool push, a
/// path-traversal result is a handle copy, and an op result is allocated exactly once.
pub struct ClvmRuntime {
    dialect: ChiaDialect,
    arena: Arena,
    value_stack: Vec<NodePtr>,
    op_stack: Vec<Operation>,
    /// One entry per pending operator application, recorded before its operands are evaluated.
    /// Everything the operand evaluation allocates is garbage once the operator has produced a
    /// self-contained result, and this is the point it is rewound to.
    checkpoint_stack: Vec<Checkpoint>,
    max_cost: u64,
}

impl ClvmRuntime {
    #[must_use]
    pub fn new(max_cost: u64, flags: u32) -> Self {
        ClvmRuntime {
            dialect: ChiaDialect::new(flags),
            arena: Arena::new(),
            value_stack: vec![],
            op_stack: vec![],
            checkpoint_stack: vec![],
            max_cost,
        }
    }

    /// Allocation counters (atoms, pairs, heap bytes incl. ghost accounting) of the last
    /// run — memory-pressure diagnostics for the probes.
    #[must_use]
    pub fn arena_counters(&self) -> (usize, usize, usize) {
        self.arena.counters()
    }

    pub fn run(&mut self, program: &SExp, args: &SExp) -> Result<(u64, SExp<'static>), ClvmError> {
        let (cost, node) = self.run_in_arena(program, args)?;
        Ok((cost, self.arena.export(node)))
    }

    /// Evaluate `program` and leave the result in this runtime's arena, returning its
    /// [`NodePtr`] and the cost — WITHOUT `export`ing it to an owned `SExp` tree.
    ///
    /// [`Self::run`] exports the result, which is unsafe for an adversarial block generator
    /// whose output the caller streams: `Arena::export` deep-copies each atom by reference, so a
    /// `concat`/`substr` ladder emitting one shared ~268 MB integer `num` times is copied `num`
    /// times and OOM-kills the process. A caller that needs to bound a large output must walk it
    /// from [`Self::arena`] rather than `run`, charging condition cost incrementally and bailing
    /// at the first duplicate or at `MAX_BLOCK_COST_CLVM`.
    pub fn run_in_arena(
        &mut self,
        program: &SExp,
        args: &SExp,
    ) -> Result<(u64, NodePtr), ClvmError> {
        self.reset();
        let program = self.arena.import(program)?;
        let args = self.arena.import(args)?;
        self.value_stack.push(program);
        self.value_stack.push(args);
        self.op_stack.push(Operation::Eval);
        let max_cost = if self.max_cost == 0 {
            u64::MAX
        } else {
            self.max_cost
        };
        let mut current_cost: u64 = 0;
        let start = Instant::now();
        while let Some(op) = self.op_stack.pop() {
            current_cost += match op {
                Operation::Apply => self.apply_op(max_cost - current_cost)?,
                Operation::Cons => self.cons()?,
                Operation::Eval => self.eval_op()?,
                Operation::SwapEval => self.swap_eval_op()?,
            };
            if current_cost > max_cost {
                return Err(ClvmError::CostExceeded(current_cost, self.max_cost));
            }
        }
        let duration = start.elapsed();
        debug!("Program duration: {duration:?}");
        let return_value = self.value_stack.pop().ok_or(ClvmError::ValueStackEmpty)?;
        Ok((current_cost, return_value))
    }

    /// Borrow the arena holding the last [`Self::run_in_arena`] result, so a caller can walk
    /// a large/deep output iteratively (bounded, streaming) instead of exporting it.
    #[must_use]
    pub fn arena(&self) -> &Arena {
        &self.arena
    }

    fn reset(&mut self) {
        self.value_stack.clear();
        self.op_stack.clear();
        self.checkpoint_stack.clear();
        self.arena.reset();
    }

    fn cons(&mut self) -> Result<u64, ClvmError> {
        let first = self.value_stack.pop().ok_or(ClvmError::ValueStackEmpty)?;
        let rest = self.value_stack.pop().ok_or(ClvmError::ValueStackEmpty)?;
        let pair = self.arena.new_pair(first, rest)?;
        self.value_stack.push(pair);
        Ok(0)
    }

    fn traverse_path(
        arena: &Arena,
        node_index: &[u8],
        args: NodePtr,
    ) -> Result<(u64, NodePtr), ClvmError> {
        let mut arg_list = args;
        // find first non-zero byte
        let first_bit_byte_index = first_non_zero(node_index);
        let mut cost: u64 = TRAVERSE_BASE_COST
            + (first_bit_byte_index as u64) * TRAVERSE_COST_PER_ZERO_BYTE
            + TRAVERSE_COST_PER_BIT;
        if first_bit_byte_index >= node_index.len() {
            return Ok((cost, NodePtr::NIL));
        }
        // find first non-zero bit (the most significant bit is a sentinel)
        let last_bitmask = msb_mask(node_index[first_bit_byte_index]);
        // follow through the bits, moving left and right
        let mut byte_idx = node_index.len() - 1;
        let mut bitmask = 0x01;
        while byte_idx > first_bit_byte_index || bitmask < last_bitmask {
            let is_bit_set: bool = (node_index[byte_idx] & bitmask) != 0;
            let NodeKind::Pair(first, rest) = arena.node_kind(arg_list) else {
                return Err(ClvmError::ExpectedPairGotAtom(arena.display(arg_list)));
            };
            arg_list = if is_bit_set { rest } else { first };
            if bitmask == 0x80 {
                bitmask = 0x01;
                byte_idx -= 1;
            } else {
                bitmask <<= 1;
            }
            cost += TRAVERSE_COST_PER_BIT;
        }
        Ok((cost, arg_list))
    }

    fn eval_op_atom(
        &mut self,
        operator_node: NodePtr,
        operand_list: NodePtr,
        args: NodePtr,
    ) -> Result<u64, ClvmError> {
        let is_quote = {
            let op_atom = self
                .arena
                .atom(operator_node)
                .ok_or_else(|| ClvmError::ExpectedAtomGotPair(self.arena.display(operator_node)))?;
            op_atom.as_ref() == self.dialect.quote_kw()
        };
        if is_quote {
            self.value_stack.push(operand_list);
            Ok(QUOTE_COST)
        } else {
            self.op_stack.push(Operation::Apply);
            self.checkpoint_stack.push(self.arena.checkpoint());
            self.value_stack.push(operator_node);
            let mut operands = operand_list;
            loop {
                match self.arena.node_kind(operands) {
                    NodeKind::Atom => {
                        if self.arena.nullp(operands) {
                            break;
                        }
                        return Err(ClvmError::InvalidOperandList(self.arena.display(operands)));
                    }
                    NodeKind::Pair(first, rest) => {
                        self.op_stack.push(Operation::SwapEval);
                        self.value_stack.push(args);
                        self.value_stack.push(first);
                        operands = rest;
                    }
                }
            }
            self.value_stack.push(NodePtr::NIL);
            Ok(OP_COST)
        }
    }

    fn eval_pair(&mut self, program: NodePtr, args: NodePtr) -> Result<u64, ClvmError> {
        let (op_node, op_list) = match self.arena.node_kind(program) {
            NodeKind::Atom => {
                let r = {
                    let path = self.arena.atom(program).expect("node_kind atom has bytes");
                    Self::traverse_path(&self.arena, path.as_ref(), args)?
                };
                self.value_stack.push(r.1);
                return Ok(r.0);
            }
            NodeKind::Pair(first, rest) => (first, rest),
        };
        if let NodeKind::Pair(inner_first, inner_rest) = self.arena.node_kind(op_node) {
            if matches!(self.arena.node_kind(inner_first), NodeKind::Atom)
                && self.arena.nullp(inner_rest)
            {
                self.value_stack.push(inner_first);
                self.value_stack.push(op_list);
                self.op_stack.push(Operation::Apply);
                return Ok(APPLY_COST);
            }
            return Err(ClvmError::InvalidSyntax(format!(
                "in ((X)...) syntax X must be lone atom: {}",
                self.arena.debug_fmt(op_node)
            )));
        };
        self.eval_op_atom(op_node, op_list, args)
    }

    fn swap_eval_op(&mut self) -> Result<u64, ClvmError> {
        let v2_index = self.value_stack.pop().ok_or(ClvmError::ValueStackEmpty)?;
        let program = self.value_stack.pop().ok_or(ClvmError::ValueStackEmpty)?;
        let args = self.value_stack.pop().ok_or(ClvmError::ValueStackEmpty)?;
        self.value_stack.push(v2_index);
        // Cons must be queued before the operand eval so the accumulated operand list is
        // rebuilt in the correct order.
        self.op_stack.push(Operation::Cons);
        self.eval_pair(program, args)
    }

    fn eval_op(&mut self) -> Result<u64, ClvmError> {
        let args = self.value_stack.pop().ok_or(ClvmError::ValueStackEmpty)?;
        let program = self.value_stack.pop().ok_or(ClvmError::ValueStackEmpty)?;
        self.eval_pair(program, args)
    }

    fn apply_op(&mut self, max_cost: u64) -> Result<u64, ClvmError> {
        let operand_list = self.value_stack.pop().ok_or(ClvmError::ValueStackEmpty)?;
        let operator = self.value_stack.pop().ok_or(ClvmError::ValueStackEmpty)?;
        let is_apply = {
            let op_atom = self
                .arena
                .atom(operator)
                .ok_or_else(|| ClvmError::ExpectedAtomGotPair(self.arena.display(operator)))?;
            op_atom.as_ref() == self.dialect.apply_kw()
        };
        if is_apply {
            if self.arena.arg_count_is(operand_list, 2) {
                let (new_program, arg_wrap) = self.arena.next(operand_list).ok_or_else(|| {
                    ClvmError::ExpectedPairGotAtom(self.arena.display(operand_list))
                })?;
                let (new_args, _) = self
                    .arena
                    .next(arg_wrap)
                    .ok_or_else(|| ClvmError::ExpectedPairGotAtom(self.arena.display(arg_wrap)))?;
                self.eval_pair(new_program, new_args)
                    .map(|c| c + APPLY_COST)
            } else {
                Err(ClvmError::InvalidApplyArgs(
                    self.arena.display(operand_list),
                ))
            }
        } else {
            let (cost, out) = self.dialect.op(&self.arena, operator, operand_list, max_cost)?;
            // The operator has finished reading, and its description borrows nothing from the
            // arena when self-contained — so everything the operand evaluation allocated is now
            // unreachable and the pools can be rewound before the result is written. Anything
            // else may point into that region, and is left alone.
            if out.is_self_contained()
                && let Some(cp) = self.checkpoint_stack.pop()
            {
                self.arena.restore(cp);
            } else {
                self.checkpoint_stack.pop();
            }
            let (cost, result) = out.materialize(&mut self.arena, cost)?;
            self.value_stack.push(result);
            Ok(cost)
        }
    }
}

// return a bitmask with a single bit set, for the most significant set bit in
// the input byte
#[allow(clippy::cast_possible_truncation)]
fn msb_mask(byte: u8) -> u8 {
    let mut byte = u32::from(byte | (byte >> 1));
    byte |= byte >> 2;
    byte |= byte >> 4;
    debug_assert!((byte + 1) >> 1 <= 0x80);
    ((byte + 1) >> 1) as u8
}

// return the index of the first non-zero byte in buf. If all bytes are 0, the
// length (one past end) will be returned.
const fn first_non_zero(buf: &[u8]) -> usize {
    let mut c: usize = 0;
    while c < buf.len() && buf[c] == 0 {
        c += 1;
    }
    c
}

#[cfg(test)]
mod tests {
    //! Eval-loop, environment-traversal and cost-accounting tests. The traversal-cost
    //! constants are the canonical CLVM path-lookup costs (TRAVERSE_BASE_COST 40 +
    //! 4 per zero byte + 4 per bit).
    use super::*;
    use crate::clvm::program::{Program, SerializedProgram};
    use crate::clvm::sexp::SExp;
    use crate::clvm::utils::INFINITE_COST;
    use num_bigint::BigInt;

    // factorial
    const FACTORIAL_HEX: &str = "ff02ffff01ff02ff02ffff04ff02ffff04ff05ff80808080ffff04ffff01ff02\
ffff03ffff09ff05ffff010180ffff01ff0101ffff01ff12ff05ffff02ff02ff\
ff04ff02ffff04ffff11ff05ffff010180ff808080808080ff0180ff018080";

    fn run_path(path: SExp<'static>, args: SExp<'static>) -> (u64, SExp<'static>) {
        let mut runtime = ClvmRuntime::new(INFINITE_COST, 0);
        let program = Program::new(path);
        let arguments = Program::new(args);
        let (cost, out) = runtime.run(program.sexp(), arguments.sexp()).unwrap();
        (cost, out)
    }

    #[test]
    fn quote_returns_operand_at_quote_cost() {
        let program = Program::to((1_u8, 100_u8));
        let args = Program::default();
        let (cost, out) = program.run(INFINITE_COST, 0, &args).unwrap();
        assert_eq!(out.as_int().unwrap(), BigInt::from(100));
        // QUOTE_COST only
        assert_eq!(cost, 20);
    }

    #[test]
    fn path_1_returns_whole_environment() {
        let args = SExp::from(vec![SExp::from(10), SExp::from(20), SExp::from(30)]);
        let (cost, out) = run_path(SExp::from(1_u8), args.clone());
        assert_eq!(out, args);
        assert_eq!(cost, 44); // TRAVERSE_BASE_COST + one bit
    }

    #[test]
    fn path_2_returns_first_and_path_3_returns_rest() {
        let args = SExp::from(vec![SExp::from(10), SExp::from(20), SExp::from(30)]);
        let (cost2, first) = run_path(SExp::from(2_u8), args.clone());
        assert_eq!(first.atom().unwrap().as_int(), BigInt::from(10));
        assert_eq!(cost2, 48);
        let (cost3, rest) = run_path(SExp::from(3_u8), args.clone());
        assert_eq!(rest, SExp::from(vec![SExp::from(20), SExp::from(30)]));
        assert_eq!(cost3, 48);
    }

    #[test]
    fn path_5_returns_second_element() {
        let args = SExp::from(vec![SExp::from(10), SExp::from(20), SExp::from(30)]);
        let (cost, out) = run_path(SExp::from(5_u8), args);
        assert_eq!(out.atom().unwrap().as_int(), BigInt::from(20));
        assert_eq!(cost, 52);
    }

    #[test]
    fn cost_limit_is_enforced() {
        let program = Program::to((1_u8, 100_u8));
        let args = Program::default();
        // QUOTE_COST (20) exceeds a budget of 5.
        let err = program.run(5, 0, &args).unwrap_err();
        assert!(matches!(err, ClvmError::CostExceeded(_, _)), "got {err:?}");
    }

    // factorial(5) == 120
    #[test]
    fn factorial_of_five_is_120() {
        let serial = SerializedProgram::from_hex(FACTORIAL_HEX).unwrap();
        let program = serial.to_program().unwrap();
        // args "ff0580" == (5)
        let args_serial = SerializedProgram::from_hex("ff0580").unwrap();
        let args = args_serial.to_program().unwrap();
        let (_cost, out) = program.run(INFINITE_COST, 0, &args).unwrap();
        assert_eq!(out.as_int().unwrap(), BigInt::from(120));
    }
}
