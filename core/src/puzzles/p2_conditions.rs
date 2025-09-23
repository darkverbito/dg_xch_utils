use crate::clvm::program::{Program, SerializedProgram};
use crate::clvm::sexp::IntoSExp;
use crate::clvm::utils::INFINITE_COST;
use std::io::Error;
use lazy_static::lazy_static;
use crate::blockchain::sized_bytes::Bytes32;

const P2_CONDITIONS_HEX: &str = "ff04ffff0101ff0280";
lazy_static! {
    pub static ref P2_CONDITIONS_MOD: Program =
        SerializedProgram::from_hex(P2_CONDITIONS_HEX)
            .unwrap()
            .to_program();
    pub static ref P2_CONDITIONS_MOD_HASH: Bytes32 = P2_CONDITIONS_MOD.tree_hash();
}
// parse_program_hex!(P2_CONDITIONS_HEX);

pub fn puzzle_for_conditions<T: IntoSExp>(conditions: T) -> Result<Program, Error> {
    let (_cost, result) = P2_CONDITIONS_MOD.run(INFINITE_COST, 0, &Program::to(vec![conditions]))?;
    Ok(result)
}

pub fn solution_for_conditions<T: IntoSExp>(conditions: T) -> Result<Program, Error> {
    Ok(Program::to(vec![
        puzzle_for_conditions(conditions)?.to_sexp(),
        0.to_sexp(),
    ]))
}
