use crate::blockchain::sized_bytes::Bytes32;
use crate::clvm::sexp::SExp;
use dg_xch_macros::ChiaSerial;
use serde::{Deserialize, Serialize};

pub type NpcCondition = (u8, Vec<(u8, String)>);
impl From<&NpcCondition> for SExp<'static> {
    fn from(value: &NpcCondition) -> Self {
        SExp::from((
            SExp::from(value.0),
            SExp::from(
                value
                    .1
                    .iter()
                    .map(|(k, v)| (SExp::from(k), SExp::from(v)).into())
                    .collect::<Vec<SExp<'_>>>(),
            ),
        ))
    }
}
impl From<&Vec<NpcCondition>> for SExp<'static> {
    fn from(value: &Vec<NpcCondition>) -> Self {
        value
            .iter()
            .map(SExp::from)
            .collect::<Vec<SExp<'_>>>()
            .into()
    }
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct NPC {
    pub coin_name: Bytes32,
    pub puzzle_hash: Bytes32,
    pub conditions: Vec<NpcCondition>,
}
