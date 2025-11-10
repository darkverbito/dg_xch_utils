use dg_parser_macro::parse_program_hex;

parse_program_hex!(
    SINGLETON_LAUNCHER,
    "ff02ffff01ff04ffff04ff04ffff04ff05ffff04ff0bff80808080ffff04ffff04ff0affff04ffff02ff0effff04ff02ffff04ffff04ff05ffff04ff0bffff04ff17ff80808080ff80808080ff808080ff808080ffff04ffff01ff33ff3cff02ffff03ffff07ff0580ffff01ff0bffff0102ffff02ff0effff04ff02ffff04ff09ff80808080ffff02ff0effff04ff02ffff04ff0dff8080808080ffff01ff0bffff0101ff058080ff0180ff018080"
);

#[test]
pub fn test_hashes() {
    assert_eq!(
        dg_xch_core::blockchain::sized_bytes::Bytes32::const_hex(
            "eff07522495060c066f66f32acc2a77e3a3e737aca8baea4d1a64ea4cdc13da9"
        ),
        SINGLETON_LAUNCHER_TREE_HASH
    );
}
