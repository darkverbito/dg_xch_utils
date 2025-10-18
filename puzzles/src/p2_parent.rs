use dg_parser_macro::parse_program_hex;
use dg_xch_core::clvm::program::Program;
use dg_xch_core::clvm::sexp::SExp;
use dg_xch_core::clvm::utils::INFINITE_COST;
use std::io::Error;

parse_program_hex!(
    P2_PARENT,
    "ff02ffff01ff04ffff04ff08ffff04ffff02ff0affff04ff02ffff04ff0bffff04ffff02ff05ffff02ff0effff04ff02ffff04ff17ff8080808080ffff04ff2fff808080808080ff808080ffff02ff17ff5f8080ffff04ffff01ffff4720ffff02ffff03ffff22ffff09ffff0dff0580ff0c80ffff09ffff0dff0b80ff0c80ffff15ff17ffff0181ff8080ffff01ff0bff05ff0bff1780ffff01ff088080ff0180ff02ffff03ffff07ff0580ffff01ff0bffff0102ffff02ff0effff04ff02ffff04ff09ff80808080ffff02ff0effff04ff02ffff04ff0dff8080808080ffff01ff0bffff0101ff058080ff0180ff018080"
);

#[test]
pub fn test_hashes() {
    assert_eq!(
        dg_xch_core::blockchain::sized_bytes::Bytes32::const_hex(
            "b10ce2d0b18dcf8c21ddfaf55d9b9f0adcbf1e0beb55b1a8b9cad9bbff4e5f22"
        ),
        P2_PARENT_TREE_HASH
    );
}

pub fn puzzle_for_conditions<T: Into<SExp<'static>>>(
    conditions: T,
) -> Result<Program<'static>, Error> {
    let (_cost, result) =
        P2_PARENT_PROGRAM.run(INFINITE_COST, 0, &Program::to(&[conditions.into()]))?;
    Ok(result)
}

pub fn solution_for_conditions<T: Into<SExp<'static>>>(
    conditions: T,
) -> Result<Program<'static>, Error> {
    Ok(Program::to([puzzle_for_conditions(conditions)?.sexp(), &0.into()]).to_owned())
}
