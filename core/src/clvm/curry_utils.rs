use crate::clvm::program::Program;
use crate::clvm::sexp::AtomBuf;
use crate::clvm::sexp::SExp;
use std::io::Error;
use std::io::ErrorKind;

pub fn concat(sexps: &[SExp]) -> Result<SExp, Error> {
    let mut buf = AtomBuf::new(vec![]);
    for sexp in sexps {
        match sexp {
            SExp::Atom(a) => {
                buf.extend(a);
            }
            SExp::Pair(_) => {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "(internal error) concat expected atom, got pair",
                ));
            }
        }
    }
    Ok(SExp::Atom(buf))
}

pub fn curry(program: &Program, args: &[Program<'_>]) -> Program<'static> {
    let mut fixed_args = Program::to(1);
    for arg in args.iter().map(Program::sexp).rev() {
        fixed_args = Program::to(&[
            4.into(),
            SExp::from(1).cons(arg.clone()),
            fixed_args.sexp().clone(),
        ]);
    }
    Program::to([
        Program::to(2).sexp(),
        Program::to((1.into(), program.sexp().to_owned())).sexp(),
        fixed_args.sexp(),
    ])
}
