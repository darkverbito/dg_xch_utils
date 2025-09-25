use dg_parser_macro::parse_program_hex;

parse_program_hex!(SETTLEMENT_PAYMENTS_V1, "ff02ffff01ff02ff0affff04ff02ffff04ff03ff80808080ffff04ffff01ffff333effff02ffff03ff05ffff01ff04ffff04ff0cffff04ffff02ff1effff04ff02ffff04ff09ff80808080ff808080ffff02ff16ffff04ff02ffff04ff19ffff04ffff02ff0affff04ff02ffff04ff0dff80808080ff808080808080ff8080ff0180ffff02ffff03ff05ffff01ff04ffff04ff08ff0980ffff02ff16ffff04ff02ffff04ff0dffff04ff0bff808080808080ffff010b80ff0180ff02ffff03ffff07ff0580ffff01ff0bffff0102ffff02ff1effff04ff02ffff04ff09ff80808080ffff02ff1effff04ff02ffff04ff0dff8080808080ffff01ff0bffff0101ff058080ff0180ff018080");

#[test]
pub fn test_hashes() {
    assert_eq!(
        dg_xch_core::blockchain::sized_bytes::Bytes32::const_hex(
            "bae24162efbd568f89bc7a340798a6118df0189eb9e3f8697bcea27af99f8f79"
        ),
        SETTLEMENT_PAYMENTS_V1_TREE_HASH
    );
}
