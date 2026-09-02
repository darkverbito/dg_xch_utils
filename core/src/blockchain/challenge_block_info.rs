use crate::blockchain::proof_of_space::ProofOfSpace;
use crate::blockchain::sized_bytes::{Bytes32, Bytes96};
use crate::blockchain::vdf_info::VdfInfo;
use crate::utils::hash_256;
use dg_xch_macros::ChiaSerial;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use serde::{Deserialize, Serialize};
use std::io::Error;

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct ChallengeBlockInfo {
    pub proof_of_space: ProofOfSpace,
    pub challenge_chain_sp_vdf: Option<VdfInfo>,
    pub challenge_chain_sp_signature: Bytes96,
    pub challenge_chain_ip_vdf: VdfInfo,
}

impl ChallengeBlockInfo {
    pub fn hash(&self) -> Result<Bytes32, Error> {
        Ok(Bytes32::from(hash_256(
            self.to_bytes(ChiaProtocolVersion::default())?,
        )))
    }
}
