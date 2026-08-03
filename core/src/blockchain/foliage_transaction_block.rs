use crate::blockchain::sized_bytes::Bytes32;
use crate::utils::hash_256;
use dg_xch_macros::ChiaSerial;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use serde::{Deserialize, Serialize};
use std::io::Error;

#[derive(ChiaSerial, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct FoliageTransactionBlock {
    pub prev_transaction_block_hash: Bytes32,
    pub timestamp: u64,
    pub filter_hash: Bytes32,
    pub additions_root: Bytes32,
    pub removals_root: Bytes32,
    pub transactions_info_hash: Bytes32,
}

impl FoliageTransactionBlock {
    /// chia `std_hash(bytes(foliage_transaction_block))`. The consensus hash of this foliage transaction
    /// block: sha256 over its streamable encoding. This is the value the foliage commits as
    /// `foliage_transaction_block_hash` and the farmer signs during header validation. As a blockchain
    /// (not network) type its encoding — hence this hash — is independent of the negotiated protocol
    /// version.
    ///
    /// # Errors
    /// Returns an error if the streamable encoding of the foliage transaction block fails.
    pub fn hash(&self) -> Result<Bytes32, Error> {
        Ok(Bytes32::from(hash_256(
            self.to_bytes(ChiaProtocolVersion::default())?,
        )))
    }
}
