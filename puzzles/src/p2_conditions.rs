use dg_parser_macro::parse_program_hex;
use dg_xch_core::clvm::program::Program;
use dg_xch_core::clvm::sexp::IntoSExp;
use dg_xch_core::clvm::utils::INFINITE_COST;
use std::io::Error;

parse_program_hex!(P2_CONDITIONS, "ff04ffff0101ff0280");

#[test]
pub fn test_hashes() {
    assert_eq!(
        dg_xch_core::blockchain::sized_bytes::Bytes32::const_hex(
            "1c77d7d5efde60a7a1d2d27db6d746bc8e568aea1ef8586ca967a0d60b83cc36"
        ),
        P2_CONDITIONS_TREE_HASH
    );
}

pub fn puzzle_for_conditions<T: IntoSExp>(conditions: T) -> Result<Program<'static>, Error> {
    let (_cost, result) =
        P2_CONDITIONS_PROGRAM.run(INFINITE_COST, 0, &Program::to(vec![conditions]))?;
    Ok(result)
}

pub fn solution_for_conditions<T: IntoSExp>(conditions: T) -> Result<Program<'static>, Error> {
    Ok(Program::to(vec![
        puzzle_for_conditions(conditions)?.to_sexp(),
        0.to_sexp(),
    ]))
}
