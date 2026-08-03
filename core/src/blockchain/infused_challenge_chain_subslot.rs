use crate::blockchain::sized_bytes::Bytes32;
use crate::blockchain::vdf_info::VdfInfo;
use crate::utils::hash_256;
use dg_xch_macros::ChiaSerial;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use serde::{Deserialize, Serialize};
use std::io::Error;

#[derive(ChiaSerial, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct InfusedChallengeChainSubSlot {
    pub infused_challenge_chain_end_of_slot_vdf: VdfInfo,
}

impl InfusedChallengeChainSubSlot {
    /// chia `std_hash(bytes(icc_sub_slot))`. The consensus hash of this infused-challenge-chain sub-slot:
    /// sha256 over its streamable encoding. This is the value committed as the challenge/reward sub-slots'
    /// `infused_challenge_chain_sub_slot_hash` and in a block record's
    /// `finished_infused_challenge_slot_hashes`. As a blockchain (not network) type its encoding — hence
    /// this hash — is independent of the negotiated protocol version.
    ///
    /// # Errors
    /// Returns an error if the streamable encoding of the sub-slot fails.
    pub fn hash(&self) -> Result<Bytes32, Error> {
        Ok(Bytes32::from(hash_256(
            self.to_bytes(ChiaProtocolVersion::default())?,
        )))
    }
}
