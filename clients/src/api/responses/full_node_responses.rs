use dg_xch_core::blockchain::block_record::BlockRecord;
use dg_xch_core::blockchain::blockchain_state::BlockchainState;
use dg_xch_core::blockchain::coin_record::{CoinRecord, HintedCoinRecord, PaginatedCoinRecord};
use dg_xch_core::blockchain::coin_spend::CoinSpend;
use dg_xch_core::blockchain::full_block::FullBlock;
use dg_xch_core::blockchain::mempool_item::MempoolItem;
use dg_xch_core::blockchain::signage_point::SignagePoint;
use dg_xch_core::blockchain::subslot_bundle::SubSlotBundle;
use dg_xch_core::blockchain::tx_status::TXStatus;
use dg_xch_core::blockchain::unfinished_header_block::UnfinishedHeaderBlock;
use dg_xch_core::protocols::full_node::{BlockCountMetrics, FeeEstimate};

use dg_xch_core::blockchain::sized_bytes::Bytes32;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use dg_xch_core::blockchain::network_info::NetworkInfo;
use dg_xch_core::blockchain::signage_point_or_eos::SignagePointOrEOS;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AdditionsAndRemovalsResp {
    pub additions: Vec<CoinRecord>,
    pub removals: Vec<CoinRecord>,
    pub success: bool,
}
impl From<AdditionsAndRemovalsResp> for (Vec<CoinRecord>, Vec<CoinRecord>) {
    fn from(a: AdditionsAndRemovalsResp) -> Self {
        (a.additions, a.removals)
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HintedAdditionsAndRemovalsResp {
    //non-standard
    pub additions: Vec<HintedCoinRecord>,
    pub removals: Vec<HintedCoinRecord>,
    pub success: bool,
}
impl From<HintedAdditionsAndRemovalsResp> for (Vec<HintedCoinRecord>, Vec<HintedCoinRecord>) {
    fn from(a: HintedAdditionsAndRemovalsResp) -> Self {
        (a.additions, a.removals)
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SingletonByLauncherIdResp {
    pub coin_record: CoinRecord,
    pub parent_spend: CoinSpend,
    pub success: bool,
}
impl From<SingletonByLauncherIdResp> for (CoinRecord, CoinSpend) {
    fn from(a: SingletonByLauncherIdResp) -> Self {
        (a.coin_record, a.parent_spend)
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BlockchainStateResp {
    pub blockchain_state: BlockchainState,
    pub success: bool,
}
impl From<BlockchainStateResp> for BlockchainState {
    fn from(a: BlockchainStateResp) -> Self {
        a.blockchain_state
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BlockRecordResp {
    pub block_record: BlockRecord,
    pub success: bool,
}
impl From<BlockRecordResp> for BlockRecord {
    fn from(a: BlockRecordResp) -> Self {
        a.block_record
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BlockRecordAryResp {
    pub block_records: Vec<BlockRecord>,
    pub success: bool,
}

impl From<BlockRecordAryResp> for Vec<BlockRecord> {
    fn from(a: BlockRecordAryResp) -> Self {
        a.block_records
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CoinRecordResp {
    pub coin_record: Option<CoinRecord>,
    pub success: bool,
}
impl From<CoinRecordResp> for Option<CoinRecord> {
    fn from(a: CoinRecordResp) -> Self {
        a.coin_record
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CoinRecordAryResp {
    pub coin_records: Vec<CoinRecord>,
    pub success: bool,
}
impl From<CoinRecordAryResp> for Vec<CoinRecord> {
    fn from(a: CoinRecordAryResp) -> Self {
        a.coin_records
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CoinHintsResp {
    pub coin_id_hints: HashMap<Bytes32, Bytes32>,
    pub success: bool,
}

impl From<CoinHintsResp> for HashMap<Bytes32, Bytes32> {
    fn from(a: CoinHintsResp) -> Self {
        a.coin_id_hints
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PaginatedCoinRecordAryResp {
    pub coin_records: Vec<PaginatedCoinRecord>,
    pub last_id: Option<Bytes32>,
    pub total_coin_count: Option<i32>,
    pub success: bool,
}

impl From<PaginatedCoinRecordAryResp> for  (Vec<PaginatedCoinRecord>, Option<Bytes32>, Option<i32>) {
    fn from(a: PaginatedCoinRecordAryResp) -> Self {
        (a.coin_records, a.last_id, a.total_coin_count)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CoinSpendResp {
    pub coin_solution: CoinSpend,
    pub success: bool,
}
impl From<CoinSpendResp> for CoinSpend {
    fn from(a: CoinSpendResp) -> Self {
        a.coin_solution
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CoinSpendMapResp {
    pub coin_solutions: HashMap<Bytes32, Option<CoinSpend>>,
    pub success: bool,
}
impl From<CoinSpendMapResp> for HashMap<Bytes32, Option<CoinSpend>> {
    fn from(a: CoinSpendMapResp) -> Self {
        a.coin_solutions
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FullBlockResp {
    pub block: FullBlock,
    pub success: bool,
}
impl From<FullBlockResp> for FullBlock {
    fn from(a: FullBlockResp) -> Self {
        a.block
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockCountMetricsResp {
    pub metrics: BlockCountMetrics,
}
impl From<BlockCountMetricsResp> for BlockCountMetrics {
    fn from(a: BlockCountMetricsResp) -> Self {
        a.metrics
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FullBlockAryResp {
    pub blocks: Vec<FullBlock>,
    pub success: bool,
}
impl From<FullBlockAryResp> for Vec<FullBlock> {
    fn from(a: FullBlockAryResp) -> Self {
        a.blocks
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MempoolItemResp {
    pub mempool_item: MempoolItem,
    pub success: bool,
}
impl From<MempoolItemResp> for MempoolItem {
    fn from(a: MempoolItemResp) -> Self {
        a.mempool_item
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MempoolItemAryResp {
    pub mempool_items: Vec<MempoolItem>,
    pub success: bool,
}
impl From<MempoolItemAryResp> for Vec<MempoolItem> {
    fn from(a: MempoolItemAryResp) -> Self {
        a.mempool_items
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MempoolItemsResp {
    pub mempool_items: HashMap<Bytes32, MempoolItem>,
    pub success: bool,
}
impl From<MempoolItemsResp> for HashMap<Bytes32, MempoolItem> {
    fn from(a: MempoolItemsResp) -> Self {
        a.mempool_items
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MempoolTXResp {
    pub tx_ids: Vec<Bytes32>,
    pub success: bool,
}
impl From<MempoolTXResp> for Vec<Bytes32> {
    fn from(a: MempoolTXResp) -> Self {
        a.tx_ids
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FeeEstimateResp {
    pub fee_estimate: FeeEstimate,
    pub success: bool,
}
impl From<FeeEstimateResp> for FeeEstimate {
    fn from(a: FeeEstimateResp) -> Self {
        a.fee_estimate
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NetworkInfoResp {
    pub network_name: String,
    pub network_prefix: String,
    pub success: bool,
}
impl From<NetworkInfoResp> for NetworkInfo {
    fn from(a: NetworkInfoResp) -> Self {
        NetworkInfo {
            network_name: a.network_name,
            network_prefix: a.network_prefix,
        }
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NetSpaceResp {
    pub space: u64,
    pub success: bool,
}
impl From<NetSpaceResp> for u64 {
    fn from(a: NetSpaceResp) -> Self {
        a.space
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SignagePointOrEOSResp {
    pub signage_point: Option<SignagePoint>,
    pub eos: Option<SubSlotBundle>,
    pub time_received: f64,
    pub reverted: bool,
    pub success: bool,
}
impl From<SignagePointOrEOSResp> for SignagePointOrEOS {
    fn from(a: SignagePointOrEOSResp) -> Self {
        SignagePointOrEOS {
            signage_point: a.signage_point,
            eos: a.eos,
            time_received: a.time_received,
            reverted: a.reverted,
        }
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UnfinishedBlockAryResp {
    pub headers: Vec<UnfinishedHeaderBlock>,
    pub success: bool,
}
impl From<UnfinishedBlockAryResp> for Vec<UnfinishedHeaderBlock> {
    fn from(a: UnfinishedBlockAryResp) -> Self {
        a.headers
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TXResp {
    pub status: TXStatus,
    pub success: bool,
}
impl From<TXResp> for TXStatus {
    fn from(a: TXResp) -> Self {
        a.status
    }
}