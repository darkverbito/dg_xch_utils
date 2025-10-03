use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::clvm::sexp::SExp;
use num_traits::ToPrimitive;
use std::io::{Error, ErrorKind};

pub struct LineageProof {
    pub parent_parent_id: Bytes32,
    pub parent_inner_puzzle_hash: Bytes32,
    pub parent_amount: u64,
}
impl From<LineageProof> for SExp {
    fn from(value: LineageProof) -> SExp {
        (&[
            SExp::from(value.parent_parent_id),
            SExp::from(value.parent_inner_puzzle_hash),
            SExp::from(value.parent_amount),
        ])
            .into()
    }
}
impl TryFrom<&SExp> for LineageProof {
    type Error = Error;
    fn try_from(sexp: &SExp) -> Result<Self, Self::Error> {
        let (parent_parent_id, rest) = sexp.split()?;
        let (parent_inner_puzzle_hash, rest) = rest.split()?;
        let (parent_amount, _) = rest.split()?;
        Ok(Self {
            parent_parent_id: Bytes32::try_from(parent_parent_id)?,
            parent_inner_puzzle_hash: Bytes32::try_from(parent_inner_puzzle_hash)?,
            parent_amount: parent_amount
                .as_int()?
                .to_u64()
                .ok_or(Error::new(ErrorKind::InvalidData, "Invalid prev_subtotal"))?,
        })
    }
}
