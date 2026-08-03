use crate::blockchain::sized_bytes::Bytes32;
use crate::blockchain::vdf_info::VdfInfo;
use crate::utils::hash_256;
use dg_xch_macros::ChiaSerial;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use serde::{Deserialize, Serialize};
use std::io::Error;

#[derive(ChiaSerial, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RewardChainSubSlot {
    pub end_of_slot_vdf: VdfInfo,
    pub challenge_chain_sub_slot_hash: Bytes32,
    pub infused_challenge_chain_sub_slot_hash: Option<Bytes32>,
    pub deficit: u8,
}

impl RewardChainSubSlot {
    /// chia `std_hash(bytes(rc_sub_slot))`. The consensus hash of this reward-chain sub-slot: sha256 over
    /// its streamable encoding. This is the value committed at a sub-slot boundary in a block record's
    /// `finished_reward_slot_hashes` and carried as a signage/infusion-point reward challenge. As a
    /// blockchain (not network) type its encoding — hence this hash — is independent of the negotiated
    /// protocol version.
    ///
    /// # Errors
    /// Returns an error if the streamable encoding of the sub-slot fails.
    pub fn hash(&self) -> Result<Bytes32, Error> {
        Ok(Bytes32::from(hash_256(
            self.to_bytes(ChiaProtocolVersion::default())?,
        )))
    }
}
