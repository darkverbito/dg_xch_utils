use lazy_static::lazy_static;
use crate::blockchain::sized_bytes::Bytes32;
use crate::clvm::program::{Program, SerializedProgram};

const SINGLETON_LAUNCHER_HEX: &str = "ff02ffff01ff04ffff04ff04ffff04ff05ffff04ff0bff80808080ffff04ffff04ff0affff04ffff02ff0effff04ff02ffff04ffff04ff05ffff04ff0bffff04ff17ff80808080ff80808080ff808080ff808080ffff04ffff01ff33ff3cff02ffff03ffff07ff0580ffff01ff0bffff0102ffff02ff0effff04ff02ffff04ff09ff80808080ffff02ff0effff04ff02ffff04ff0dff8080808080ffff01ff0bffff0101ff058080ff0180ff018080";
lazy_static! {
    pub static ref SINGLETON_LAUNCHER: Program =
        SerializedProgram::from_hex(SINGLETON_LAUNCHER_HEX)
            .unwrap()
            .to_program();
    pub static ref SINGLETON_LAUNCHER_HASH: Bytes32 = SINGLETON_LAUNCHER.tree_hash();
}