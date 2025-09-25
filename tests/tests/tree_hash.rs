use dg_parser_macro::parse_program_hex;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::clvm::program::{Program, SerializedProgram};

pub const TEST_PROGRAM: Program = parse_program_hex!(
    "ff02ffff01ff04ffff04ff02ffff04ff05ffff04ff5fff80808080ff8080ffff04ffff0132ff018080"
);
pub const TEST_PROGRAM_HEX: &str =
    "ff02ffff01ff04ffff04ff02ffff04ff05ffff04ff5fff80808080ff8080ffff04ffff0132ff018080";
pub const TEST_PROGRAM_HASH: Bytes32 =
    Bytes32::const_hex("1720d13250a7c16988eaf530331cefa9dd57a76b2c82236bec8bbbff91499b89");
lazy_static::lazy_static! {
    pub static ref TEST_PROGRAM_STATIC: Program<'static> = Program::from_serial(
        &SerializedProgram::from_hex(TEST_PROGRAM_HEX).unwrap()
    ).unwrap();
}
#[test]
fn test_minimal_static_program() {
    let static_tree_hash = TEST_PROGRAM_STATIC.tree_hash();
    let program_tree_hash = TEST_PROGRAM.tree_hash();
    let known_tree_hash = TEST_PROGRAM_HASH;
    assert_eq!(static_tree_hash, known_tree_hash);
    assert_eq!(program_tree_hash, known_tree_hash);
}
