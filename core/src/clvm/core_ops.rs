use crate::clvm::arena::{Arena, NodeKind, NodePtr};
use crate::clvm::pure_ops::OpOut;
use crate::clvm::dialect::Dialect;
use crate::clvm::utils::{atom, check_arg_count, split};
use crate::errors::ClvmError;

const FIRST_COST: u64 = 30;
const IF_COST: u64 = 33;
// Cons cost lowered from 245. It only allocates a pair, which is small
const CONS_COST: u64 = 50;
// Rest cost lowered from 77 since it doesn't allocate anything and it should be
// the same as first
const REST_COST: u64 = 30;
const LISTP_COST: u64 = 19;
const EQ_BASE_COST: u64 = 117;
const EQ_COST_PER_BYTE: u64 = 1;

pub fn op_if<D: Dialect>(
    arena: &Arena,
    args: NodePtr,
    _max_cost: u64,
    _dialect: &D,
) -> Result<(u64, OpOut), ClvmError> {
    check_arg_count(arena, args, 3, "i")?;
    let (cond, mut chosen_node) = split(arena, args)?;
    if arena.nullp(cond) {
        chosen_node = split(arena, chosen_node)?.1;
    }
    Ok((IF_COST, OpOut::Same(split(arena, chosen_node)?.0)))
}

pub fn op_cons<D: Dialect>(
    arena: &Arena,
    args: NodePtr,
    _max_cost: u64,
    _dialect: &D,
) -> Result<(u64, OpOut), ClvmError> {
    check_arg_count(arena, args, 2, "c")?;
    let (first, rest) = split(arena, args)?;
    let second = split(arena, rest)?.0;
    Ok((CONS_COST, OpOut::Pair(first, second)))
}

pub fn op_first<D: Dialect>(
    arena: &Arena,
    args: NodePtr,
    _max_cost: u64,
    _dialect: &D,
) -> Result<(u64, OpOut), ClvmError> {
    check_arg_count(arena, args, 1, "f")?;
    let inner = split(arena, args)?.0;
    Ok((FIRST_COST, OpOut::Same(split(arena, inner)?.0)))
}

pub fn op_rest<D: Dialect>(
    arena: &Arena,
    args: NodePtr,
    _max_cost: u64,
    _dialect: &D,
) -> Result<(u64, OpOut), ClvmError> {
    check_arg_count(arena, args, 1, "r")?;
    let inner = split(arena, args)?.0;
    Ok((REST_COST, OpOut::Same(split(arena, inner)?.1)))
}

pub fn op_listp<D: Dialect>(
    arena: &Arena,
    args: NodePtr,
    _max_cost: u64,
    _dialect: &D,
) -> Result<(u64, OpOut), ClvmError> {
    check_arg_count(arena, args, 1, "l")?;
    let inner = split(arena, args)?.0;
    match arena.node_kind(inner) {
        NodeKind::Pair(_, _) => Ok((LISTP_COST, OpOut::Same(NodePtr::ONE))),
        NodeKind::Atom => Ok((LISTP_COST, OpOut::Same(NodePtr::NIL))),
    }
}

pub fn op_raise<D: Dialect>(
    arena: &Arena,
    args: NodePtr,
    _max_cost: u64,
    _dialect: &D,
) -> Result<(u64, OpOut), ClvmError> {
    match arena.node_kind(args) {
        NodeKind::Atom => Err(ClvmError::Raise(arena.debug_fmt(args))),
        NodeKind::Pair(first, rest) => {
            if arena.nullp(rest) {
                if arena.atom(first).is_none() {
                    return Err(ClvmError::ExpectedAtomGotPair(arena.display(first)));
                }
                Err(ClvmError::Raise(arena.debug_fmt(first)))
            } else {
                Err(ClvmError::Raise(arena.debug_fmt(rest)))
            }
        }
    }
}

pub fn op_eq<D: Dialect>(
    arena: &Arena,
    args: NodePtr,
    _max_cost: u64,
    _dialect: &D,
) -> Result<(u64, OpOut), ClvmError> {
    check_arg_count(arena, args, 2, "=")?;
    let (a0, rest) = split(arena, args)?;
    let a1 = split(arena, rest)?.0;
    let (equal, cost) = {
        let s0 = atom(arena, a0, "=")?;
        let s1 = atom(arena, a1, "=")?;
        (
            s0.as_ref() == s1.as_ref(),
            EQ_BASE_COST + (s0.len() as u64 + s1.len() as u64) * EQ_COST_PER_BYTE,
        )
    };
    Ok((cost, OpOut::Same(if equal { NodePtr::ONE } else { NodePtr::NIL })))
}

#[cfg(test)]
mod tests {
    //! Behavioral tests for the core CLVM operators. Operands are quoted so the
    //! eval loop pre-evaluates them to literals before the operator is applied.
    use crate::clvm::program::Program;
    use crate::clvm::sexp::SExp;
    use crate::clvm::utils::INFINITE_COST;
    use crate::errors::ClvmError;
    use num_bigint::BigInt;

    // Build and run `(op (q . a0) (q . a1) ...)` against a nil environment.
    fn run_op(op: u8, args: &[SExp<'static>]) -> Result<SExp<'static>, ClvmError> {
        let mut items = vec![SExp::from(op)];
        for a in args {
            items.push(SExp::from((1_u8, a.clone())));
        }
        let program = Program::new(SExp::from(items));
        program
            .run(INFINITE_COST, 0, &Program::default())
            .map(|(_c, out)| out.sexp().to_owned())
    }

    fn int(sexp: &SExp) -> BigInt {
        sexp.atom().unwrap().as_int()
    }

    #[test]
    fn op_if_selects_branch_by_truthiness() {
        // (i 1 20 30) -> 20 ; (i () 20 30) -> 30
        let t = run_op(3, &[SExp::from(1), SExp::from(20), SExp::from(30)]).unwrap();
        assert_eq!(int(&t), BigInt::from(20));
        let f = run_op(3, &[SExp::default(), SExp::from(20), SExp::from(30)]).unwrap();
        assert_eq!(int(&f), BigInt::from(30));
    }

    #[test]
    fn op_cons_builds_pair() {
        // (c 1 2) -> (1 . 2)
        let out = run_op(4, &[SExp::from(1), SExp::from(2)]).unwrap();
        assert_eq!(out, SExp::from((1_u8, 2_u8)));
    }

    #[test]
    fn op_first_and_rest() {
        let list = SExp::from(vec![SExp::from(10), SExp::from(20)]);
        let first = run_op(5, std::slice::from_ref(&list)).unwrap();
        assert_eq!(int(&first), BigInt::from(10));
        let rest = run_op(6, &[list]).unwrap();
        assert_eq!(rest, SExp::from(vec![SExp::from(20)]));
    }

    #[test]
    fn op_listp_distinguishes_pairs_from_atoms() {
        let is_pair = run_op(7, &[SExp::from(vec![SExp::from(1), SExp::from(2)])]).unwrap();
        assert_eq!(int(&is_pair), BigInt::from(1));
        let is_atom = run_op(7, &[SExp::from(5)]).unwrap();
        assert!(is_atom.nullp());
    }

    #[test]
    fn op_eq_compares_atoms() {
        let equal = run_op(9, &[SExp::from(5), SExp::from(5)]).unwrap();
        assert_eq!(int(&equal), BigInt::from(1));
        let unequal = run_op(9, &[SExp::from(5), SExp::from(6)]).unwrap();
        assert!(unequal.nullp());
    }

    #[test]
    fn op_raise_errors() {
        let err = run_op(8, &[SExp::from(1)]).unwrap_err();
        assert!(matches!(err, ClvmError::Raise(_)), "got {err:?}");
    }

    #[test]
    fn op_first_wrong_arg_count_errors() {
        // (f 1 2) — first takes exactly one argument.
        let err = run_op(5, &[SExp::from(1), SExp::from(2)]).unwrap_err();
        assert!(
            matches!(err, ClvmError::InvalidOperandArgs("f", 1)),
            "got {err:?}"
        );
    }

    #[test]
    fn op_first_on_atom_errors() {
        let err = run_op(5, &[SExp::from(5)]).unwrap_err();
        assert!(
            matches!(err, ClvmError::ExpectedPairGotAtom(_)),
            "got {err:?}"
        );
    }
}
