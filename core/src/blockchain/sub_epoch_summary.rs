use crate::blockchain::sized_bytes::Bytes32;
use crate::utils::hash_256;
use dg_xch_macros::ChiaSerial;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use serde::{Deserialize, Serialize};
use std::io::Error;

// Mainnet hashes the five fields below; future wire-format changes require an activation-height gate.
#[derive(ChiaSerial, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct SubEpochSummary {
    pub prev_subepoch_summary_hash: Bytes32,
    pub reward_chain_hash: Bytes32,
    pub num_blocks_overflow: u8,
    pub new_difficulty: Option<u64>,
    pub new_sub_slot_iters: Option<u64>,
}

impl SubEpochSummary {
    pub fn hash(&self) -> Result<Bytes32, Error> {
        Ok(Bytes32::from(hash_256(
            self.to_bytes(ChiaProtocolVersion::default())?,
        )))
    }
}
