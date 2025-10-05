use std::collections::HashMap;
use std::io::Error;
use std::sync::Arc;
use reqwest::Client;
use dg_xch_core::blockchain::block_record::BlockRecord;
use dg_xch_core::blockchain::blockchain_state::BlockchainState;
use dg_xch_core::blockchain::coin_record::{CoinRecord, HintedCoinRecord, PaginatedCoinRecord};
use dg_xch_core::blockchain::coin_spend::CoinSpend;
use dg_xch_core::blockchain::full_block::FullBlock;
use dg_xch_core::blockchain::mempool_item::MempoolItem;
use dg_xch_core::blockchain::network_info::NetworkInfo;
use dg_xch_core::blockchain::signage_point_or_eos::SignagePointOrEOS;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::blockchain::spend_bundle::SpendBundle;
use dg_xch_core::blockchain::unfinished_header_block::UnfinishedHeaderBlock;
use dg_xch_core::protocols::full_node::{BlockCountMetrics, FeeEstimate};
use crate::api::responses::{ AdditionsAndRemovalsResp, BlockCountMetricsResp, BlockRecordAryResp, BlockRecordResp, BlockchainStateResp, CoinHintsResp, CoinRecordAryResp, CoinRecordResp, CoinSpendMapResp, CoinSpendResp, FeeEstimateResp, FullBlockAryResp, FullBlockResp, HintedAdditionsAndRemovalsResp, MempoolItemAryResp, MempoolItemResp, MempoolItemsResp, MempoolTXResp, NetSpaceResp, NetworkInfoResp, PaginatedCoinRecordAryResp, SignagePointOrEOSResp, SingletonByLauncherIdResp, UnfinishedBlockAryResp};
use crate::ClientSSLConfig;
use crate::rpc::{get_client, get_http_client, get_insecure_url, get_url, ChiaRpcError};
use crate::rpc::full_node::UrlFunction;
use crate::rpc::post;

#[derive(Clone)]
pub struct FullnodeClient {
    pub client: Client,
    pub secure: bool,
    pub host: String,
    pub port: u16,
    pub ssl_path: Option<ClientSSLConfig>,
    pub additional_headers: Option<HashMap<String, String>>,
    pub url_function: UrlFunction,
}

impl FullnodeClient {
    pub fn new(
        host: &str,
        port: u16,
        timeout: u64,
        ssl_path: Option<ClientSSLConfig>,
        additional_headers: &Option<HashMap<String, String>>,
    ) -> Result<Self, Error> {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        Ok(FullnodeClient {
            client: get_client(&ssl_path, timeout)?,
            secure: true,
            host: host.to_string(),
            port,
            ssl_path,
            additional_headers: additional_headers.clone(),
            url_function: Arc::new(get_url),
        })
    }
    pub fn new_simulator(host: &str, port: u16, timeout: u64) -> Result<Self, Error> {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        Ok(FullnodeClient {
            client: get_http_client(timeout)?,
            secure: false,
            host: host.to_string(),
            port,
            ssl_path: None,
            additional_headers: None,
            url_function: Arc::new(get_insecure_url),
        })
    }
}

#[macro_export]
macro_rules! generate_rpc {
    (
        $Trait:ident,
        $Client:ident,
        [
            $(
                // Optionally: => oneof(...)  and/or  => map(<ident>) { ... }
                ($fn:ident, [ $( $( ? )? $arg:ident : $ty:ty ),* $(,)? ]
                    $( => oneof( $( $oneof:ident ),+ ) )?
                    $( => map ( $map_ident:ident ) $map:block )?
                , $Resp:ty, $Out:ty)
            ),* $(,)?
        ]
    ) => {
        #[::async_trait::async_trait]
        pub trait $Trait {
            $(
                async fn $fn(&self $(, $arg: $ty )*)
                    -> ::core::result::Result<$Out, ChiaRpcError>;
            )*
        }

        #[::async_trait::async_trait]
        impl $Trait for $Client {
            $(
                async fn $fn(&self $(, $arg: $ty )*)
                    -> ::core::result::Result<$Out, ChiaRpcError>
                {
                    let mut request_body: ::serde_json::Map<String, ::serde_json::Value> =
                        ::serde_json::Map::new();

                    $(
                        {
                            let __v = ::serde_json::to_value(&$arg)
                                .map_err(|e| ChiaRpcError { error: Some(e.to_string()), success: false })?;
                            if !__v.is_null() {
                                request_body.insert(stringify!($arg).to_string(), __v);
                            }
                        }
                    )*

                    $(
                        {
                            let mut __chosen: Option<&'static str> = None;
                            $(
                                if __chosen.is_none() && request_body.contains_key(stringify!($oneof)) {
                                    __chosen = Some(stringify!($oneof));
                                }
                            )*
                            $(
                                if Some(stringify!($oneof)) != __chosen {
                                    request_body.remove(stringify!($oneof));
                                }
                            )*
                            if __chosen.is_none() {
                                let __expected = [$( stringify!($oneof) ),*].join(", ");
                                return Err(ChiaRpcError {
                                    error: Some(format!("oneof: expected one of [{}]", __expected)),
                                    success: false,
                                });
                            }
                        }
                    )?

                    let __resp: $Resp = post::<$Resp, ::std::hash::RandomState>(
                        &self.client,
                        &(self.url_function)(self.host.as_str(), self.port, stringify!($fn)),
                        &request_body,
                        &self.additional_headers,
                    ).await?;

                    macro_rules! __genrpc_map_or_default {
                        ($resp:expr) => {{
                            $resp.result
                        }};
                        ($resp:expr, ($id:ident) $blk:block) => {{
                            let $id = $resp;
                            $blk
                        }};
                    }

                    let __out: $Out = __genrpc_map_or_default!(
                        __resp
                        $(, ($map_ident) $map)?
                    );

                    Ok(__out)
                }
            )*
        }
    };
}



generate_rpc!(
    FullnodeAPI,
    FullnodeClient,
    [
        (get_blockchain_state, [], BlockchainStateResp, BlockchainState),

        (get_block, [header_hash: &Bytes32], FullBlockResp, FullBlock),

        (get_blocks, [
            start: u32,
            end: u32,
            exclude_header_hash: bool,
            exclude_reorged: bool
        ], FullBlockAryResp, Vec<FullBlock>),

        (get_block_count_metrics, [], BlockCountMetricsResp, BlockCountMetrics),

        (get_block_record_by_height, [height: u32], BlockRecordResp, BlockRecord),

        (get_block_record, [header_hash: &Bytes32], BlockRecordResp, BlockRecord),

        (get_block_records, [start: u32, end: u32], BlockRecordAryResp, Vec<BlockRecord>),

        (get_unfinished_block_headers, [], UnfinishedBlockAryResp, Vec<UnfinishedHeaderBlock>),

        (get_network_space, [
            older_block_header_hash: &Bytes32,
            newer_block_header_hash: &Bytes32
        ], NetSpaceResp, u64),

        (get_additions_and_removals, [
            header_hash: &Bytes32
        ] => map(resp) {
            (resp.result.additions, resp.result.removals)
        }, AdditionsAndRemovalsResp, (Vec<CoinRecord>, Vec<CoinRecord>)),

        (get_network_info, []
            => map(resp) {
                NetworkInfo {
                    network_name: resp.result.network_name,
                    network_prefix: resp.result.network_prefix,
                }
            }
          , NetworkInfoResp, NetworkInfo),

        (get_recent_signage_point_or_eos, [
            ?sp_hash: Option<&Bytes32>,
            ?challenge_hash: Option<&Bytes32>
        ] => oneof(sp_hash, challenge_hash)
          => map(resp) {
                SignagePointOrEOS {
                    signage_point: resp.result.signage_point,
                    eos: resp.result.eos,
                    time_received: resp.result.time_received,
                    reverted: resp.result.reverted,
                }
          }
        , SignagePointOrEOSResp, SignagePointOrEOS),

        (get_coin_records_by_puzzle_hash, [
            puzzle_hash: &Bytes32,
            ?include_spent_coins: Option<bool>,
            ?start_height: Option<u32>,
            ?end_height: Option<u32>
        ], CoinRecordAryResp, Vec<CoinRecord>),

        (get_coin_records_by_puzzle_hashes, [
            puzzle_hashes: &[Bytes32],
            ?include_spent_coins: Option<bool>,
            ?start_height: Option<u32>,
            ?end_height: Option<u32>
        ], CoinRecordAryResp, Vec<CoinRecord>),

        (get_coin_record_by_name, [name: &Bytes32], CoinRecordResp, Option<CoinRecord>),

        (get_coin_records_by_names, [
            names: &[Bytes32],
            ?include_spent_coins: Option<bool>,
            ?start_height: Option<u32>,
            ?end_height: Option<u32>
        ], CoinRecordAryResp, Vec<CoinRecord>),

        (get_coin_records_by_parent_ids, [
            parent_ids: &[Bytes32],
            ?include_spent_coins: Option<bool>,
            ?start_height: Option<u32>,
            ?end_height: Option<u32>
        ], CoinRecordAryResp, Vec<CoinRecord>),

        (get_coin_records_by_hint, [
            hint: &Bytes32,
            ?include_spent_coins: Option<bool>,
            ?start_height: Option<u32>,
            ?end_height: Option<u32>
        ], CoinRecordAryResp, Vec<CoinRecord>),

        (get_puzzle_and_solution, [
            coin_id: &Bytes32,
            height: u32
        ], CoinSpendResp, CoinSpend),

        (get_all_mempool_tx_ids, [], MempoolTXResp, Vec<Bytes32>),

        (get_all_mempool_items, [], MempoolItemsResp, HashMap<Bytes32, MempoolItem>),

        (get_mempool_item_by_tx_id, [tx_id: &str], MempoolItemResp, MempoolItem),

        (get_mempool_items_by_coin_name, [coin_name: &Bytes32], MempoolItemAryResp, Vec<MempoolItem>),

        (get_fee_estimate, [
            ?cost: Option<u64>,
            ?spend_bundle: Option<SpendBundle>,
            ?spend_type: Option<String>,
            target_times: &[u64]
        ] => oneof(spend_bundle, spend_type, cost)
          , FeeEstimateResp, FeeEstimate)
    ]
);

generate_rpc!(
    FullnodeExtAPI,
    FullnodeClient,
    [
        (get_additions_and_removals_with_hints, [
            header_hash: &Bytes32
        ] => map(resp) {
            (resp.result.additions, resp.result.removals)
        }, HintedAdditionsAndRemovalsResp, (Vec<HintedCoinRecord>, Vec<HintedCoinRecord>)),

        (get_singleton_by_launcher_id, [
            launcher_id: &Bytes32
        ] => map(resp) {
            (resp.result.coin_record, resp.result.parent_spend)
        }, SingletonByLauncherIdResp, (CoinRecord, CoinSpend)),

        (get_coin_records_by_hints_paginated, [
            hints: &[Bytes32],
            ?include_spent_coins: Option<bool>,
            ?start_height: Option<u32>,
            ?end_height: Option<u32>,
            page_size: u32,
            ?last_id: Option<Bytes32>
        ] => map(resp) {
            (resp.result.coin_records, resp.result.last_id, resp.result.total_coin_count)
        }, PaginatedCoinRecordAryResp, (Vec<PaginatedCoinRecord>, Option<Bytes32>, Option<i32>)),

        (get_coin_records_by_puzzle_hashes_paginated, [
            puzzle_hashes: &[Bytes32],
            ?include_spent_coins: Option<bool>,
            ?start_height: Option<u32>,
            ?end_height: Option<u32>,
            page_size: u32,
            ?last_id: Option<Bytes32>
        ] => map(resp) {
            (resp.result.coin_records, resp.result.last_id, resp.result.total_coin_count)
        }, PaginatedCoinRecordAryResp, (Vec<PaginatedCoinRecord>, Option<Bytes32>, Option<i32>)),

        (get_coin_records_by_hints, [
            hints: &[Bytes32],
            ?include_spent_coins: Option<bool>,
            ?start_height: Option<u32>,
            ?end_height: Option<u32>
        ], CoinRecordAryResp, Vec<CoinRecord>),


        (get_hints_by_coin_ids, [
            coin_ids: &[Bytes32]
        ], CoinHintsResp, HashMap<Bytes32, Bytes32>),

        (get_puzzles_and_solutions_by_names, [
            names: &[Bytes32],
            ?include_spent_coins: Option<bool>,
            ?start_height: Option<u32>,
            ?end_height: Option<u32>
        ], CoinSpendMapResp, HashMap<Bytes32, Option<CoinSpend>>)
    ]
);

