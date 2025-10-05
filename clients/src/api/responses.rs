use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use dg_xch_core::blockchain::{
    blockchain_state::BlockchainState,
    block_record::BlockRecord,
    coin_record::{CoinRecord, HintedCoinRecord, PaginatedCoinRecord},
    coin_spend::CoinSpend,
    full_block::FullBlock,
    mempool_item::MempoolItem,
    network_info::NetworkInfo,
    signage_point_or_eos::SignagePointOrEOS,
    sized_bytes::Bytes32,
    transaction_record::TransactionRecord,
    tx_status::TXStatus,
    unfinished_header_block::UnfinishedHeaderBlock,
    wallet_balance::WalletBalance,
    wallet_info::WalletInfo,
};
use dg_xch_core::blockchain::spend_bundle::SpendBundle;
use dg_xch_core::protocols::full_node::{BlockCountMetrics, FeeEstimate};

/* ------------------------------------------------------------------------------------------------
 * Shared “payload” structs (used by flattening responses)
 * ----------------------------------------------------------------------------------------------*/

#[derive(Debug, Clone)]
pub struct AdditionsAndRemovals {
    pub additions: Vec<CoinRecord>,
    pub removals: Vec<CoinRecord>,
}

impl serde::Serialize for AdditionsAndRemovals {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        // Force the JSON shape to be `[additions, removals]` to match the manual client
        (&self.additions, &self.removals).serialize(ser)
    }
}

impl<'de> serde::Deserialize<'de> for AdditionsAndRemovals {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Either {
            Obj { additions: Vec<CoinRecord>, removals: Vec<CoinRecord> },
            Tup((Vec<CoinRecord>, Vec<CoinRecord>)),
        }

        let parsed = <Either as serde::Deserialize>::deserialize(de)?;
        match parsed {
            Either::Obj { additions, removals } => Ok(Self { additions, removals }),
            Either::Tup((additions, removals)) => Ok(Self { additions, removals }),
        }
    }
}

#[derive(Debug, Clone)]
pub struct HintedAdditionsAndRemovals {
    pub additions: Vec<HintedCoinRecord>,
    pub removals: Vec<HintedCoinRecord>,
}

impl serde::Serialize for HintedAdditionsAndRemovals {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        (&self.additions, &self.removals).serialize(ser)
    }
}

impl<'de> serde::Deserialize<'de> for HintedAdditionsAndRemovals {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Either {
            Obj { additions: Vec<HintedCoinRecord>, removals: Vec<HintedCoinRecord> },
            Tup((Vec<HintedCoinRecord>, Vec<HintedCoinRecord>)),
        }

        let parsed = <Either as serde::Deserialize>::deserialize(de)?;
        match parsed {
            Either::Obj { additions, removals } => Ok(Self { additions, removals }),
            Either::Tup((additions, removals)) => Ok(Self { additions, removals }),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SingletonByLauncherId {
    pub coin_record: CoinRecord,
    pub parent_spend: CoinSpend,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PaginatedCoinRecordPage {
    pub coin_records: Vec<PaginatedCoinRecord>,
    pub last_id: Option<Bytes32>,
    pub total_coin_count: Option<i32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WalletSyncStatus {
    pub genesis_initialized: bool,
    pub synced: bool,
    pub syncing: bool,
}

/* ------------------------------------------------------------------------------------------------
 * Helper macros to reduce repetition for common “alias to result” envelopes
 * ----------------------------------------------------------------------------------------------*/

/// Single-key envelope -> `.result`, with tolerant `success`.
macro_rules! resp_alias {
    ($name:ident, $ty:ty, [$($alias:literal),+ $(,)?]) => {
        #[derive(Debug, Clone, Deserialize, Serialize)]
        pub struct $name {
            #[serde(alias = "result", $(alias = $alias,)+)]
            pub result: $ty,
            #[serde(default)]
            pub success: bool,
        }
    };
}

/// Flatten multiple top-level fields into `.result`, with tolerant `success`.
macro_rules! resp_flatten {
    ($name:ident, $ty:ty) => {
        #[derive(Debug, Clone, Deserialize, Serialize)]
        pub struct $name {
            #[serde(flatten)]
            pub result: $ty,
            #[serde(default)]
            pub success: bool,
        }
    };
}

/* ------------------------------------------------------------------------------------------------
 * FULLNODE API RESPONSES
 * ----------------------------------------------------------------------------------------------*/
// Aliased single-field envelopes
resp_alias!(BlockchainStateResp, BlockchainState, ["blockchain_state"]);
resp_alias!(FullBlockResp, FullBlock, ["block"]);
resp_alias!(FullBlockAryResp, Vec<FullBlock>, ["blocks"]);
resp_alias!(BlockRecordResp, BlockRecord, ["block_record"]);
resp_alias!(BlockRecordAryResp, Vec<BlockRecord>, ["block_records"]);
resp_alias!(UnfinishedBlockAryResp, Vec<UnfinishedHeaderBlock>, ["headers"]);
resp_alias!(NetSpaceResp, u64, ["space"]);
// Some nodes return `metrics` without `success`
resp_alias!(BlockCountMetricsResp, BlockCountMetrics, ["metrics"]);
// Mempool
resp_alias!(MempoolTXResp, Vec<Bytes32>, ["tx_ids", "mempool_tx_ids"]);
resp_alias!(MempoolItemResp, MempoolItem, ["mempool_item"]);
resp_alias!(MempoolItemAryResp, Vec<MempoolItem>, ["mempool_items"]);
resp_alias!(MempoolItemsResp, HashMap<Bytes32, MempoolItem>, ["mempool_items"]);
// Coins
resp_alias!(CoinRecordResp, Option<CoinRecord>, ["coin_record"]);
resp_alias!(CoinRecordAryResp, Vec<CoinRecord>, ["coin_records"]);
// Fee estimate
resp_alias!(FeeEstimateResp, FeeEstimate, ["fee_estimate", "estimate"]);
// Flattened multi-field envelopes
resp_flatten!(NetworkInfoResp, NetworkInfo);
resp_flatten!(AdditionsAndRemovalsResp, AdditionsAndRemovals);
resp_flatten!(SignagePointOrEOSResp, SignagePointOrEOS);

/* ------------------------------------------------------------------------------------------------
 * WALLET API RESPONSES
 * ----------------------------------------------------------------------------------------------*/
// Aliased single-field envelopes
resp_alias!(LoginResp, u32, ["fingerprint"]);
resp_alias!(WalletBalanceResp, Vec<WalletBalance>, ["wallets"]);
resp_alias!(WalletInfoResp, Vec<WalletInfo>, ["wallets"]);
resp_alias!(TransactionRecordResp, TransactionRecord, ["transaction"]);
resp_alias!(SignedTransactionRecordResp, TransactionRecord, ["signed_tx"]);
resp_alias!(TXResp, TXStatus, ["status"]);
resp_alias!(CoinHintsResp, HashMap<Bytes32, Bytes32>, ["coin_id_hints"]);
resp_alias!(CoinSpendResp, CoinSpend, ["coin_solution"]);
resp_alias!(CoinSpendMapResp, HashMap<Bytes32, Option<CoinSpend>>, ["coin_solutions"]);
resp_alias!(InitialFreezePeriodResp, u64, ["initial_freeze_end_timestamp"]);
resp_alias!(AutoFarmResp, bool, ["auto_farm_enabled"]);
resp_alias!(MempoolTXWalletResp, Vec<Bytes32>, ["tx_ids"]); // (if wallet returns tx_ids)
// Flattened multi-field envelopes
resp_flatten!(WalletSyncResp, WalletSyncStatus);
resp_flatten!(HintedAdditionsAndRemovalsResp, HintedAdditionsAndRemovals);
resp_flatten!(SingletonByLauncherIdResp, SingletonByLauncherId);
// Paginated / grouped pages
resp_alias!(PaginatedCoinRecordAryResp, PaginatedCoinRecordPage, ["page", "coin_records_page"]);

/* ------------------------------------------------------------------------------------------------
 * FARMER API RESPONSES
 * ----------------------------------------------------------------------------------------------*/

/* ------------------------------------------------------------------------------------------------
 * MISC / UTILITY ENVELOPES
 * ----------------------------------------------------------------------------------------------*/

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EmptyResponse {
    pub result: (),                     // serializes as null
    #[serde(default)]
    pub success: bool,
}
