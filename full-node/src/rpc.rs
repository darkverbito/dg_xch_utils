use crate::config::RpcTlsMode;
use async_trait::async_trait;
use bytes::Bytes;
use dg_xch_core::blockchain::block_record::BlockRecord;
use dg_xch_core::blockchain::blockchain_state::{BlockchainState, MinMempoolFees};
use dg_xch_core::blockchain::coin_record::CoinRecord;
use dg_xch_core::blockchain::coin_spend::CoinSpend;
use dg_xch_core::blockchain::full_block::FullBlock;
use dg_xch_core::blockchain::mempool_item::MempoolItem as MempoolItemJson;
use dg_xch_core::blockchain::npc_result::NPCResult;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::blockchain::spend_bundle::SpendBundle;
use dg_xch_core::blockchain::sync::Sync as SyncStatus;
use dg_xch_core::blockchain::tx_status::TXStatus;
use dg_xch_core::blockchain::unfinished_header_block::UnfinishedHeaderBlock;
use dg_xch_core::blockchain::unsized_bytes::UnsizedBytes;
use dg_xch_core::consensus::block_generator::{
    BlockGeneratorFlags, BlockGeneratorInput, GeneratorReference, RawCondition,
    additions_for_conditions, coin_spend_from_generator, coin_spends_from_generator,
    coin_spends_with_conditions_from_generator, conditions_from_spend_bundle,
};
use dg_xch_core::consensus::constants::{ConsensusConstants, MAINNET};
use dg_xch_core::constants::CHIA_CA_CRT;
use dg_xch_core::protocols::PeerMap;
use dg_xch_core::protocols::full_node::NewTransaction;
use dg_xch_core::ssl::{
    generate_ca_signed_cert_data, load_certs_from_bytes, load_private_key_from_bytes, make_ca_cert,
    make_ca_cert_data,
};
use dg_xch_core::traits::SizedBytes;
use dg_xch_core::utils::hash_256;
use dg_xch_node::slots::SlotState;
use dg_xch_node::unfinished::UnfinishedCache;
use dg_xch_node::{Mempool, MempoolError};
use dg_xch_servers::rpc::{RequestType, RpcHandler, RpcRequest};
use dg_xch_stores::{BlockStore, CoinStore, StoreError};
use http::HeaderMap;
use http::header::CONTENT_TYPE;
use http::request::Parts;
use http_body_util::{BodyExt, Full};
use hyper::{Response, StatusCode};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::error::Error;
use std::fmt;
use std::io::Error as IoError;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;

// ---- bounds (every range is bounded LOUDLY — an over-cap request errors, it is never silently
// truncated) ------------------------------------------------------------------------------------

/// Most block records one `get_block_records` call may return — one full mainnet day (4608
/// blocks), the window the `get_blockchain_state` netspace math reads.
pub const MAX_BLOCK_RECORDS_PER_REQUEST: u32 = 4608;
/// Most FULL blocks (bodies included) one `get_blocks` call may return.
pub const MAX_BLOCKS_PER_REQUEST: u32 = 128;
/// Most ids (names / parent ids / puzzle hashes / hints) one coin query may carry.
pub const MAX_IDS_PER_REQUEST: usize = 32_690;
/// Request-body cap (1 MiB). An oversize body is refused with HTTP 413.
pub const MAX_RPC_BODY_BYTES: usize = 1024 * 1024;
// The netspace / average-block-time lookback for get_blockchain_state (one day).
const BLOCKCHAIN_STATE_LOOKBACK: u32 = 4608;
// Longest walk down from a height to find the nearest transaction block (tx blocks land every
// ~2 blocks; 128 is generous and keeps the walk bounded).
const MAX_TX_BLOCK_WALK: u32 = 128;
// UI_ACTUAL_SPACE_CONSTANT_FACTOR — the netspace estimate's plot-efficiency constant.
const UI_ACTUAL_SPACE_CONSTANT_FACTOR: f64 = 0.762;

#[derive(Debug)]
pub enum RpcError {
    Store(StoreError),
    Mempool(MempoolError),
    BadRequest(String),
    Corrupt(String),
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RpcError::Store(e) => write!(f, "store error: {e}"),
            RpcError::Mempool(e) => write!(f, "mempool rejected: {e}"),
            RpcError::BadRequest(s) => write!(f, "{s}"),
            RpcError::Corrupt(s) => write!(f, "inconsistent store: {s}"),
        }
    }
}

impl Error for RpcError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            RpcError::Store(e) => Some(e),
            RpcError::Mempool(e) => Some(e),
            RpcError::BadRequest(_) | RpcError::Corrupt(_) => None,
        }
    }
}

impl From<StoreError> for RpcError {
    fn from(e: StoreError) -> Self {
        RpcError::Store(e)
    }
}

impl From<MempoolError> for RpcError {
    fn from(e: MempoolError) -> Self {
        RpcError::Mempool(e)
    }
}

// ---- live-state attachment ---------------------------------------------------------------------

/// Daemon-owned live state the RPC serves beyond the store/mempool: the caches behind
/// `get_unfinished_block_headers` / `get_recent_signage_point_or_eos`, the inbound peer map
/// behind `get_connections`, and identity fields for `get_blockchain_state` /
/// `get_network_info`. Attached once by `spawn_rpc_server`; a `NodeRpc` without it (unit tests)
/// serves the store-backed endpoints and answers the live ones empty / not-in-cache.
pub struct NodeRpcLive {
    /// sha256 of our RPC leaf certificate — the cert-hash node identity.
    pub node_id: Bytes32,
    /// The `--network` id (`selected_network`).
    pub network_id: String,
    pub local_port: u16,
    /// The heaviest claimed peer peak — sync_tip_height.
    pub claimed_peak: Arc<AtomicU32>,
    /// Phase-2 slot state: recent signage points + finished sub-slots.
    pub slot_state: Arc<Mutex<SlotState>>,
    /// The unfinished-block cache.
    pub unfinished: Arc<Mutex<UnfinishedCache>>,
    /// The inbound peer sessions map.
    pub inbound_peers: PeerMap,
}

// ---- shared coin-query window ------------------------------------------------------------------

/// The coin-query window every `get_coin_records_by_*` endpoint takes:
/// `include_spent_coins` defaults FALSE, `start_height` / `end_height` filter on the confirmed
/// height (end EXCLUSIVE: `confirmed_index >= start AND confirmed_index < end`).
#[derive(Deserialize, Clone, Copy, Debug, Default)]
pub struct CoinQueryWindow {
    #[serde(default)]
    pub include_spent_coins: bool,
    #[serde(default)]
    pub start_height: Option<u32>,
    #[serde(default)]
    pub end_height: Option<u32>,
}

impl CoinQueryWindow {
    fn keep(&self, cr: &CoinRecord) -> bool {
        if !self.include_spent_coins && cr.spent {
            return false;
        }
        if let Some(start) = self.start_height
            && cr.confirmed_block_index < start
        {
            return false;
        }
        if let Some(end) = self.end_height
            && cr.confirmed_block_index >= end
        {
            return false;
        }
        true
    }

    fn apply(&self, records: Vec<CoinRecord>) -> Vec<CoinRecord> {
        records.into_iter().filter(|cr| self.keep(cr)).collect()
    }
}

/// `get_blockchain_state`'s full answer: the typed `blockchain_state` object plus the two
/// fields core's struct does not carry (`average_block_time`, `mempool_fees`) — the HTTP layer
/// merges them into the JSON object.
#[derive(Clone, Debug)]
pub struct BlockchainStateSummary {
    pub state: BlockchainState,
    pub average_block_time: Option<u64>,
    pub mempool_fees: u64,
}

// The node's read/write RPC surface. Holds Arc handles to the store (query-shaped) + mempool
// (push_tx target) + a synced flag the sync pipeline flips; every endpoint reads through the
// trait, never a backend.
pub struct NodeRpc<S> {
    store: Arc<S>,
    mempool: Arc<Mutex<Mempool>>,
    constants: ConsensusConstants,
    synced: Arc<AtomicBool>,
    // Accepted transactions queued for NewTransaction gossip — the driver drains this to the live
    // outbound peers, so a locally-pushed bundle propagates exactly like a peer-gossiped one.
    tx_announce: Arc<Mutex<Vec<NewTransaction>>>,
    // Daemon live-state, attached once by spawn_rpc_server (None in store-only unit tests).
    live: OnceLock<NodeRpcLive>,
    // Simulator block-production control, attached once when a simulator serves this RPC. `None` on a
    // production node, where the simulator endpoints answer 404.
    sim: OnceLock<Arc<dyn SimControl>>,
}

impl<S> NodeRpc<S>
where
    S: CoinStore + BlockStore + Send + Sync + 'static,
{
    #[must_use]
    pub fn new(
        store: Arc<S>,
        mempool: Arc<Mutex<Mempool>>,
        constants: ConsensusConstants,
        synced: Arc<AtomicBool>,
        tx_announce: Arc<Mutex<Vec<NewTransaction>>>,
    ) -> Self {
        Self {
            store,
            mempool,
            constants,
            synced,
            tx_announce,
            live: OnceLock::new(),
            sim: OnceLock::new(),
        }
    }

    /// Attach the daemon's live state (idempotent — the first attach wins).
    pub fn attach_live(&self, live: NodeRpcLive) {
        let _ = self.live.set(live);
    }

    /// Attach a simulator's block-production control, enabling the `farm_block` / `set_auto_farming`
    /// / `get_auto_farming` endpoints (idempotent — the first attach wins).
    pub fn attach_sim(&self, sim: Arc<dyn SimControl>) {
        let _ = self.sim.set(sim);
    }

    #[must_use]
    pub fn store(&self) -> &Arc<S> {
        &self.store
    }

    #[must_use]
    pub fn mempool(&self) -> &Arc<Mutex<Mempool>> {
        &self.mempool
    }

    /// The full `blockchain_state` object: peak as a full
    /// [`BlockRecord`], the `sync` sub-object, the netspace estimate over the last 4608 blocks,
    /// average block time, the mempool gauges, `block_max_cost`, and the cert-hash `node_id`.
    ///
    /// # Errors
    /// Returns [`RpcError::Store`] on a query failure or [`RpcError::Corrupt`] if the peak record
    /// is missing.
    #[allow(clippy::too_many_lines)]
    pub async fn get_blockchain_state(&self) -> Result<BlockchainStateSummary, RpcError> {
        let synced = self.synced.load(Ordering::Relaxed);
        let node_id = self
            .live
            .get()
            .map_or_else(|| Bytes32::from([0u8; 32]), |l| l.node_id);
        let peak = match self.store.get_peak().await? {
            Some((hh, _)) => Some(
                self.store
                    .get_block_record(&hh)
                    .await?
                    .ok_or_else(|| RpcError::Corrupt(format!("peak record {hh} missing")))?,
            ),
            None => None,
        };
        let (difficulty, sub_slot_iters) = match &peak {
            Some(rec) if rec.height > 0 => {
                let prev = self.store.get_block_record(&rec.prev_hash).await?;
                let difficulty = prev.map_or(rec.weight, |p| rec.weight.saturating_sub(p.weight));
                (
                    u64::try_from(difficulty).unwrap_or(u64::MAX),
                    rec.sub_slot_iters,
                )
            }
            _ => (
                self.constants.difficulty_starting,
                self.constants.sub_slot_iters_starting,
            ),
        };
        // Netspace + average block time over the last day of blocks. A node
        // without deep history (mid-chain anchored) reports 0/None rather than erroring.
        let (space, average_block_time) = match &peak {
            Some(rec) if rec.height > 1 => {
                let older_height = rec.height.saturating_sub(BLOCKCHAIN_STATE_LOOKBACK).max(1);
                let space = match self.store.get_block_record_by_height(older_height).await? {
                    Some(older) => self.network_space_between(&older, rec).unwrap_or(0),
                    None => 0,
                };
                (space, self.average_block_time(rec, older_height).await?)
            }
            _ => (0, None),
        };
        let (mempool_size, mempool_cost, mempool_fees, mempool_max_total_cost, min_fee_5m) = {
            let mp = self.mempool.lock().await;
            let items = mp.items_by_fee();
            let fees: u64 = items.iter().map(|i| i.fee).sum();
            (
                mp.len() as u64,
                mp.total_cost(),
                fees,
                mp.max_total_cost(),
                // get_min_fee_rate(5_000_000), served by the node's Mempool; null when nothing
                // could fit — flattened to 0 here
                // (MinMempoolFees carries a plain f64).
                mp.get_min_fee_rate(5_000_000).unwrap_or(0.0),
            )
        };
        let sync_tip_height = self
            .live
            .get()
            .map_or(0, |l| l.claimed_peak.load(Ordering::Relaxed));
        let peak_height = peak.as_ref().map_or(0, |p| p.height);
        let sync_mode = !synced;
        let sync = SyncStatus {
            sync_mode,
            synced,
            // While syncing toward an unknown tip, display peak/peak.
            sync_tip_height: if sync_mode && sync_tip_height == 0 {
                peak_height
            } else if sync_mode {
                sync_tip_height
            } else {
                0
            },
            sync_progress_height: if sync_mode { peak_height } else { 0 },
        };
        Ok(BlockchainStateSummary {
            state: BlockchainState {
                peak,
                genesis_challenge_initialized: true,
                sync,
                difficulty,
                sub_slot_iters,
                space,
                mempool_size,
                mempool_cost,
                mempool_min_fees: MinMempoolFees {
                    cost_5000000: min_fee_5m,
                },
                mempool_max_total_cost,
                block_max_cost: self.constants.max_block_cost_clvm,
                node_id,
            },
            average_block_time,
            mempool_fees,
        })
    }

    /// `get_fee_estimate`: fee-per-cost estimates for a spend of
    /// the given `cost` (or `spend_bundle`) to be confirmed within each `target_times` offset, plus
    /// the mempool gauges and last-block telemetry. Estimates are made monotonically DECREASING in
    /// target time (sooner ⇒ pricier). Insufficient tracker history yields the
    /// floor (0) — never a fabricated constant.
    ///
    /// # Errors
    /// [`RpcError::BadRequest`] if neither/both of `spend_bundle`/`cost` are supplied, or the
    /// bundle fails cost validation; [`RpcError::Store`] on a query failure.
    pub async fn get_fee_estimate(
        &self,
        spend_bundle: Option<SpendBundle>,
        cost: Option<u64>,
        mut target_times: Vec<u64>,
    ) -> Result<FeeEstimateResponse, RpcError> {
        // Exactly one of {spend_bundle, cost} (a spend_type option is not served here).
        let spend_cost = match (spend_bundle, cost) {
            (Some(_), Some(_)) | (None, None) => {
                return Err(RpcError::BadRequest(
                    "Request must contain exactly one of ['spend_bundle', 'cost']".to_string(),
                ));
            }
            (None, Some(c)) => c,
            (Some(bundle), None) => {
                // Cost from the conditions run. Height = next block (peak + 1).
                let height = match self.store.get_peak().await? {
                    Some((hh, _)) => {
                        self.store
                            .get_block_record(&hh)
                            .await?
                            .map_or(0, |r| r.height)
                            + 1
                    }
                    None => 0,
                };
                conditions_from_spend_bundle(&bundle, height, &self.constants)
                    .map_err(|e| RpcError::BadRequest(format!("invalid spend_bundle: {e:?}")))?
                    .cost
            }
        };
        // Sort target_times ascending, then make the products monotonically decreasing.
        target_times.sort_unstable();

        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let (estimates, current_fee_rate, mempool_size, mempool_fees, num_spends, mempool_max_size) = {
            let mp = self.mempool.lock().await;
            let est = mp.fee_estimator();
            let raw: Vec<f64> = target_times
                .iter()
                .map(|&t| est.estimate_fee_rate(t) * spend_cost as f64)
                .collect();
            let estimates: Vec<u64> = make_monotonically_decreasing(&raw)
                .into_iter()
                .map(|e| e as u64)
                .collect();
            // current_fee_rate = estimate_fee_rate(time_offset_seconds=1)
            let current_fee_rate = est.estimate_fee_rate(1);
            (
                estimates,
                current_fee_rate,
                mp.total_cost(),
                mp.total_fees(),
                mp.len() as u64,
                est.mempool_max_size(),
            )
        };

        let full_node_synced = self.synced.load(Ordering::Relaxed);
        let node_time_utc = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());

        // Peak + last-transaction-block telemetry (walk back to the last tx block).
        let peak = match self.store.get_peak().await? {
            Some((hh, _)) => self.store.get_block_record(&hh).await?,
            None => None,
        };
        let (
            peak_height,
            last_peak_timestamp,
            last_block_cost,
            fees_last_block,
            fee_rate_last_block,
            last_tx_block_height,
        ) = match &peak {
            None => (0u32, 0u64, 0u64, 0u64, 0.0f64, 0u32),
            Some(rec) => {
                // Walk to the most recent transaction block at/below the peak (a tx block carries a
                // timestamp).
                let mut cur = Some(rec.clone());
                let mut last_tx = None;
                while let Some(c) = cur {
                    if c.timestamp.is_some() {
                        last_tx = Some(c);
                        break;
                    }
                    cur = match c.prev_transaction_block_hash {
                        Some(prev) => self.store.get_block_record(&prev).await?,
                        None => None,
                    };
                }
                match last_tx {
                    None => (rec.height, 0, 0, 0, 0.0, 0),
                    Some(tx) => {
                        let ts = tx.timestamp.unwrap_or(0);
                        let fees = tx.fees.unwrap_or(0);
                        // last_block_cost + fee_rate need the full block's transactions_info.
                        #[allow(clippy::cast_precision_loss)]
                        let (block_cost, rate) = match self.store.get_block(&tx.header_hash).await?
                        {
                            Some(fb) => match fb.transactions_info {
                                Some(ti) if ti.cost > 0 => {
                                    (ti.cost, ti.fees as f64 / ti.cost as f64)
                                }
                                _ => (0, 0.0),
                            },
                            None => (0, 0.0),
                        };
                        (rec.height, ts, block_cost, fees, rate, tx.height)
                    }
                }
            }
        };

        Ok(FeeEstimateResponse {
            estimates,
            target_times,
            current_fee_rate,
            mempool_size,
            mempool_fees,
            num_spends,
            mempool_max_size,
            full_node_synced,
            peak_height,
            last_peak_timestamp,
            node_time_utc,
            last_block_cost,
            fees_last_block,
            fee_rate_last_block,
            last_tx_block_height,
        })
    }

    // get_average_block_time: seconds between the nearest
    // transaction blocks at/below the peak and at/below the lookback height.
    async fn average_block_time(
        &self,
        peak: &BlockRecord,
        older_height: u32,
    ) -> Result<Option<u64>, RpcError> {
        let Some(newer) = self.nearest_tx_block_at_or_below(peak.height).await? else {
            return Ok(None);
        };
        let Some(older) = self.nearest_tx_block_at_or_below(older_height).await? else {
            return Ok(None);
        };
        let (Some(ts_new), Some(ts_old)) = (newer.timestamp, older.timestamp) else {
            return Ok(None);
        };
        if newer.height <= older.height || ts_new <= ts_old {
            return Ok(None);
        }
        Ok(Some(
            (ts_new - ts_old) / u64::from(newer.height - older.height),
        ))
    }

    async fn nearest_tx_block_at_or_below(
        &self,
        height: u32,
    ) -> Result<Option<BlockRecord>, RpcError> {
        let stop = height.saturating_sub(MAX_TX_BLOCK_WALK);
        let mut h = height;
        loop {
            if let Some(rec) = self.store.get_block_record_by_height(h).await?
                && rec.is_transaction_block()
            {
                return Ok(Some(rec));
            }
            if h == 0 || h == stop {
                return Ok(None);
            }
            h -= 1;
        }
    }

    /// The full block for a header hash` — `get_block`: an
    /// unknown hash is an ERROR (`Block ... not found`), never null.
    ///
    /// # Errors
    /// Returns [`RpcError::BadRequest`] if the block is unknown, [`RpcError::Store`] on a
    /// query/decode failure.
    pub async fn get_block(&self, header_hash: &Bytes32) -> Result<FullBlock, RpcError> {
        self.store.get_block(header_hash).await?.ok_or_else(|| {
            RpcError::BadRequest(format!("Block {} not found", plain_hex(header_hash)))
        })
    }

    pub async fn get_blocks(
        &self,
        start: u32,
        end: u32,
    ) -> Result<Vec<(FullBlock, Bytes32)>, RpcError> {
        if end <= start {
            return Ok(Vec::new());
        }
        if end - start > MAX_BLOCKS_PER_REQUEST {
            return Err(RpcError::BadRequest(format!(
                "block range {start}..{end} exceeds the {MAX_BLOCKS_PER_REQUEST}-block cap"
            )));
        }
        let mut out = Vec::new();
        for h in start..end {
            let Some(rec) = self.store.get_block_record_by_height(h).await? else {
                continue;
            };
            if let Some(block) = self.store.get_block(&rec.header_hash).await? {
                out.push((block, rec.header_hash));
            }
        }
        Ok(out)
    }

    /// The block record for a header hash` — `get_block_record`
    ///: unknown hash is an error.
    ///
    /// # Errors
    /// Returns [`RpcError::BadRequest`] if the record is unknown, [`RpcError::Store`] on a query
    /// failure.
    pub async fn get_block_record(&self, header_hash: &Bytes32) -> Result<BlockRecord, RpcError> {
        self.store
            .get_block_record(header_hash)
            .await?
            .ok_or_else(|| {
                RpcError::BadRequest(format!("Block {} does not exist", plain_hex(header_hash)))
            })
    }

    /// The canonical block record at a height` — `get_block_record_by_height`
    ///: a height above the peak (or an empty chain) is an error.
    ///
    /// # Errors
    /// Returns [`RpcError::BadRequest`] if the height is above the peak or has no confirmed
    /// record; [`RpcError::Store`] on a query failure.
    pub async fn get_block_record_by_height(&self, height: u32) -> Result<BlockRecord, RpcError> {
        let peak_height = self.store.get_peak().await?.map(|(_, h)| h);
        if peak_height.is_none_or(|p| height > p) {
            return Err(RpcError::BadRequest(format!(
                "Block height {height} not found in chain"
            )));
        }
        self.store
            .get_block_record_by_height(height)
            .await?
            .ok_or_else(|| RpcError::BadRequest(format!("Block hash {height} not found in chain")))
    }

    /// Block records for the height range `start..end` (END-EXCLUSIVE) —
    /// `get_block_records`: heights above the peak end the walk
    /// (partial list, `break`); a height at/below the peak with no confirmed record is an
    /// ERROR (`Height not in blockchain`), never silently skipped.
    ///
    /// # Errors
    /// Returns [`RpcError::BadRequest`] if the chain has no peak, the range exceeds
    /// [`MAX_BLOCK_RECORDS_PER_REQUEST`], or a sub-peak height is missing; [`RpcError::Store`]
    /// on a query failure.
    pub async fn get_block_records(
        &self,
        start: u32,
        end: u32,
    ) -> Result<Vec<BlockRecord>, RpcError> {
        if end <= start {
            return Ok(Vec::new());
        }
        if end - start > MAX_BLOCK_RECORDS_PER_REQUEST {
            return Err(RpcError::BadRequest(format!(
                "block record range {start}..{end} exceeds the {MAX_BLOCK_RECORDS_PER_REQUEST}-record cap"
            )));
        }
        let Some((_, peak_height)) = self.store.get_peak().await? else {
            return Err(RpcError::BadRequest("Peak is None".to_string()));
        };
        let mut out = Vec::new();
        for h in start..end {
            if h > peak_height {
                break;
            }
            let rec = self
                .store
                .get_block_record_by_height(h)
                .await?
                .ok_or_else(|| RpcError::BadRequest(format!("Height not in blockchain: {h}")))?;
            out.push(rec);
        }
        Ok(out)
    }

    /// Every coin spend a block's generator produces` — `get_block_spends`. A non-transaction
    /// block answers an empty list.
    ///
    /// # Errors
    /// Returns [`RpcError::BadRequest`] if the block is unknown or its generator cannot be run
    /// (pre-hard-fork ROM generators do not surface reveals);
    /// [`RpcError::Store`] on a query failure; [`RpcError::Corrupt`] on a missing generator ref.
    pub async fn get_block_spends(
        &self,
        header_hash: &Bytes32,
    ) -> Result<Vec<CoinSpend>, RpcError> {
        let block = self.get_block(header_hash).await?;
        if block.transactions_generator.is_none() {
            return Ok(Vec::new());
        }
        let input = generator_input_for_block(self.store.as_ref(), &self.constants, &block).await?;
        // Off the async runtime: a full generator run is unbounded CLVM CPU (up to a whole
        // block's cost), so running it inline lets RPC load stall the event loop.
        tokio::task::spawn_blocking(move || coin_spends_from_generator(&input))
            .await
            .map_err(|e| RpcError::BadRequest(format!("spends worker panicked: {e:?}")))?
            .map_err(|e| RpcError::BadRequest(format!("Failed to get spends for block: {e:?}")))
    }

    /// [`NodeRpc::get_block_spends`] plus each spend's parsed conditions —
    /// `get_block_spends_with_conditions`.
    ///
    /// # Errors
    /// As [`NodeRpc::get_block_spends`].
    pub async fn get_block_spends_with_conditions(
        &self,
        header_hash: &Bytes32,
    ) -> Result<Vec<(CoinSpend, Vec<RawCondition>)>, RpcError> {
        let block = self.get_block(header_hash).await?;
        if block.transactions_generator.is_none() {
            return Ok(Vec::new());
        }
        let input = generator_input_for_block(self.store.as_ref(), &self.constants, &block).await?;
        // Off the async runtime — see get_block_spends.
        tokio::task::spawn_blocking(move || coin_spends_with_conditions_from_generator(&input))
            .await
            .map_err(|e| RpcError::BadRequest(format!("spends worker panicked: {e:?}")))?
            .map_err(|e| RpcError::BadRequest(format!("Failed to get spends for block: {e:?}")))
    }

    /// The netspace estimate between two block records` — `get_network_space`
    ///: `UI_ACTUAL_SPACE_CONSTANT_FACTOR * (Δweight / Δiters) *
    /// DIFFICULTY_CONSTANT_FACTOR * 2^plot_filter_prefix_bits`.
    ///
    /// # Errors
    /// Returns [`RpcError::BadRequest`] if the hashes are equal, a block is unknown, or the
    /// blocks carry no iteration delta; [`RpcError::Store`] on a query failure.
    pub async fn get_network_space(
        &self,
        newer_block_header_hash: &Bytes32,
        older_block_header_hash: &Bytes32,
    ) -> Result<u128, RpcError> {
        if newer_block_header_hash == older_block_header_hash {
            return Err(RpcError::BadRequest(
                "New and old must not be the same".to_string(),
            ));
        }
        let newer = self
            .store
            .get_block_record(newer_block_header_hash)
            .await?
            .ok_or_else(|| {
                RpcError::BadRequest(format!(
                    "Newer block {} not found",
                    plain_hex(newer_block_header_hash)
                ))
            })?;
        let older = self
            .store
            .get_block_record(older_block_header_hash)
            .await?
            .ok_or_else(|| {
                RpcError::BadRequest(format!(
                    "Older block {} not found",
                    plain_hex(older_block_header_hash)
                ))
            })?;
        self.network_space_between(&older, &newer)
            .ok_or_else(|| RpcError::BadRequest("blocks carry no iteration delta".to_string()))
    }

    // The netspace formula; float math matches the wire convention.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn network_space_between(&self, older: &BlockRecord, newer: &BlockRecord) -> Option<u128> {
        let delta_weight = newer.weight.checked_sub(older.weight)?;
        let delta_iters = newer.total_iters.checked_sub(older.total_iters)?;
        if delta_iters == 0 {
            return None;
        }
        let prefix_bits = plot_filter_prefix_bits(&self.constants, newer.height);
        let weight_div_iters = delta_weight as f64 / delta_iters as f64;
        let estimate = UI_ACTUAL_SPACE_CONSTANT_FACTOR
            * weight_div_iters
            * self.constants.difficulty_constant_factor as f64
            * 2f64.powi(i32::from(prefix_bits));
        Some(estimate as u128)
    }

    /// The cached unfinished blocks at the peak height, as header blocks —
    /// `get_unfinished_block_headers`.
    /// Answers empty without attached live state.
    ///
    /// # Errors
    /// Returns [`RpcError::Store`] on a peak query failure.
    pub async fn get_unfinished_block_headers(
        &self,
    ) -> Result<Vec<UnfinishedHeaderBlock>, RpcError> {
        let Some(live) = self.live.get() else {
            return Ok(Vec::new());
        };
        let Some((_, peak_height)) = self.store.get_peak().await? else {
            return Ok(Vec::new());
        };
        let cache = live.unfinished.lock().await;
        Ok(cache
            .blocks_at_height(peak_height)
            .into_iter()
            .map(|b| UnfinishedHeaderBlock {
                finished_sub_slots: b.finished_sub_slots.clone(),
                reward_chain_block: b.reward_chain_block.clone(),
                challenge_chain_sp_proof: b.challenge_chain_sp_proof.clone(),
                reward_chain_sp_proof: b.reward_chain_sp_proof.clone(),
                foliage: b.foliage,
                foliage_transaction_block: b.foliage_transaction_block,
                transactions_filter: UnsizedBytes::new(Vec::new()),
            })
            .collect())
    }

    /// A recently-received signage point (by `sp_hash`) or end-of-sub-slot (by
    /// `challenge_hash`)` — `get_recent_signage_point_or_eos`,
    /// served from the live slot state. `time_received` is not tracked by this cache and is
    /// reported as `0.0`; a point still resident in the slot state is `reverted: false`
    /// (the still-in-store fast path).
    ///
    /// # Errors
    /// Returns [`RpcError::BadRequest`] if neither parameter is given or the point is not in
    /// the cache (`SP_NOT_IN_CACHE` / `EOS_NOT_IN_CACHE`).
    pub async fn get_recent_signage_point_or_eos(
        &self,
        sp_hash: Option<&Bytes32>,
        challenge_hash: Option<&Bytes32>,
    ) -> Result<Map<String, Value>, RpcError> {
        let live = self.live.get();
        if let Some(sp_hash) = sp_hash {
            let sp = live
                .ok_or_else(|| sp_not_in_cache(sp_hash))?
                .slot_state
                .lock()
                .await
                .get_signage_point(sp_hash)
                .ok_or_else(|| sp_not_in_cache(sp_hash))?;
            let mut m = Map::new();
            m.insert("signage_point".to_string(), to_value(&sp)?);
            m.insert("time_received".to_string(), Value::from(0.0f64));
            m.insert("reverted".to_string(), Value::from(false));
            return Ok(m);
        }
        let Some(challenge_hash) = challenge_hash else {
            return Err(RpcError::BadRequest(
                "sp_hash or challenge_hash required".to_string(),
            ));
        };
        let eos = live
            .ok_or_else(|| eos_not_in_cache(challenge_hash))?
            .slot_state
            .lock()
            .await
            .get_sub_slot(challenge_hash)
            .map(|(eos, _, _)| eos.clone())
            .ok_or_else(|| eos_not_in_cache(challenge_hash))?;
        let mut m = Map::new();
        m.insert("eos".to_string(), to_value(&eos)?);
        m.insert("time_received".to_string(), Value::from(0.0f64));
        m.insert("reverted".to_string(), Value::from(false));
        Ok(m)
    }

    /// Coin records by coin id` — `get_coin_records_by_names`
    ///: unknown names are simply absent; `include_spent_coins`
    /// defaults FALSE; the height window filters on the confirmed height.
    ///
    /// # Errors
    /// Returns [`RpcError::BadRequest`] beyond [`MAX_IDS_PER_REQUEST`] names;
    /// [`RpcError::Store`] on a query failure.
    pub async fn get_coin_records_by_names(
        &self,
        names: &[Bytes32],
        window: CoinQueryWindow,
    ) -> Result<Vec<CoinRecord>, RpcError> {
        check_id_cap(names.len())?;
        Ok(window.apply(self.store.get_coin_records(names).await?))
    }

    /// A single coin record by id` — `get_coin_record_by_name`
    ///: unknown is an ERROR.
    ///
    /// # Errors
    /// Returns [`RpcError::BadRequest`] if the coin is unknown; [`RpcError::Store`] on a query
    /// failure.
    pub async fn get_coin_record_by_name(&self, name: &Bytes32) -> Result<CoinRecord, RpcError> {
        self.store
            .get_coin_record(name)
            .await?
            .ok_or_else(|| RpcError::BadRequest(format!("Coin record {name} not found")))
    }

    /// Coin records by parent coin id` — `get_coin_records_by_parent_ids`
    ///. Service tier (`coin-index`).
    ///
    /// # Errors
    /// Returns [`RpcError::BadRequest`] beyond [`MAX_IDS_PER_REQUEST`] parents;
    /// [`RpcError::Store`] on a query failure.
    #[cfg(feature = "coin-index")]
    pub async fn get_coin_records_by_parent_ids(
        &self,
        parent_ids: &[Bytes32],
        window: CoinQueryWindow,
    ) -> Result<Vec<CoinRecord>, RpcError> {
        check_id_cap(parent_ids.len())?;
        let mut out = Vec::new();
        for parent in parent_ids {
            out.extend(self.store.get_coins_by_parent(parent).await?);
        }
        Ok(window.apply(out))
    }

    /// Coin records for one puzzle hash` — `get_coin_records_by_puzzle_hash`
    ///. Service tier (`coin-index`).
    ///
    /// # Errors
    /// Returns [`RpcError::Store`] on a query failure.
    #[cfg(feature = "coin-index")]
    pub async fn get_coin_records_by_puzzle_hash(
        &self,
        puzzle_hash: &Bytes32,
        window: CoinQueryWindow,
    ) -> Result<Vec<CoinRecord>, RpcError> {
        self.get_coin_records_by_puzzle_hashes(std::slice::from_ref(puzzle_hash), window)
            .await
    }

    /// Coin records for a puzzle-hash list` — `get_coin_records_by_puzzle_hashes`
    ///. Unspent queries read the unspent secondary index directly;
    /// spent-inclusive queries resolve ids through the coin-state index first (bounded by the
    /// store's `MAX_COIN_STATES` budget). Service tier (`coin-index`).
    ///
    /// # Errors
    /// Returns [`RpcError::BadRequest`] beyond [`MAX_IDS_PER_REQUEST`] hashes;
    /// [`RpcError::Store`] on a query failure.
    #[cfg(feature = "coin-index")]
    pub async fn get_coin_records_by_puzzle_hashes(
        &self,
        puzzle_hashes: &[Bytes32],
        window: CoinQueryWindow,
    ) -> Result<Vec<CoinRecord>, RpcError> {
        check_id_cap(puzzle_hashes.len())?;
        let mut out = Vec::new();
        if window.include_spent_coins {
            let states = self
                .store
                .get_coin_states_by_puzzle_hashes(
                    puzzle_hashes,
                    0,
                    true,
                    dg_xch_stores::traits::MAX_COIN_STATES,
                )
                .await?;
            let names: Vec<Bytes32> = states.iter().map(|cs| cs.coin.name()).collect();
            for chunk in names.chunks(900) {
                out.extend(self.store.get_coin_records(chunk).await?);
            }
        } else {
            for ph in puzzle_hashes {
                out.extend(self.store.get_unspent_by_puzzle_hash(ph).await?);
            }
        }
        Ok(window.apply(out))
    }

    /// Admit a spend bundle to the mempool, returning its name. The bundle→conditions CLVM run
    /// (mempool mode, next-block height) and the aggregate-signature check both happen here,
    /// server-side — the caller supplies only the bundle, matching `push_tx`
    ///. Idempotent: a bundle already resident answers SUCCESS via the
    /// shared admission seam.
    ///
    /// # Errors
    /// Returns [`RpcError::BadRequest`] if the bundle fails CLVM or signature validation,
    /// [`RpcError::Mempool`] with the rejection reason, or [`RpcError::Store`] on a store failure.
    pub async fn push_tx(&self, bundle: SpendBundle) -> Result<Bytes32, RpcError> {
        // The shared admission seam (tx_admission.rs): the same
        // validate → admit → announce path the gossip worker and the wallet's p2p SendTransaction
        // run — one mempool admission seam, three ingress surfaces.
        crate::tx_admission::admit_spend_bundle(
            self.store.as_ref(),
            &self.mempool,
            &self.constants,
            &self.tx_announce,
            bundle,
        )
        .await
        .map_err(|e| match e {
            crate::tx_admission::TxAdmissionError::Validation(v) => {
                RpcError::BadRequest(format!("invalid spend bundle: {v:?}"))
            }
            crate::tx_admission::TxAdmissionError::Mempool(m) => RpcError::Mempool(m),
            crate::tx_admission::TxAdmissionError::Store(s) => RpcError::Store(s),
            crate::tx_admission::TxAdmissionError::Corrupt(c) => RpcError::Corrupt(c),
        })
    }

    /// Coin records a 32-byte hint points at` — `get_coin_records_by_hint`
    ///: unspent-only by default, height-windowed. Resolves through
    /// the `coin_hint` index populated on block apply. Requires the `hint` service tier.
    ///
    /// # Errors
    /// Returns [`RpcError::Store`] on a query failure.
    #[cfg(feature = "hint")]
    pub async fn get_coin_records_by_hint(
        &self,
        hint: &Bytes32,
        window: CoinQueryWindow,
    ) -> Result<Vec<CoinRecord>, RpcError> {
        let records = self
            .store
            .get_coin_records_by_hint(hint, window.include_spent_coins)
            .await?;
        Ok(window.apply(records))
    }

    /// Additions (coins created) and removals (coins spent) at a block —
    /// `get_additions_and_removals`. Rejects a header
    /// hash that is not the confirmed block at its height (`height_to_hash(height) ==
    /// header_hash` fork check). Requires the `coin-index` service tier (reads the
    /// `confirmed_index` / `spent_index` secondary indexes).
    ///
    /// # Errors
    /// Returns [`RpcError::BadRequest`] if the block is unknown or in a fork, or
    /// [`RpcError::Store`] on a query failure.
    #[cfg(feature = "coin-index")]
    pub async fn get_additions_and_removals(
        &self,
        header_hash: &Bytes32,
    ) -> Result<AdditionsAndRemovals, RpcError> {
        let record = self
            .store
            .get_block_record(header_hash)
            .await?
            .ok_or_else(|| {
                RpcError::BadRequest(format!("Block {} not found", plain_hex(header_hash)))
            })?;
        // Main-chain check: the confirmed block at this height must be exactly this header hash.
        let confirmed = self.store.get_block_record_by_height(record.height).await?;
        if confirmed.map(|r| r.header_hash) != Some(*header_hash) {
            return Err(RpcError::BadRequest(format!(
                "Block at {} is no longer in the blockchain (it's in a fork)",
                plain_hex(header_hash)
            )));
        }
        let additions = self.store.get_coins_added_at_height(record.height).await?;
        let removals = self
            .store
            .get_coins_removed_at_height(record.height)
            .await?;
        Ok(AdditionsAndRemovals {
            additions,
            removals,
        })
    }

    /// The [`CoinSpend`] (coin + puzzle reveal + solution) of a spent coin —
    /// `get_puzzle_and_solution`. The coin must be
    /// spent at exactly `height`; the block's generator is then re-run through the existing CLVM
    /// runner ([`coin_spend_from_generator`]) to recover the reveal and solution — no second VM.
    ///
    /// # Errors
    /// Returns [`RpcError::BadRequest`] if the coin is unknown, not spent at `height`, or its
    /// block or generator is missing; [`RpcError::Store`] on a query failure;
    /// [`RpcError::Corrupt`] if a confirmed generator back-reference is absent.
    pub async fn get_puzzle_and_solution(
        &self,
        coin_id: &Bytes32,
        height: u32,
    ) -> Result<CoinSpend, RpcError> {
        puzzle_and_solution_coin_spend(self.store.as_ref(), &self.constants, coin_id, height).await
    }

    /// Every mempool transaction id` — `get_all_mempool_tx_ids`
    ///.
    pub async fn get_all_mempool_tx_ids(&self) -> Vec<Bytes32> {
        let mp = self.mempool.lock().await;
        mp.items_by_fee().iter().map(|i| i.name).collect()
    }

    /// Every mempool item, keyed by transaction id` — `get_all_mempool_items`
    ///.
    pub async fn get_all_mempool_items(&self) -> Vec<MempoolItemJson> {
        let mp = self.mempool.lock().await;
        mp.items_by_fee()
            .iter()
            .map(|i| mempool_item_json(i))
            .collect()
    }

    /// One mempool item by transaction id` — `get_mempool_item_by_tx_id`
    ///: unknown is an ERROR. `include_pending` is accepted but a
    /// no-op — this mempool holds no pending set (a PENDING-classed bundle is not retained).
    ///
    /// # Errors
    /// Returns [`RpcError::BadRequest`] if the id is not resident.
    pub async fn get_mempool_item_by_tx_id(
        &self,
        tx_id: &Bytes32,
    ) -> Result<MempoolItemJson, RpcError> {
        let mp = self.mempool.lock().await;
        mp.get(tx_id)
            .map(mempool_item_json)
            .ok_or_else(|| RpcError::BadRequest(format!("Tx id {tx_id} not in the mempool")))
    }

    /// Every mempool item spending a coin` — `get_mempool_items_by_coin_name`
    ///. An unknown coin answers an empty list.
    pub async fn get_mempool_items_by_coin_name(
        &self,
        coin_name: &Bytes32,
    ) -> Vec<MempoolItemJson> {
        let mp = self.mempool.lock().await;
        mp.items_by_fee()
            .iter()
            .filter(|i| i.removals.contains(coin_name))
            .map(|i| mempool_item_json(i))
            .collect()
    }

    /// The network's AGG_SIG_ME additional data` — `get_aggsig_additional_data`
    ///.
    #[must_use]
    pub fn get_aggsig_additional_data(&self) -> Bytes32 {
        self.constants.agg_sig_me_additional_data
    }

    /// Network name / address prefix / genesis challenge` — `get_network_info`
    ///. Without attached live state the name is inferred from the
    /// consensus constants.
    #[must_use]
    pub fn get_network_info(&self) -> (String, String, Bytes32) {
        let network_name = self.live.get().map_or_else(
            || {
                if self.constants.genesis_challenge == MAINNET.genesis_challenge {
                    "mainnet".to_string()
                } else {
                    "testnet".to_string()
                }
            },
            |l| l.network_id.clone(),
        );
        let prefix = if network_name == "mainnet" {
            "xch"
        } else {
            "txch"
        };
        (
            network_name,
            prefix.to_string(),
            self.constants.genesis_challenge,
        )
    }

    /// The live peer connections — `get_connections`. Serves the INBOUND session map;
    /// per-connection byte/time counters are not tracked per peer and report 0. Answers empty
    /// without attached live state.
    pub async fn get_connections(&self, node_type: Option<u8>) -> Vec<Map<String, Value>> {
        let Some(live) = self.live.get() else {
            return Vec::new();
        };
        let peers = live.inbound_peers.read().await;
        let mut out = Vec::new();
        for (peer_id, peer) in peers.iter() {
            let peer_type = *peer.node_type.read().await as u8;
            if node_type.is_some_and(|t| t != peer_type) {
                continue;
            }
            let mut m = Map::new();
            m.insert("type".to_string(), Value::from(peer_type));
            m.insert("local_port".to_string(), Value::from(live.local_port));
            m.insert("peer_host".to_string(), Value::from(""));
            m.insert("peer_port".to_string(), Value::from(0));
            m.insert("peer_server_port".to_string(), Value::from(0));
            m.insert("node_id".to_string(), Value::from(peer_id.to_string()));
            m.insert("creation_time".to_string(), Value::from(0));
            m.insert("bytes_read".to_string(), Value::from(0));
            m.insert("bytes_written".to_string(), Value::from(0));
            m.insert("last_message_time".to_string(), Value::from(0));
            out.push(m);
        }
        out
    }
}

fn check_id_cap(len: usize) -> Result<(), RpcError> {
    if len > MAX_IDS_PER_REQUEST {
        return Err(RpcError::BadRequest(format!(
            "{len} ids exceeds the {MAX_IDS_PER_REQUEST}-id cap"
        )));
    }
    Ok(())
}

fn sp_not_in_cache(sp_hash: &Bytes32) -> RpcError {
    RpcError::BadRequest(format!("Did not find sp {} in cache", plain_hex(sp_hash)))
}

fn eos_not_in_cache(challenge_hash: &Bytes32) -> RpcError {
    RpcError::BadRequest(format!(
        "Did not find eos {} in cache",
        plain_hex(challenge_hash)
    ))
}

// The v1 plot-filter halvings ladder, keyed on the newer block's height.
fn plot_filter_prefix_bits(constants: &ConsensusConstants, height: u32) -> u8 {
    let mut bits = constants.number_zero_bits_plot_filter;
    if height >= constants.plot_filter_32_height {
        bits = bits.saturating_sub(4);
    } else if height >= constants.plot_filter_64_height {
        bits = bits.saturating_sub(3);
    } else if height >= constants.plot_filter_128_height {
        bits = bits.saturating_sub(2);
    } else if height >= constants.hard_fork_height {
        bits = bits.saturating_sub(1);
    }
    bits
}

// The MempoolItem JSON shape (spend_bundle / fee / cost / npc_result / spend_bundle_name /
// additions / removals), built from the node's internal item.
fn mempool_item_json(item: &dg_xch_node::MempoolItem) -> MempoolItemJson {
    MempoolItemJson {
        spend_bundle: item.bundle.clone(),
        fee: item.fee,
        cost: item.cost,
        npc_result: NPCResult {
            error: None,
            conds: Some(item.conds.clone()),
        },
        spend_bundle_name: item.name,
        additions: additions_for_conditions(&item.conds, &[]),
        removals: item.bundle.removals(),
        ..Default::default()
    }
}

// Resolve a block's generator back-references from the confirmed chain and assemble the CLVM
// runner input (shared by get_puzzle_and_solution and the get_block_spends pair).
async fn generator_input_for_block<S: CoinStore + BlockStore>(
    store: &S,
    constants: &ConsensusConstants,
    block: &FullBlock,
) -> Result<BlockGeneratorInput, RpcError> {
    let Some(generator) = block.transactions_generator.clone() else {
        return Err(RpcError::BadRequest(
            "block carries no transactions generator".to_string(),
        ));
    };
    let mut generator_refs = Vec::with_capacity(block.transactions_generator_ref_list.len());
    for (index, ref_height) in block.transactions_generator_ref_list.iter().enumerate() {
        let g = store
            .get_generator_at_height(*ref_height)
            .await?
            .ok_or_else(|| {
                RpcError::Corrupt(format!("missing generator ref at height {ref_height}"))
            })?;
        generator_refs.push(GeneratorReference {
            height: *ref_height,
            index: u32::try_from(index).unwrap_or(u32::MAX),
            generator: g,
        });
    }
    Ok(BlockGeneratorInput {
        transactions_generator: generator,
        generator_refs,
        constants: *constants,
        height: block.height(),
        flags: BlockGeneratorFlags::for_height(constants, block.height()),
    })
}

/// Recover the [`CoinSpend`] (coin + puzzle reveal + solution) of a coin spent at exactly `height` by
/// re-running that block's generator through the existing CLVM runner —
/// `get_puzzle_and_solution`. Shared by the HTTP RPC and the light-wallet p2p handler
/// (`RequestPuzzleSolution`) so both paths run the one tested extraction, never a second VM.
///
/// # Errors
/// Returns [`RpcError::BadRequest`] if the coin is unknown, not spent at `height`, or its block or
/// generator is missing; [`RpcError::Store`] on a query failure; [`RpcError::Corrupt`] if a confirmed
/// generator back-reference is absent.
pub(crate) async fn puzzle_and_solution_coin_spend<S: CoinStore + BlockStore>(
    store: &S,
    constants: &ConsensusConstants,
    coin_id: &Bytes32,
    height: u32,
) -> Result<CoinSpend, RpcError> {
    let coin_record = store
        .get_coin_record(coin_id)
        .await?
        .ok_or_else(|| RpcError::BadRequest(format!("coin {coin_id} not found")))?;
    if !coin_record.spent || coin_record.spent_block_index != height {
        return Err(RpcError::BadRequest(format!(
            "invalid height {height} for coin {coin_id} (spent at {})",
            coin_record.spent_block_index
        )));
    }
    let record = store
        .get_block_record_by_height(height)
        .await?
        .ok_or_else(|| RpcError::BadRequest(format!("no confirmed block at height {height}")))?;
    let block = store
        .get_block(&record.header_hash)
        .await?
        .ok_or_else(|| RpcError::BadRequest(format!("block {} has no body", record.header_hash)))?;
    if block.transactions_generator.is_none() {
        return Err(RpcError::BadRequest(format!(
            "block at height {height} carries no transactions generator"
        )));
    }
    let input = generator_input_for_block(store, constants, &block).await?;
    coin_spend_from_generator(&input, coin_id)
        .map_err(|e| RpcError::BadRequest(format!("failed to extract puzzle and solution: {e:?}")))?
        .ok_or_else(|| {
            RpcError::BadRequest(format!(
                "coin {coin_id} is not spent by the generator at height {height}"
            ))
        })
}

/// The `get_additions_and_removals` response — coins created and coins spent at a block
/// (the `{additions, removals}` JSON).
#[cfg(feature = "coin-index")]
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AdditionsAndRemovals {
    pub additions: Vec<CoinRecord>,
    pub removals: Vec<CoinRecord>,
}

// ---- TLS ---------------------------------------------------------------------------------------

/// The RPC listener's TLS material: the rustls server config plus the cert-hash node
/// id derived from the served leaf certificate.
pub struct RpcTlsContext {
    pub server_config: Arc<ServerConfig>,
    pub node_id: Bytes32,
}

/// Build the RPC listener's TLS config for the selected [`RpcTlsMode`].
///
/// `PrivateCa` is mutual TLS rooted at a per-install private CA.
/// private CA and the client MUST present a cert chaining to that CA. The private CA comes from
/// `PRIVATE_CA_CRT`/`PRIVATE_CA_KEY` (inline PEM) if both are set, else it is loaded from — or
/// generated once and persisted into `<ssl_dir>/ca/private_ca.{crt,key}`.
///
/// `Local` is server-only TLS with an ephemeral self-signed cert and NO client-cert requirement,
/// for private/loopback operation; it is refused on a non-loopback `--rpc` bind (fail closed).
///
/// TLS below 1.3 is refused in either mode.
///
/// # Errors
/// Returns an I/O error on cert generation/parsing/verifier failure, if `Local` is selected for a
/// routable bind, or if the resolved private CA is the public network CA.
pub fn build_rpc_tls_context(
    mode: &RpcTlsMode,
    bind: SocketAddr,
) -> Result<RpcTlsContext, IoError> {
    match mode {
        RpcTlsMode::PrivateCa { ssl_dir } => build_private_ca_rpc_tls(ssl_dir),
        RpcTlsMode::Local => build_local_rpc_tls(bind),
    }
}

fn build_private_ca_rpc_tls(ssl_dir: &Path) -> Result<RpcTlsContext, IoError> {
    let (ca_crt, ca_key) = resolve_private_ca(ssl_dir)?;
    // A publicly distributed CA cannot authenticate RPC clients.
    if ca_crt == CHIA_CA_CRT.as_bytes() {
        return Err(IoError::other(
            "refusing to root RPC client-auth at the public network CA; \
             supply a private CA (PRIVATE_CA_CRT/KEY or <ssl_dir>/ca) or use --rpc-tls local",
        ));
    }
    let (cert_bytes, key_bytes) = generate_ca_signed_cert_data(&ca_crt, &ca_key)?;
    let certs = load_certs_from_bytes(&cert_bytes)?;
    let key = load_private_key_from_bytes(&key_bytes)?;
    let node_id = Bytes32::new(hash_256(
        certs.first().map(AsRef::as_ref).unwrap_or_default(),
    ));
    let mut roots = RootCertStore::empty();
    for cert in load_certs_from_bytes(&ca_crt)? {
        roots
            .add(cert)
            .map_err(|e| IoError::other(format!("invalid RPC root cert: {e:?}")))?;
    }
    let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .map_err(|e| IoError::other(format!("client verifier: {e:?}")))?;
    let server_config = ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .map_err(|e| IoError::other(format!("invalid RPC server cert: {e:?}")))?;
    Ok(RpcTlsContext {
        server_config: Arc::new(server_config),
        node_id,
    })
}

// Local/private: transport TLS only, no client-cert requirement — permitted on loopback alone.
fn build_local_rpc_tls(bind: SocketAddr) -> Result<RpcTlsContext, IoError> {
    if !bind.ip().is_loopback() {
        return Err(IoError::other(format!(
            "--rpc-tls local is unauthenticated and only allowed on a loopback --rpc bind; got \
             {bind}. Use --rpc-tls private-ca for a routable RPC address."
        )));
    }
    // Ephemeral in-memory CA + server leaf: encrypt the loopback transport without a persisted
    // identity or any client-cert machinery.
    let (ca_crt, ca_key) = make_ca_cert_data()?;
    let (cert_bytes, key_bytes) = generate_ca_signed_cert_data(&ca_crt, &ca_key)?;
    let certs = load_certs_from_bytes(&cert_bytes)?;
    let key = load_private_key_from_bytes(&key_bytes)?;
    let node_id = Bytes32::new(hash_256(
        certs.first().map(AsRef::as_ref).unwrap_or_default(),
    ));
    let server_config = ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| IoError::other(format!("invalid RPC server cert: {e:?}")))?;
    Ok(RpcTlsContext {
        server_config: Arc::new(server_config),
        node_id,
    })
}

/// Resolve the RPC private CA: inline-PEM env override, else load-or-generate under `<ssl_dir>/ca`.
fn resolve_private_ca(ssl_dir: &Path) -> Result<(Vec<u8>, Vec<u8>), IoError> {
    if let (Ok(crt), Ok(key)) = (
        std::env::var("PRIVATE_CA_CRT"),
        std::env::var("PRIVATE_CA_KEY"),
    ) {
        return Ok((crt.into_bytes(), key.into_bytes()));
    }
    let ca_dir = ssl_dir.join("ca");
    let crt_path = ca_dir.join("private_ca.crt");
    let key_path = ca_dir.join("private_ca.key");
    if crt_path.exists() && key_path.exists() {
        return Ok((std::fs::read(&crt_path)?, std::fs::read(&key_path)?));
    }
    std::fs::create_dir_all(&ca_dir)?;
    // Generate a unique private CA ONCE and persist it. Distribute <ssl_dir>/ca/private_ca.crt to
    // RPC tooling and sign client certs with the paired key.
    let (crt, key) = make_ca_cert(&crt_path, &key_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
    }
    Ok((crt, key))
}

// ---- HTTP adapter over dg_xch_servers::RpcServer ----------------------------------------------

#[derive(Deserialize)]
struct HeaderHashReq {
    header_hash: Bytes32,
}

#[derive(Deserialize)]
struct RangeReq {
    start: u32,
    end: u32,
}

#[derive(Deserialize)]
struct BlocksReq {
    start: u32,
    end: u32,
    #[serde(default)]
    exclude_header_hash: bool,
    #[serde(default)]
    #[allow(dead_code)]
    exclude_reorged: bool,
}

#[derive(Deserialize)]
struct HeightReq {
    height: u32,
}

#[derive(Deserialize)]
struct FarmBlockReq {
    address: String,
    #[serde(default = "one")]
    blocks: i64,
    #[serde(default)]
    guarantee_tx_block: bool,
}

const fn one() -> i64 {
    1
}

#[derive(Deserialize)]
struct AutoFarmReq {
    // The request field is `auto_farm`; accept the older name too.
    #[serde(rename = "auto_farm", alias = "should_auto_farm")]
    auto_farm: bool,
}

/// The block-production control a simulator attaches to the RPC, adding the `farm_block`,
/// `set_auto_farming` and `get_auto_farming` endpoints. A production node attaches none, and those
/// endpoints 404.
#[async_trait]
pub trait SimControl: Send + Sync {
    /// Farm `blocks` blocks whose rewards pay `address` (a bech32 puzzle-hash address), sealing any
    /// pending mempool transactions; `guarantee_tx_block` forces each to be a transaction block.
    async fn farm_block(
        &self,
        address: &str,
        blocks: u32,
        guarantee_tx_block: bool,
    ) -> Result<(), String>;
    /// Set auto-farming (a block is sealed whenever a wallet submits a transaction); returns the new
    /// state.
    fn set_auto_farming(&self, should_auto_farm: bool) -> bool;
    /// Whether auto-farming is on.
    fn auto_farming(&self) -> bool;
}

#[derive(Deserialize)]
struct NamesReq {
    names: Vec<Bytes32>,
    #[serde(flatten)]
    window: CoinQueryWindow,
}

#[derive(Deserialize)]
struct NameReq {
    name: String,
}

// A coin id must be exactly 32 bytes. The message is the one wallets match on to surface a "bad
// coin id", so it is part of the RPC contract.
fn coin_id_from_hex(s: &str) -> Result<Bytes32, RpcError> {
    use std::str::FromStr;
    let hex = s.strip_prefix("0x").unwrap_or(s);
    if hex.len() != 64 {
        return Err(RpcError::BadRequest("bad bytes32 initializer".to_string()));
    }
    Bytes32::from_str(s).map_err(|_| RpcError::BadRequest("bad bytes32 initializer".to_string()))
}

#[cfg(feature = "coin-index")]
#[derive(Deserialize)]
struct ParentIdsReq {
    parent_ids: Vec<Bytes32>,
    #[serde(flatten)]
    window: CoinQueryWindow,
}

#[cfg(feature = "coin-index")]
#[derive(Deserialize)]
struct PuzzleHashReq {
    puzzle_hash: Bytes32,
    #[serde(flatten)]
    window: CoinQueryWindow,
}

#[cfg(feature = "coin-index")]
#[derive(Deserialize)]
struct PuzzleHashesReq {
    puzzle_hashes: Vec<Bytes32>,
    #[serde(flatten)]
    window: CoinQueryWindow,
}

#[derive(Deserialize)]
struct PushTxReq {
    spend_bundle: SpendBundle,
}

#[cfg(feature = "hint")]
#[derive(Deserialize)]
struct HintReq {
    hint: Bytes32,
    #[serde(flatten)]
    window: CoinQueryWindow,
}

#[cfg(feature = "hint")]
#[derive(Deserialize)]
struct HintsReq {
    hints: Vec<Bytes32>,
    #[serde(flatten)]
    window: CoinQueryWindow,
}

#[derive(Deserialize)]
struct PuzzleSolutionReq {
    coin_id: Bytes32,
    height: u32,
}

#[derive(Deserialize)]
struct NetworkSpaceReq {
    newer_block_header_hash: Bytes32,
    older_block_header_hash: Bytes32,
}

#[derive(Deserialize, Default)]
struct SpOrEosReq {
    #[serde(default)]
    sp_hash: Option<Bytes32>,
    #[serde(default)]
    challenge_hash: Option<Bytes32>,
}

#[derive(Deserialize)]
struct TxIdReq {
    tx_id: Bytes32,
}

#[derive(Deserialize)]
struct CoinNameReq {
    coin_name: Bytes32,
}

#[derive(Deserialize, Default)]
struct ConnectionsReq {
    #[serde(default)]
    node_type: Option<u8>,
}

// get_fee_estimate request: exactly one of spend_bundle|cost, plus target_times (seconds).
#[derive(Deserialize, Default)]
struct FeeEstimateReq {
    #[serde(default)]
    spend_bundle: Option<SpendBundle>,
    #[serde(default)]
    cost: Option<u64>,
    #[serde(default)]
    target_times: Vec<u64>,
}

/// `get_fee_estimate` response. Field names and JSON types match the wire convention exactly so
/// existing wallets and tooling parse it unchanged.
#[derive(Serialize)]
pub struct FeeEstimateResponse {
    /// Estimated total fee (mojos) per target time, monotonically decreasing in target time.
    pub estimates: Vec<u64>,
    /// The requested target times (seconds), sorted ascending — echoed back.
    pub target_times: Vec<u64>,
    /// Fee-rate (mojos per clvm cost) for near-term inclusion (`estimate_fee_rate(1)`).
    pub current_fee_rate: f64,
    /// Current mempool cost (`total_mempool_cost`).
    pub mempool_size: u64,
    /// Current mempool fees (`total_mempool_fees`).
    pub mempool_fees: u64,
    /// Number of spends resident in the mempool.
    pub num_spends: u64,
    /// Mempool max cost.
    pub mempool_max_size: u64,
    /// Whether the node is synced.
    pub full_node_synced: bool,
    /// Peak height (0 if no peak).
    pub peak_height: u32,
    /// Timestamp of the last transaction block.
    pub last_peak_timestamp: u64,
    /// Node wall-clock (unix seconds).
    pub node_time_utc: u64,
    /// CLVM cost of the last transaction block.
    pub last_block_cost: u64,
    /// Fees in the last transaction block.
    pub fees_last_block: u64,
    /// Fee-per-cost of the last transaction block.
    pub fee_rate_last_block: f64,
    /// Height of the last transaction block.
    pub last_tx_block_height: u32,
}

/// The served route list (`get_routes`), honoring the compiled service tier.
#[must_use]
pub fn route_names() -> Vec<&'static str> {
    #[allow(unused_mut)]
    let mut routes = vec![
        "/get_blockchain_state",
        "/get_block",
        "/get_blocks",
        "/get_block_record",
        "/get_block_record_by_height",
        "/get_block_records",
        "/get_block_spends",
        "/get_block_spends_with_conditions",
        "/get_unfinished_block_headers",
        "/get_network_space",
        "/get_recent_signage_point_or_eos",
        "/get_coin_records_by_names",
        "/get_coin_record_by_name",
        "/get_puzzle_and_solution",
        "/push_tx",
        "/get_all_mempool_tx_ids",
        "/get_all_mempool_items",
        "/get_mempool_item_by_tx_id",
        "/get_mempool_items_by_coin_name",
        "/get_fee_estimate",
        "/get_aggsig_additional_data",
        "/get_network_info",
        "/get_connections",
        "/get_routes",
        "/get_version",
        "/healthz",
    ];
    #[cfg(feature = "coin-index")]
    routes.extend([
        "/get_coin_records_by_puzzle_hash",
        "/get_coin_records_by_puzzle_hashes",
        "/get_coin_records_by_parent_ids",
        "/get_additions_and_removals",
    ]);
    #[cfg(feature = "hint")]
    routes.push("/get_coin_records_by_hint");
    routes
}

// Routes `RpcServer` HTTP requests to `NodeRpc` and speaks the response envelope. One
// dispatcher keyed on the URI path; the body is a JSON params object.
pub struct NodeRpcHandler<S> {
    rpc: Arc<NodeRpc<S>>,
}

impl<S> NodeRpcHandler<S>
where
    S: CoinStore + BlockStore + Send + Sync + 'static,
{
    #[must_use]
    pub fn new(rpc: Arc<NodeRpc<S>>) -> Self {
        Self { rpc }
    }

    // Dispatch one request. `Ok(None)` = unknown endpoint (HTTP 404); `Ok(Some(map))` = the
    // response object BEFORE the success flag is stamped; `Err` = the error envelope. Public so the
    // plain-HTTP control facade can serve the same RPC surface as the mTLS server.
    #[allow(clippy::too_many_lines)]
    pub async fn route(
        &self,
        path: &str,
        body: &[u8],
    ) -> Result<Option<Map<String, Value>>, RpcError> {
        let out = match path {
            "/get_blockchain_state" => {
                let summary = self.rpc.get_blockchain_state().await?;
                // serde_json holds no >u64 integers: serialize with space zeroed, then inject
                // the real value through json_u128 (see that fn's divergence note).
                let mut state = summary.state.clone();
                let space = state.space;
                state.space = 0;
                let mut state = to_value(&state)?;
                if let Value::Object(obj) = &mut state {
                    obj.insert("space".to_string(), json_u128(space));
                    obj.insert(
                        "average_block_time".to_string(),
                        summary.average_block_time.map_or(Value::Null, Value::from),
                    );
                    obj.insert(
                        "mempool_fees".to_string(),
                        Value::from(summary.mempool_fees),
                    );
                }
                obj_with("blockchain_state", state)
            }
            "/get_block" => {
                let req: HeaderHashReq = parse(body)?;
                envelope("block", &self.rpc.get_block(&req.header_hash).await?)?
            }
            "/get_blocks" => {
                let req: BlocksReq = parse(body)?;
                let blocks = self.rpc.get_blocks(req.start, req.end).await?;
                let mut arr = Vec::with_capacity(blocks.len());
                for (block, header_hash) in blocks {
                    let mut v = to_value(&block)?;
                    if !req.exclude_header_hash
                        && let Value::Object(obj) = &mut v
                    {
                        // The wire convention injects PLAIN hex here.
                        obj.insert(
                            "header_hash".to_string(),
                            Value::from(plain_hex(&header_hash)),
                        );
                    }
                    arr.push(v);
                }
                obj_with("blocks", Value::from(arr))
            }
            "/get_block_record" => {
                let req: HeaderHashReq = parse(body)?;
                envelope(
                    "block_record",
                    &self.rpc.get_block_record(&req.header_hash).await?,
                )?
            }
            "/get_block_record_by_height" => {
                let req: HeightReq = parse(body)?;
                envelope(
                    "block_record",
                    &self.rpc.get_block_record_by_height(req.height).await?,
                )?
            }
            "/get_block_records" => {
                let req: RangeReq = parse(body)?;
                envelope(
                    "block_records",
                    &self.rpc.get_block_records(req.start, req.end).await?,
                )?
            }
            "/get_block_spends" => {
                let req: HeaderHashReq = parse(body)?;
                envelope(
                    "block_spends",
                    &self.rpc.get_block_spends(&req.header_hash).await?,
                )?
            }
            "/get_block_spends_with_conditions" => {
                let req: HeaderHashReq = parse(body)?;
                let spends = self
                    .rpc
                    .get_block_spends_with_conditions(&req.header_hash)
                    .await?;
                let mut arr = Vec::with_capacity(spends.len());
                for (coin_spend, conditions) in spends {
                    let mut m = Map::new();
                    m.insert("coin_spend".to_string(), to_value(&coin_spend)?);
                    m.insert(
                        "conditions".to_string(),
                        Value::from(
                            conditions
                                .iter()
                                .map(condition_json)
                                .collect::<Vec<Value>>(),
                        ),
                    );
                    arr.push(Value::Object(m));
                }
                obj_with("block_spends_with_conditions", Value::from(arr))
            }
            "/get_unfinished_block_headers" => {
                envelope("headers", &self.rpc.get_unfinished_block_headers().await?)?
            }
            "/get_network_space" => {
                let req: NetworkSpaceReq = parse(body)?;
                let space = self
                    .rpc
                    .get_network_space(&req.newer_block_header_hash, &req.older_block_header_hash)
                    .await?;
                obj_with("space", json_u128(space))
            }
            "/get_recent_signage_point_or_eos" => {
                let req: SpOrEosReq = parse_or_default(body)?;
                self.rpc
                    .get_recent_signage_point_or_eos(
                        req.sp_hash.as_ref(),
                        req.challenge_hash.as_ref(),
                    )
                    .await?
            }
            "/get_coin_records_by_names" => {
                let req: NamesReq = parse(body)?;
                envelope(
                    "coin_records",
                    &self
                        .rpc
                        .get_coin_records_by_names(&req.names, req.window)
                        .await?,
                )?
            }
            "/get_coin_record_by_name" => {
                let req: NameReq = parse(body)?;
                let name = coin_id_from_hex(&req.name)?;
                envelope(
                    "coin_record",
                    &self.rpc.get_coin_record_by_name(&name).await?,
                )?
            }
            #[cfg(feature = "coin-index")]
            "/get_coin_records_by_parent_ids" => {
                let req: ParentIdsReq = parse(body)?;
                envelope(
                    "coin_records",
                    &self
                        .rpc
                        .get_coin_records_by_parent_ids(&req.parent_ids, req.window)
                        .await?,
                )?
            }
            #[cfg(feature = "coin-index")]
            "/get_coin_records_by_puzzle_hash" => {
                let req: PuzzleHashReq = parse(body)?;
                envelope(
                    "coin_records",
                    &self
                        .rpc
                        .get_coin_records_by_puzzle_hash(&req.puzzle_hash, req.window)
                        .await?,
                )?
            }
            #[cfg(feature = "coin-index")]
            "/get_coin_records_by_puzzle_hashes" => {
                let req: PuzzleHashesReq = parse(body)?;
                envelope(
                    "coin_records",
                    &self
                        .rpc
                        .get_coin_records_by_puzzle_hashes(&req.puzzle_hashes, req.window)
                        .await?,
                )?
            }
            "/push_tx" => {
                let req: PushTxReq = parse(body)?;
                match self.rpc.push_tx(req.spend_bundle).await {
                    Ok(_name) => obj_with("status", Value::from("SUCCESS")),
                    // A PENDING-classed rejection answers {"status": "PENDING"}; only FAILED
                    // errors.
                    Err(RpcError::Mempool(m)) => {
                        let (status, err_name) = m.ack();
                        if status == TXStatus::PENDING {
                            obj_with("status", Value::from("PENDING"))
                        } else {
                            return Err(RpcError::BadRequest(format!(
                                "Failed to include transaction, error {err_name}"
                            )));
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
            #[cfg(feature = "hint")]
            "/get_coin_records_by_hint" => {
                let req: HintReq = parse(body)?;
                envelope(
                    "coin_records",
                    &self
                        .rpc
                        .get_coin_records_by_hint(&req.hint, req.window)
                        .await?,
                )?
            }
            #[cfg(feature = "hint")]
            "/get_coin_records_by_hints" => {
                let req: HintsReq = parse(body)?;
                let mut records = Vec::new();
                for hint in &req.hints {
                    records.extend(self.rpc.get_coin_records_by_hint(hint, req.window).await?);
                }
                envelope("coin_records", &records)?
            }
            #[cfg(feature = "coin-index")]
            "/get_additions_and_removals" => {
                let req: HeaderHashReq = parse(body)?;
                let ar = self
                    .rpc
                    .get_additions_and_removals(&req.header_hash)
                    .await?;
                let mut m = Map::new();
                m.insert("additions".to_string(), to_value(&ar.additions)?);
                m.insert("removals".to_string(), to_value(&ar.removals)?);
                m
            }
            "/get_puzzle_and_solution" => {
                let req: PuzzleSolutionReq = parse(body)?;
                envelope(
                    "coin_solution",
                    &self
                        .rpc
                        .get_puzzle_and_solution(&req.coin_id, req.height)
                        .await?,
                )?
            }
            "/get_all_mempool_tx_ids" => {
                envelope("tx_ids", &self.rpc.get_all_mempool_tx_ids().await)?
            }
            "/get_all_mempool_items" => {
                // The map is keyed by PLAIN-hex tx id.
                let items = self.rpc.get_all_mempool_items().await;
                let mut m = Map::new();
                for item in items {
                    m.insert(plain_hex(&item.spend_bundle_name), to_value(&item)?);
                }
                obj_with("mempool_items", Value::Object(m))
            }
            "/get_mempool_item_by_tx_id" => {
                let req: TxIdReq = parse(body)?;
                envelope(
                    "mempool_item",
                    &self.rpc.get_mempool_item_by_tx_id(&req.tx_id).await?,
                )?
            }
            "/get_mempool_items_by_coin_name" => {
                let req: CoinNameReq = parse(body)?;
                envelope(
                    "mempool_items",
                    &self
                        .rpc
                        .get_mempool_items_by_coin_name(&req.coin_name)
                        .await,
                )?
            }
            "/get_fee_estimate" => {
                let req: FeeEstimateReq = parse(body)?;
                let resp = self
                    .rpc
                    .get_fee_estimate(req.spend_bundle, req.cost, req.target_times)
                    .await?;
                // A flat object (no named-key wrapper), then `success` is stamped.
                match to_value(&resp)? {
                    Value::Object(m) => m,
                    _ => Map::new(),
                }
            }
            "/get_aggsig_additional_data" => obj_with(
                "additional_data",
                Value::from(plain_hex(&self.rpc.get_aggsig_additional_data())),
            ),
            "/get_network_info" => {
                let (name, prefix, genesis) = self.rpc.get_network_info();
                let mut m = Map::new();
                m.insert("network_name".to_string(), Value::from(name));
                m.insert("network_prefix".to_string(), Value::from(prefix));
                m.insert(
                    "genesis_challenge".to_string(),
                    Value::from(plain_hex(&genesis)),
                );
                m
            }
            "/get_connections" => {
                let req: ConnectionsReq = parse_or_default(body)?;
                let connections = self.rpc.get_connections(req.node_type).await;
                obj_with(
                    "connections",
                    Value::from(
                        connections
                            .into_iter()
                            .map(Value::Object)
                            .collect::<Vec<_>>(),
                    ),
                )
            }
            "/get_routes" => obj_with(
                "routes",
                Value::from(
                    route_names()
                        .into_iter()
                        .map(Value::from)
                        .collect::<Vec<_>>(),
                ),
            ),
            "/get_version" => obj_with("version", Value::from(env!("CARGO_PKG_VERSION"))),
            "/healthz" => Map::new(),
            "/farm_block" => {
                let Some(sim) = self.rpc.sim.get() else {
                    return Ok(None);
                };
                let req: FarmBlockReq = parse(body)?;
                let blocks = u32::try_from(req.blocks.max(0)).unwrap_or(u32::MAX);
                sim.farm_block(&req.address, blocks, req.guarantee_tx_block)
                    .await
                    .map_err(RpcError::BadRequest)?;
                Map::new()
            }
            "/set_auto_farming" => {
                let Some(sim) = self.rpc.sim.get() else {
                    return Ok(None);
                };
                let req: AutoFarmReq = parse(body)?;
                obj_with(
                    "auto_farm_enabled",
                    Value::from(sim.set_auto_farming(req.auto_farm)),
                )
            }
            "/get_auto_farming" => {
                let Some(sim) = self.rpc.sim.get() else {
                    return Ok(None);
                };
                obj_with("auto_farm_enabled", Value::from(sim.auto_farming()))
            }
            _ => return Ok(None),
        };
        Ok(Some(out))
    }
}

// Clamp each element to the running minimum so the sequence never increases (the fee estimator
// can quote a HIGHER rate for a LONGER wait — an artifact users do not expect).
fn make_monotonically_decreasing(seq: &[f64]) -> Vec<f64> {
    let mut out = Vec::with_capacity(seq.len());
    let mut min = f64::INFINITY;
    for (i, &n) in seq.iter().enumerate() {
        if i == 0 || n <= min {
            out.push(n);
            min = n;
        } else {
            out.push(min);
        }
    }
    out
}

fn parse<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, RpcError> {
    serde_json::from_slice(body).map_err(|e| RpcError::BadRequest(e.to_string()))
}

// For endpoints whose parameters are all optional: an empty body reads as `{}`.
fn parse_or_default<T: for<'de> Deserialize<'de> + Default>(body: &[u8]) -> Result<T, RpcError> {
    if body.is_empty() {
        return Ok(T::default());
    }
    parse(body)
}

fn to_value<T: Serialize>(value: &T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|e| RpcError::Corrupt(e.to_string()))
}

fn envelope<T: Serialize>(key: &str, value: &T) -> Result<Map<String, Value>, RpcError> {
    Ok(obj_with(key, to_value(value)?))
}

fn obj_with(key: &str, value: Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(key.to_string(), value);
    m
}

// A u128 as JSON: an exact integer while it fits u64, else an f64 (serde_json cannot carry
// arbitrary-precision integers). Mainnet netspace exceeds u64 —
// the f64 form keeps 53 bits of mantissa, far inside the estimate's own error bar.
#[allow(clippy::cast_precision_loss)]
fn json_u128(value: u128) -> Value {
    u64::try_from(value).map_or_else(|_| Value::from(value as f64), Value::from)
}

// PLAIN hex (no 0x) — the wire convention for injected header hashes, mempool map keys, and
// `additional_data`.
fn plain_hex(bytes: &Bytes32) -> String {
    let s = bytes.to_string();
    s.strip_prefix("0x").map_or(s.clone(), ToString::to_string)
}

// The condition JSON shape:
// opcode as 0x-prefixed hex, vars as plain hex.
fn condition_json(cond: &RawCondition) -> Value {
    let mut m = Map::new();
    m.insert(
        "opcode".to_string(),
        Value::from(format!("0x{}", hex_of(&cond.opcode))),
    );
    m.insert(
        "vars".to_string(),
        Value::from(
            cond.vars
                .iter()
                .map(|v| Value::from(hex_of(v)))
                .collect::<Vec<_>>(),
        ),
    );
    Value::Object(m)
}

fn hex_of(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

async fn read_body(req: RpcRequest) -> (Parts, HeaderMap, Vec<u8>) {
    let headers = req.response_headers.clone();
    match req.request_type {
        RequestType::Stream(r) => {
            let (parts, body) = r.into_parts();
            let bytes = body
                .collect()
                .await
                .map(|c| c.to_bytes().to_vec())
                .unwrap_or_default();
            (parts, headers, bytes)
        }
        RequestType::Sized(r) => {
            let (parts, body) = r.into_parts();
            let bytes = body
                .collect()
                .await
                .map(|c| c.to_bytes().to_vec())
                .unwrap_or_default();
            (parts, headers, bytes)
        }
    }
}

#[async_trait]
impl<S> RpcHandler for NodeRpcHandler<S>
where
    S: CoinStore + BlockStore + Send + Sync + 'static,
{
    async fn handle(
        &self,
        request: RpcRequest,
        mut response: Response<Full<Bytes>>,
        _address: &SocketAddr,
    ) -> Result<Response<Full<Bytes>>, (Parts, HeaderMap, IoError)> {
        let (parts, _headers, body) = read_body(request).await;
        let path = parts.uri.path().to_string();
        response.headers_mut().insert(
            CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        // An oversize body is refused at the transport.
        if body.len() > MAX_RPC_BODY_BYTES {
            *response.status_mut() = StatusCode::PAYLOAD_TOO_LARGE;
            *response.body_mut() = Full::new(Bytes::from(
                serde_json::json!({
                    "success": false,
                    "error": format!("request body exceeds {MAX_RPC_BODY_BYTES} bytes"),
                })
                .to_string(),
            ));
            return Ok(response);
        }
        let payload = match self.route(&path, &body).await {
            // Stamp success unless the handler already set it.
            Ok(Some(mut map)) => {
                map.entry("success".to_string())
                    .or_insert(Value::from(true));
                Value::Object(map)
            }
            // Unknown endpoint: 404.
            Ok(None) => {
                *response.status_mut() = StatusCode::NOT_FOUND;
                serde_json::json!({
                    "success": false,
                    "error": format!("unknown endpoint {path}"),
                })
            }
            // Application errors are HTTP-200 with the error envelope.
            Err(e) => serde_json::json!({
                "success": false,
                "error": e.to_string(),
                "traceback": Value::Null,
                "structuredError": Value::Null,
            }),
        };
        *response.body_mut() = Full::new(Bytes::from(payload.to_string()));
        Ok(response)
    }
}

impl dg_xch_core::errors::ErrorCode for RpcError {
    fn band(&self) -> dg_xch_core::errors::ErrorBand {
        match self {
            RpcError::Store(inner) => inner.band(),
            RpcError::Mempool(inner) => inner.band(),
            _ => dg_xch_core::errors::ErrorBand::Rpc,
        }
    }
    fn variant(&self) -> u16 {
        match self {
            RpcError::Store(inner) => inner.variant(),
            RpcError::Mempool(inner) => inner.variant(),
            RpcError::BadRequest(_) => 1,
            RpcError::Corrupt(_) => 2,
        }
    }
}
