use crate::clvm::program::Program;
use crate::clvm::sexp::SExp;
use crate::clvm::sexp::{AtomBuf, IntoSExp};
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
    for arg in args.iter().map(IntoSExp::to_sexp).rev() {
        fixed_args = Program::to(vec![
            4.to_sexp(),
            (1.to_sexp(), arg).to_sexp(),
            fixed_args.to_sexp(),
        ]);
    }
    Program::to(vec![
        Program::to(2),
        Program::to((1.to_sexp(), program.sexp().to_owned())),
        fixed_args,
    ])
}
