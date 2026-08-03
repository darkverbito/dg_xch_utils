use crate::blockchain::proof_of_space::ProofOfSpace;
use crate::blockchain::reward_chain_block_unfinished::RewardChainBlockUnfinished;
use crate::blockchain::sized_bytes::{Bytes32, Bytes96};
use crate::blockchain::vdf_info::VdfInfo;
use crate::utils::hash_256;
use dg_xch_macros::ChiaSerial;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use serde::{Deserialize, Serialize};
use std::io::Error;

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RewardChainBlock {
    pub weight: u128,
    pub height: u32,
    pub total_iters: u128,
    pub signage_point_index: u8,
    pub pos_ss_cc_challenge_hash: Bytes32,
    pub proof_of_space: ProofOfSpace,
    pub challenge_chain_sp_vdf: Option<VdfInfo>,
    pub challenge_chain_sp_signature: Bytes96,
    pub challenge_chain_ip_vdf: VdfInfo,
    pub reward_chain_sp_vdf: Option<VdfInfo>,
    pub reward_chain_sp_signature: Bytes96,
    pub reward_chain_ip_vdf: VdfInfo,
    pub infused_challenge_chain_ip_vdf: Option<VdfInfo>,
    pub is_transaction_block: bool,
}

impl RewardChainBlock {
    /// chia `std_hash(bytes(reward_chain_block))`. The consensus hash of this reward-chain block: sha256
    /// over its streamable encoding. This is the value a block record carries as
    /// `reward_infusion_new_challenge` and the foliage commits as `reward_block_hash`. As a blockchain
    /// (not network) type its encoding — hence this hash — is independent of the negotiated protocol
    /// version.
    ///
    /// # Errors
    /// Returns an error if the streamable encoding of the reward-chain block fails.
    pub fn hash(&self) -> Result<Bytes32, Error> {
        Ok(Bytes32::from(hash_256(
            self.to_bytes(ChiaProtocolVersion::default())?,
        )))
    }

    /// chia `reward_chain_block.get_unfinished()`. The [`RewardChainBlockUnfinished`] view of this block:
    /// the signage-point-and-earlier fields, dropping the infusion-point VDFs and the transaction flag.
    /// Header validation hashes it to check the foliage's `unfinished_reward_block_hash`.
    #[must_use]
    pub fn get_unfinished(&self) -> RewardChainBlockUnfinished {
        RewardChainBlockUnfinished {
            total_iters: self.total_iters,
            signage_point_index: self.signage_point_index,
            pos_ss_cc_challenge_hash: self.pos_ss_cc_challenge_hash,
            proof_of_space: self.proof_of_space.clone(),
            challenge_chain_sp_vdf: self.challenge_chain_sp_vdf,
            challenge_chain_sp_signature: self.challenge_chain_sp_signature,
            reward_chain_sp_vdf: self.reward_chain_sp_vdf,
            reward_chain_sp_signature: self.reward_chain_sp_signature,
        }
    }
}
