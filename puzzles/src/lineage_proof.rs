use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::clvm::sexp::{IntoSExp, SExp};

pub struct LineageProof {
    pub parent_parent_id: Bytes32,
    pub parent_inner_puzzle_hash: Bytes32,
    pub parent_amount: u64,
}
impl IntoSExp for LineageProof {
    fn to_sexp(self) -> SExp {
        vec![
            self.parent_parent_id.to_sexp(),
            self.parent_inner_puzzle_hash.to_sexp(),
            self.parent_amount.to_sexp(),
        ]
            .to_sexp()
    }
}