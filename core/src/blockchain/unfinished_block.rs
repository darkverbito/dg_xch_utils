use crate::blockchain::foliage::Foliage;
use crate::blockchain::foliage_transaction_block::FoliageTransactionBlock;
use crate::blockchain::reward_chain_block_unfinished::RewardChainBlockUnfinished;
use crate::blockchain::sized_bytes::Bytes32;
use crate::blockchain::subslot_bundle::SubSlotBundle;
use crate::blockchain::transactions_info::TransactionsInfo;
use crate::blockchain::vdf_proof::VdfProof;
use crate::clvm::program::SerializedProgram;
use crate::utils::hash_256;
use dg_xch_macros::ChiaSerial;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use serde::{Deserialize, Serialize};
use std::io::Error;

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct UnfinishedBlock {
    pub finished_sub_slots: Vec<SubSlotBundle>,
    pub reward_chain_block: RewardChainBlockUnfinished,
    pub challenge_chain_sp_proof: Option<VdfProof>,
    pub reward_chain_sp_proof: Option<VdfProof>,
    pub foliage: Foliage,
    pub foliage_transaction_block: Option<FoliageTransactionBlock>,
    pub transactions_info: Option<TransactionsInfo>,
    pub transactions_generator: Option<SerializedProgram>,
    pub transactions_generator_ref_list: Vec<u32>,
}

impl UnfinishedBlock {
    /// chia `UnfinishedBlock.get_hash()`: sha256 over the streamable encoding of the WHOLE
    /// unfinished block — the exact-duplicate identity `full_node_store.seen_unfinished_block`
    /// dedups on (many foliages can share one reward-chain trunk, so the trunk hash alone is not
    /// it). As a blockchain (not network) type its encoding — hence this hash — is independent of
    /// the negotiated protocol version.
    ///
    /// # Errors
    /// Returns an error if the streamable encoding of the unfinished block fails.
    pub fn hash(&self) -> Result<Bytes32, Error> {
        Ok(Bytes32::from(hash_256(
            self.to_bytes(ChiaProtocolVersion::default())?,
        )))
    }
}
