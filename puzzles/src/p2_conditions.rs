use dg_xch_core::clvm::program::{Program};
use dg_xch_core::clvm::sexp::IntoSExp;
use dg_xch_core::clvm::utils::INFINITE_COST;
use std::io::Error;
use dg_parser_macro::parse_program_hex;

pub const P2_CONDITIONS_PROGRAM: Program = parse_program_hex!("ff04ffff0101ff0280");


pub fn puzzle_for_conditions<T: IntoSExp>(conditions: T) -> Result<Program, Error> {
    let (_cost, result) = P2_CONDITIONS_PROGRAM.run(INFINITE_COST, 0, &Program::to(vec![conditions]))?;
    Ok(result)
}

pub fn solution_for_conditions<T: IntoSExp>(conditions: T) -> Result<Program, Error> {
    Ok(Program::to(vec![
        puzzle_for_conditions(conditions)?.to_sexp(),
        0.to_sexp(),
    ]))
}
