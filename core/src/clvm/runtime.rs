use crate::clvm::dialect::{ChiaDialect, Dialect};
use crate::clvm::sexp::{PairBuf, SExp};
use crate::constants::NULL_SEXP;
use crate::errors::ClvmError;
use bumpalo::Bump;
use log::debug;
use std::time::Instant;

const QUOTE_COST: u64 = 20;
const APPLY_COST: u64 = 90;
const OP_COST: u64 = 1;
const TRAVERSE_BASE_COST: u64 = 40;
const TRAVERSE_COST_PER_ZERO_BYTE: u64 = 4;
const TRAVERSE_COST_PER_BIT: u64 = 4;

pub type PreEval = Box<dyn Fn(&SExp, &SExp) -> Result<Option<Box<PostEval>>, ClvmError>>;
pub type PostEval = dyn Fn(Option<&SExp>);

#[repr(u8)]
enum Operation {
    Apply,
    Cons,
    Eval,
    SwapEval,
    PostEval,
}

pub struct ClvmRuntime<'a> {
    dialect: ChiaDialect,
    arena: Bump,
    value_stack: Vec<&'a SExp<'a>>,
    op_stack: Vec<Operation>,
    pre_eval: Option<PreEval>,
    posteval_stack: Vec<Box<PostEval>>,
    max_cost: u64,
}
impl<'a> ClvmRuntime<'a> {
    pub fn new(max_cost: u64, flags: u32) -> Self {
        ClvmRuntime {
            dialect: ChiaDialect::new(flags),
            arena: Bump::new(),
            value_stack: vec![],
            op_stack: vec![],
            pre_eval: None,
            posteval_stack: vec![],
            max_cost,
        }
    }
    #[inline]
    fn alloc(arena: *const Bump, node: SExp<'a>) -> &'a SExp<'a> {
        // Bump::alloc returns &'s SExp<'a> tied to &self borrow 's.
        // We widen to &'a because `self.arena` (and self) live for 'a.
        unsafe {
            let r = (&*arena).alloc(node);
            &*(r as *const SExp<'a>)
        }
    }
    #[inline]
    fn alloc_pair(arena: *const Bump, first: &'a SExp<'a>, rest: &'a SExp<'a>) -> &'a SExp<'a> {
        // SAFETY NOTE BELOW
        // bumpalo::Bump::alloc returns a reference with lifetime tied to &self (call-site borrow).
        // We widen it to &'a because `self.arena` (and thus its allocations) live for 'a.
        // sound: `arena` (and runtime) live for 'a; nodes never freed earlier.
        unsafe {
            let r = (&*arena).alloc(SExp::Pair(PairBuf::Borrowed((first, rest))));
            &*(r as *const SExp<'a>)
        }
    }
    pub fn run(
        &mut self,
        program: &'a SExp<'a>,
        args: &'a SExp<'a>,
    ) -> Result<(u64, SExp<'_>), ClvmError> {
        self.reset();
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
        loop {
            let Some(op) = self.op_stack.pop() else { break };
            current_cost += match op {
                Operation::Apply => self.apply_op(max_cost - current_cost)?,
                Operation::Cons => self.cons()?,
                Operation::Eval => self.eval_op()?,
                Operation::SwapEval => self.swap_eval_op()?,
                Operation::PostEval => {
                    let f = self.posteval_stack.pop().ok_or(ClvmError::NoPostEval)?;
                    f(self.value_stack.last().copied());
                    0
                }
            };
            if current_cost > max_cost {
                return Err(ClvmError::CostExceeded(current_cost, self.max_cost));
            }
        }
        let duration = start.elapsed();
        debug!("Program duration: {duration:?}");
        let return_value = self.value_stack.pop().ok_or(ClvmError::ValueStackEmpty)?;
        Ok((current_cost, return_value.clone()))
    }
    fn reset(&mut self) {
        self.value_stack.clear();
        self.op_stack.clear();
        self.arena.reset();
    }
    fn cons(&mut self) -> Result<u64, ClvmError> {
        let first = self.value_stack.pop().ok_or(ClvmError::ValueStackEmpty)?;
        let rest = self.value_stack.pop().ok_or(ClvmError::ValueStackEmpty)?;
        let a = &self.arena as *const Bump;
        let pair = Self::alloc_pair(a, first, rest);
        self.value_stack.push(pair);
        Ok(0)
    }
    fn traverse_path(
        &mut self,
        node_index: &'_ [u8],
        args: &'a SExp<'a>,
    ) -> Result<(u64, &'a SExp<'a>), ClvmError> {
        let mut arg_list: &SExp = args;
        // find first non-zero byte
        let first_bit_byte_index = first_non_zero(node_index);
        let mut cost: u64 = TRAVERSE_BASE_COST
            + (first_bit_byte_index as u64) * TRAVERSE_COST_PER_ZERO_BYTE
            + TRAVERSE_COST_PER_BIT;
        if first_bit_byte_index >= node_index.len() {
            return Ok((cost, &NULL_SEXP));
        }
        // find first non-zero bit (the most significant bit is a sentinel)
        let last_bitmask = msb_mask(node_index[first_bit_byte_index]);
        // follow through the bits, moving left and right
        let mut byte_idx = node_index.len() - 1;
        let mut bitmask = 0x01;
        while byte_idx > first_bit_byte_index || bitmask < last_bitmask {
            let is_bit_set: bool = (node_index[byte_idx] & bitmask) != 0;
            let pair = arg_list.pair()?;
            arg_list = if is_bit_set {
                pair.rest()
            } else {
                pair.first()
            };
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
        operator_node: &'a SExp<'a>,
        operand_list: &'a SExp<'a>,
        args: &'a SExp<'a>,
    ) -> Result<u64, ClvmError> {
        let op_atom = operator_node.atom()?;
        if op_atom.as_ref() == self.dialect.quote_kw() {
            self.value_stack.push(operand_list);
            Ok(QUOTE_COST)
        } else {
            self.op_stack.push(Operation::Apply);
            self.value_stack.push(operator_node);
            let mut operands: &SExp = operand_list;
            loop {
                match operands {
                    SExp::Atom(buf) => {
                        if buf.as_ref().is_empty() {
                            break;
                        }
                        return Err(ClvmError::InvalidOperandList(operands.to_string()));
                    }
                    SExp::Pair(pair) => {
                        self.op_stack.push(Operation::SwapEval);
                        self.value_stack.push(args);
                        self.value_stack.push(pair.first());
                        operands = pair.rest();
                    }
                }
            }
            self.value_stack.push(&NULL_SEXP);
            Ok(OP_COST)
        }
    }
    fn eval_pair(&mut self, program: &'a SExp<'a>, args: &'a SExp<'a>) -> Result<u64, ClvmError> {
        let (op_node, op_list) = match program {
            SExp::Atom(path) => {
                let r = self.traverse_path(path.as_ref(), args)?;
                self.value_stack.push(r.1);
                return Ok(r.0);
            }
            SExp::Pair(pair) => (pair.first(), pair.rest()),
        };
        if let SExp::Pair(pair) = op_node {
            if let SExp::Atom(_) = pair.first()
                && pair.rest().nullp()
            {
                self.value_stack.push(pair.first());
                self.value_stack.push(op_list);
                self.op_stack.push(Operation::Apply);
                return Ok(APPLY_COST);
            }
            return Err(ClvmError::InvalidSyntax(format!(
                "in ((X)...) syntax X must be lone atom: {pair:?}"
            )));
        };
        self.eval_op_atom(op_node, op_list, args)
    }

    fn swap_eval_op(&mut self) -> Result<u64, ClvmError> {
        let v2_index = self.value_stack.pop().ok_or(ClvmError::ValueStackEmpty)?;
        let program = self.value_stack.pop().ok_or(ClvmError::ValueStackEmpty)?;
        let args = self.value_stack.pop().ok_or(ClvmError::ValueStackEmpty)?;
        self.value_stack.push(v2_index);
        let post_eval = match self.pre_eval {
            None => None,
            Some(ref pre_eval) => pre_eval(program, args)?,
        };
        if let Some(post_eval) = post_eval {
            self.posteval_stack.push(post_eval);
            self.op_stack.push(Operation::PostEval);
        };

        self.op_stack.push(Operation::Cons);
        self.eval_pair(program, args)
    }

    fn eval_op(&mut self) -> Result<u64, ClvmError> {
        let args = self.value_stack.pop().ok_or(ClvmError::ValueStackEmpty)?;
        let program = self.value_stack.pop().ok_or(ClvmError::ValueStackEmpty)?;
        let post_eval = match self.pre_eval {
            None => None,
            Some(ref pre_eval) => pre_eval(program, args)?,
        };
        if let Some(post_eval) = post_eval {
            self.posteval_stack.push(post_eval);
            self.op_stack.push(Operation::PostEval);
        };
        self.eval_pair(program, args)
    }

    fn apply_op(&mut self, max_cost: u64) -> Result<u64, ClvmError> {
        let operand_list = self.value_stack.pop().ok_or(ClvmError::ValueStackEmpty)?;
        let operator = self.value_stack.pop().ok_or(ClvmError::ValueStackEmpty)?;
        let op_atom = operator.atom()?;
        if op_atom.as_ref() == self.dialect.apply_kw() {
            if operand_list.arg_count_is(2) {
                let (new_program, arg_wrap) = operand_list.split()?;
                let (new_args, _) = arg_wrap.split()?;
                let post_eval = match self.pre_eval {
                    None => None,
                    Some(ref pre_eval) => pre_eval(new_program, new_args)?,
                };
                if let Some(post_eval) = post_eval {
                    self.posteval_stack.push(post_eval);
                    self.op_stack.push(Operation::PostEval);
                };
                self.eval_pair(new_program, new_args)
                    .map(|c| c + APPLY_COST)
            } else {
                Err(ClvmError::InvalidApplyArgs(operand_list.to_string()))
            }
        } else {
            let (cost, result) = self.dialect.op(operator, operand_list, max_cost)?;
            let a = &self.arena as *const Bump;
            let node = Self::alloc(a, result);
            self.value_stack.push(node);
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
