use lazy_static::lazy_static;
use crate::blockchain::sized_bytes::Bytes32;
use crate::clvm::program::{Program, SerializedProgram};

const SETTLEMENT_PAYMENT_HEX: &str = "ff02ffff01ff02ff0affff04ff02ffff04ff03ff80808080ffff04ffff01ffff333effff02ffff03ff05ffff01ff04ffff04ff0cffff04ffff02ff1effff04ff02ffff04ff09ff80808080ff808080ffff02ff16ffff04ff02ffff04ff19ffff04ffff02ff0affff04ff02ffff04ff0dff80808080ff808080808080ff8080ff0180ffff02ffff03ff05ffff01ff02ffff03ffff15ff29ff8080ffff01ff04ffff04ff08ff0980ffff02ff16ffff04ff02ffff04ff0dffff04ff0bff808080808080ffff01ff088080ff0180ffff010b80ff0180ff02ffff03ffff07ff0580ffff01ff0bffff0102ffff02ff1effff04ff02ffff04ff09ff80808080ffff02ff1effff04ff02ffff04ff0dff8080808080ffff01ff0bffff0101ff058080ff0180ff018080";
lazy_static! {
    pub static ref SETTLEMENT_PAYMENT: Program =
        SerializedProgram::from_hex(SETTLEMENT_PAYMENT_HEX)
            .unwrap()
            .to_program();
    pub static ref SETTLEMENT_PAYMENT_HASH: Bytes32 = SETTLEMENT_PAYMENT.tree_hash();
}