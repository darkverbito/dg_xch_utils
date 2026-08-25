use async_trait::async_trait;
use dg_xch_core::blockchain::header_block::HeaderBlock;
use dg_xch_core::blockchain::peer_info::TimestampedPeerInfo;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::blockchain::spend_bundle::SpendBundle;
use dg_xch_core::blockchain::tx_status::TXStatus;
use dg_xch_core::blockchain::unfinished_block::UnfinishedBlock;
use dg_xch_core::protocols::ban::BanCause;
use dg_xch_core::protocols::farmer::{DeclareProofOfSpace, RequestSignedValues, SignedValues};
use dg_xch_core::protocols::full_node::{
    NewCompactVDF, NewPeak, NewSignagePointOrEndOfSubSlot, NewTransaction, NewUnfinishedBlock,
    NewUnfinishedBlock2, RejectBlock, RejectBlocks, RequestBlock, RequestBlocks, RequestCompactVDF,
    RequestMempoolTransactions, RequestPeers, RequestProofOfWeight,
    RequestSignagePointOrEndOfSubSlot, RequestTransaction, RequestUnfinishedBlock,
    RequestUnfinishedBlock2, RespondBlock, RespondBlocks, RespondCompactVDF, RespondEndOfSubSlot,
    RespondPeers, RespondSignagePoint, RespondTransaction, RespondUnfinishedBlock,
};
use dg_xch_core::protocols::rate_limits_v3::{
    configure_message, peer_supports_v3, settings_from_configure,
};
use dg_xch_core::protocols::shared::{
    CAPABILITIES, Capability, ConfigureWindowSizes, ErrorMessage, Handshake,
};
use dg_xch_core::protocols::timelord::{
    NewEndOfSubSlotVDF, NewInfusionPointVDF, NewPeakTimelord, NewSignagePointVDF,
    RespondCompactProofOfTime,
};
use dg_xch_core::protocols::wallet::{
    CoinState, CoinStateUpdate, FeeEstimate, FeeEstimateGroup, FeeRate, NewPeakWallet,
    PuzzleSolutionResponse, RegisterForCoinUpdates, RegisterForPhUpdates, RejectAdditionsRequest,
    RejectBlockHeaders, RejectCoinState, RejectHeaderBlocks, RejectHeaderRequest,
    RejectPuzzleSolution, RejectPuzzleState, RejectRemovalsRequest, RejectStateReason,
    RequestAdditions, RequestBlockHeader, RequestBlockHeaders, RequestChildren, RequestCoinState,
    RequestFeeEstimates, RequestHeaderBlocks, RequestPuzzleSolution, RequestPuzzleState,
    RequestRemovals, RequestRemoveCoinSubscriptions, RequestRemovePuzzleSubscriptions,
    RespondAdditions, RespondBlockHeader, RespondBlockHeaders, RespondChildren, RespondCoinState,
    RespondFeeEstimates, RespondHeaderBlocks, RespondPuzzleSolution, RespondPuzzleState,
    RespondRemovals, RespondRemoveCoinSubscriptions, RespondRemovePuzzleSubscriptions,
    RespondToCoinUpdates, RespondToPhUpdates, SendTransaction, TransactionAck,
};
use dg_xch_core::protocols::{
    ChiaMessage, ChiaMessageFilter, ChiaMessageHandler, MessageHandler, NodeType, PeerMap,
    ProtocolMessageTypes,
};
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use std::collections::HashMap;
use std::io::{Cursor, Error};
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::Arc;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

/// What the dispatch layer should do with a `NewTransaction` announcement, mirroring chia
/// `full_node_api.py::new_transaction`'s three outcomes: ignore it, pull the bundle, or ban the
/// peer. Modeling the ban as a return value (not a side effect inside the handler) keeps the
/// handler store-blind and lets loopback tests assert the outcome without a live socket — the
/// same shape as the existing `Option<RequestTransaction>` return it replaces.
pub enum TransactionAnnounceAction {
    /// Not synced, already-seen-and-consistent, fee too low, or a duplicate in-flight pull —
    /// chia's several `return None` paths. Do nothing.
    Ignore,
    /// New to us and worth fetching: send this `RequestTransaction` back to the announcer.
    Request(RequestTransaction),
    /// A protocol violation chia bans for: a zero-cost announcement, or an already-seen tx whose
    /// advertised cost/fee disagrees with our validated mempool item. Close the connection
    /// (chia `peer.close(CONSENSUS_ERROR_BAN_SECONDS)`).
    Ban,
}

// The full-node protocol surface, blind to the store and consensus: the node layer
// implements it, the handlers call it. Loopback tests back it with an in-memory map.
#[async_trait]
pub trait FullNodeApi: Send + Sync {
    async fn block_by_height(
        &self,
        height: u32,
    ) -> Option<Box<dg_xch_core::blockchain::full_block::FullBlock>>;
    // The RequestBlocks serving cap — chia `constants.MAX_BLOCK_COUNT_PER_REQUESTS`
    // (default_constants.py:77, 32 on every chia network). The range check is over INCLUSIVE ends
    // with the size compared BEFORE the +1 bump ("MAX_BLOCK_COUNT_PER_REQUESTS is off by one",
    // full_node_api.py:422-424), so a conforming node serves at most cap+1 = 33 blocks. The daemon
    // overrides this with its network constants (a consensus override can retune it); the
    // store-blind default matches chia's default.
    fn max_block_count_per_requests(&self) -> u32 {
        32
    }
    // The per-peer combined subscription cap — chia `max_subscriptions(peer)`
    // (full_node_api.py, config `max_subscriptions`). The store-blind default is chia's UNTRUSTED
    // config default (initial-config.yaml `max_subscriptions: 200000`); the daemon overrides it
    // with the trusted-tier policy (200,000 untrusted / 2,000,000 trusted). Since CHIA-4203
    // (chia `b483e59f22`) this cap is applied DURING decode of the two register arms, so it must
    // be resolvable before the message body is parsed.
    fn max_subscriptions(&self, _peer: &Bytes32, _host: Option<IpAddr>) -> u32 {
        200_000
    }
    // chia `max_subscribe_response_items(peer)` (initial-config.yaml
    // `max_subscribe_response_items: 100000`, trusted 500,000) — the decode-time list cap for
    // `RequestCoinState.coin_ids` (CHIA-4203).
    fn max_subscribe_response_items(&self, _peer: &Bytes32, _host: Option<IpAddr>) -> u32 {
        100_000
    }
    // chia 0046a3a4e (CHIA-3995): an inbound TIMELORD connection is accepted only from
    // localhost or an exempt peer network (`ChiaServer.should_accept_inbound`; the
    // `max_inbound_timelord` config limit was removed outright). The store-blind default is
    // chia's default posture — `exempt_peer_networks` ships empty, so localhost only. The
    // daemon widens it with the trusted-CIDR list (our operational analog of chia's exempt
    // networks). An unresolved host cannot be localhost — refuse.
    fn accept_inbound_timelord(&self, host: Option<IpAddr>) -> bool {
        matches!(host, Some(h) if h.is_loopback())
    }
    async fn gossip_peers(&self) -> Vec<TimestampedPeerInfo>;
    async fn on_new_peak(&self, _peer: Bytes32, _peak: NewPeak) {}
    // A NewTransaction announcement; return the action the dispatch layer takes (pull the bundle,
    // ban the peer, or ignore) — chia full_node_api.py::new_transaction.
    async fn on_new_transaction(
        &self,
        _peer: Bytes32,
        _tx: NewTransaction,
    ) -> TransactionAnnounceAction {
        TransactionAnnounceAction::Ignore
    }
    async fn transaction(&self, _id: Bytes32) -> Option<SpendBundle> {
        None
    }
    // `host` is the peer's remote IP (`SocketPeer.host`); the node impl resolves the trusted tx-queue
    // tier from it (localhost / trusted-CIDR / trusted node-id), chia `is_trusted_peer`.
    async fn on_respond_transaction(
        &self,
        _peer: Bytes32,
        _host: Option<IpAddr>,
        _tx: SpendBundle,
    ) {
    }
    // A signage-point / end-of-sub-slot announcement; return the request to pull it when the
    // node's slot state does not hold it yet, or None to ignore it.
    async fn on_new_signage_point_or_eos(
        &self,
        _peer: Bytes32,
        _ann: NewSignagePointOrEndOfSubSlot,
    ) -> Option<RequestSignagePointOrEndOfSubSlot> {
        None
    }
    // Serve a pull from the slot state: an EOS bundle when `index_from_challenge` is 0, a
    // signage point otherwise.
    async fn signage_point_or_eos(
        &self,
        _req: RequestSignagePointOrEndOfSubSlot,
    ) -> Option<SignagePointResponse> {
        None
    }
    async fn on_respond_signage_point(&self, _peer: Bytes32, _sp: RespondSignagePoint) {}
    async fn on_respond_end_of_sub_slot(&self, _peer: Bytes32, _eos: RespondEndOfSubSlot) {}
    // An unfinished-block announcement (v1: reward hash only / v2: + foliage hash); return the
    // matching request when the cache wants the block, or None to ignore it.
    async fn on_new_unfinished_block(
        &self,
        _peer: Bytes32,
        _ann: NewUnfinishedBlock,
    ) -> Option<RequestUnfinishedBlock> {
        None
    }
    async fn on_new_unfinished_block2(
        &self,
        _peer: Bytes32,
        _ann: NewUnfinishedBlock2,
    ) -> Option<RequestUnfinishedBlock2> {
        None
    }
    // Serve a cached unfinished block: v1 returns the best variant for the reward hash, v2 the
    // exact (reward hash, foliage hash) variant.
    async fn unfinished_block(&self, _reward_hash: Bytes32) -> Option<Box<UnfinishedBlock>> {
        None
    }
    async fn unfinished_block2(
        &self,
        _reward_hash: Bytes32,
        _foliage_hash: Option<Bytes32>,
    ) -> Option<Box<UnfinishedBlock>> {
        None
    }
    async fn on_respond_unfinished_block(&self, _block: Box<UnfinishedBlock>) {}
    // SERVE (chia full_node.request_compact_vdf): answer a peer's RequestCompactVDF with OUR stored
    // proof for the field only when we already hold it compact; None stays silent.
    async fn compact_vdf(&self, _req: RequestCompactVDF) -> Option<RespondCompactVDF> {
        None
    }
    // CONSUME step 1 (chia full_node.new_compact_vdf): a peer announces it holds a compact proof for
    // a block field; return the RequestCompactVDF to pull it when we still hold that field bulky, or
    // None to ignore (already compact / too recent / not ours).
    async fn on_new_compact_vdf(
        &self,
        _peer: Bytes32,
        _ann: NewCompactVDF,
    ) -> Option<RequestCompactVDF> {
        None
    }
    // CONSUME step 2 (chia full_node.add_compact_vdf): the pulled compact proof. The implementation
    // validates + swaps it into the stored block off the read path (never validate a VDF on the
    // websocket loop) and re-gossips NewCompactVDF.
    async fn on_respond_compact_vdf(&self, _peer: Bytes32, _resp: RespondCompactVDF) {}
    // CONSUME the bluebox return (chia full_node.add_compact_proof_of_time): a TIMELORD we solicited
    // with RequestCompactProofOfTime returns the compact proof for the field. Carries the same five
    // fields as RespondCompactVDF (height/header_hash/field_vdf/vdf_info/vdf_proof), so the
    // implementation validates + swaps + re-gossips through the identical consume path — off the read
    // loop, never validating a VDF on the websocket task. Default no-op for nodes that never solicit.
    async fn on_respond_compact_proof_of_time(
        &self,
        _peer: Bytes32,
        _resp: RespondCompactProofOfTime,
    ) {
    }
    // Serve a RequestMempoolTransactions: up to 100 resident items as NewTransaction
    // announcements (chia request_mempool_transactions; the peer pulls what it lacks through
    // the normal announce path).
    async fn mempool_items(&self, _filter: Vec<u8>) -> Vec<NewTransaction> {
        Vec::new()
    }
    // A RequestProofOfWeight (chia full_node_api.py request_proof_of_weight): building the proof
    // walks sub-epochs of store history, so the implementation must only QUEUE {peer, tip, id}
    // for a worker and return — never build here. The worker builds (single-flight per tip) and
    // responds through the handed peer map with the request id, so the requester's oneshot on
    // RespondProofOfWeight matches. Chia's refusals (unknown tip / short chain) also live in the
    // worker: it simply sends nothing, mirroring the reference's `return None`.
    async fn on_request_proof_of_weight(
        &self,
        _peer: Bytes32,
        _req: RequestProofOfWeight,
        _id: Option<u16>,
        _peers: PeerMap,
    ) {
    }
    async fn on_respond_peers(&self, _peers: Vec<TimestampedPeerInfo>) {}
    // FARMER→NODE (chia full_node_api.declare_proof_of_space): a harvester found a proof for a
    // signage point we announced. The implementation gates on sync, looks the SP + sub-slot up in
    // the slot state, and runs the proof through `verify_and_get_quality_string`. On accept
    // (Phase 4 increment 5) it assembles the candidate unfinished block (foliage signed with
    // placeholders + the SP signatures FROM this declare message) and returns the
    // `RequestSignedValues` for the farmer to sign the two foliage hashes; the node inserts those
    // real signatures at `on_signed_values` time and broadcasts `NewUnfinishedBlock`. Returns `None`
    // when the proof is rejected OR the candidate cannot yet be assembled from the available slot /
    // block-store state (the caller then holds the accepted proof for a later assembly).
    async fn on_declare_proof_of_space(
        &self,
        _peer: Bytes32,
        _declare: DeclareProofOfSpace,
    ) -> Option<RequestSignedValues> {
        None
    }
    // FARMER→NODE (chia full_node_api.signed_values): the farmer's signatures over the foliage we
    // asked it to sign. Consumed during block assembly (Phase 4); a no-op hook here keeps the
    // message routable end to end.
    async fn on_signed_values(&self, _peer: Bytes32, _signed: SignedValues) {}
    // TIMELORD→NODE (chia full_node_api.new_infusion_point_vdf): the infusion-point VDFs that finish one
    // of OUR cached unfinished blocks into a `FullBlock`. Assembly walks the block store and runs the
    // consensus engine (add_block → set peak), so the implementation only QUEUES the request for the
    // driver — a VDF-infused block is never assembled on the websocket read loop. chia likewise defers to
    // `full_node.new_infusion_point_vdf` under the timelord lock. Gated on sync by the implementation.
    async fn on_new_infusion_point_vdf(&self, _peer: Bytes32, _req: NewInfusionPointVDF) {}
    // TIMELORD→NODE (chia full_node_api.new_signage_point_vdf): a signage-point VDF the timelord produced.
    // chia rewraps it as a `RespondSignagePoint` and runs `respond_signage_point`; the implementation
    // routes it to the same slot-state validation inbox `on_respond_signage_point` feeds. Gated on sync.
    async fn on_new_signage_point_vdf(&self, _peer: Bytes32, _req: NewSignagePointVDF) {}
    // TIMELORD→NODE (chia full_node_api.new_end_of_sub_slot_vdf): an end-of-sub-slot the timelord produced.
    // chia ignores it when the sub-slot is already known, else runs `add_end_of_sub_slot`; the
    // implementation routes the bundle to the same slot-state validation inbox `on_respond_end_of_sub_slot`
    // feeds. Gated on sync.
    async fn on_new_end_of_sub_slot_vdf(&self, _peer: Bytes32, _req: NewEndOfSubSlotVDF) {}

    // ---- light-wallet query surface (chia wallet_protocol; served against the coin/block store) --------

    // WALLET→NODE (chia full_node_api.request_puzzle_solution): the (puzzle, solution) of a coin spent
    // at `height`, recovered by re-running that block's generator. `None` maps to a
    // `RejectPuzzleSolution` on the wire (chia's reject on unknown coin / wrong height / no generator).
    async fn puzzle_solution(
        &self,
        _coin_name: Bytes32,
        _height: u32,
    ) -> Option<PuzzleSolutionResponse> {
        None
    }

    // WALLET→NODE (chia full_node_api.send_transaction, code 48): a spend bundle submitted for
    // mempool admission. ALWAYS answered with a TransactionAck — chia never silently drops a
    // submit (the reference wallet and Sage both block on this ack). The implementation gates on
    // sync, shares the push_tx admission seam (validate → admit → announce), and maps the outcome
    // to chia's (MempoolInclusionStatus, Err.name): SUCCESS(1)/PENDING(2)/FAILED(3). The
    // store-blind default acks chia's not-synced reject — a node that cannot validate must not
    // claim success (full_node.py:2882-2885).
    async fn send_transaction(&self, _peer: Bytes32, tx: SendTransaction) -> TransactionAck {
        TransactionAck {
            txid: tx.transaction.name().unwrap_or_default(),
            status: TXStatus::FAILED,
            error: Some("NO_TRANSACTIONS_WHILE_SYNCING".to_string()),
        }
    }

    // WALLET→NODE (chia full_node_api.request_block_header): the HeaderBlock at a height.
    async fn block_header(&self, _height: u32) -> BlockHeaderReply {
        BlockHeaderReply::Silent
    }

    // WALLET→NODE (chia full_node_api.request_header_blocks, code 60): header blocks in [start, end].
    async fn header_blocks(&self, _start_height: u32, _end_height: u32) -> HeaderBlocksReply {
        HeaderBlocksReply::Silent
    }

    // WALLET→NODE (chia full_node_api.request_block_headers, code 86): header blocks in [start, end].
    // The default rejects (a store-blind impl serves nothing); the real node overrides it.
    async fn block_headers(
        &self,
        start_height: u32,
        end_height: u32,
        _return_filter: bool,
    ) -> BlockHeadersReply {
        BlockHeadersReply::Reject(RejectBlockHeaders {
            start_height,
            end_height,
        })
    }

    // WALLET→NODE (chia full_node_api.request_additions): coins created at a block, grouped by puzzle
    // hash. The default rejects; the real node overrides it.
    async fn additions(&self, req: RequestAdditions) -> AdditionsReply {
        AdditionsReply::Reject(RejectAdditionsRequest {
            height: req.height,
            header_hash: req.header_hash.unwrap_or_default(),
        })
    }

    // WALLET→NODE (chia full_node_api.request_removals): coins spent at a block. The default rejects;
    // the real node overrides it.
    async fn removals(&self, req: RequestRemovals) -> RemovalsReply {
        RemovalsReply::Reject(RejectRemovalsRequest {
            height: req.height,
            header_hash: req.header_hash,
        })
    }

    // WALLET→NODE (chia full_node_api.request_children): the coin states of every child of a coin
    // (spent and unspent). An empty vec is a valid answer (no children yet); chia always responds.
    async fn children(&self, _coin_name: Bytes32) -> Vec<CoinState> {
        Vec::new()
    }

    // WALLET→NODE (chia full_node_api.register_for_ph_updates): subscribe the peer to puzzle-hash coin
    // updates AND return the initial matching CoinState set. The default is store-blind (empty state, no
    // subscription); the real node overrides it, registering interest and reading the initial set from
    // the coin/hint store.
    async fn register_for_ph_updates(
        &self,
        _peer: Bytes32,
        _host: Option<IpAddr>,
        req: RegisterForPhUpdates,
    ) -> PhRegistration {
        PhRegistration {
            response: RespondToPhUpdates {
                puzzle_hashes: req.puzzle_hashes,
                min_height: req.min_height,
                coin_states: Vec::new(),
            },
            receiver: None,
        }
    }

    // WALLET→NODE (chia full_node_api.register_for_coin_updates): subscribe the peer to coin-id updates
    // AND return the initial matching CoinState set. Default store-blind.
    async fn register_for_coin_updates(
        &self,
        _peer: Bytes32,
        _host: Option<IpAddr>,
        req: RegisterForCoinUpdates,
    ) -> CoinRegistration {
        CoinRegistration {
            response: RespondToCoinUpdates {
                coin_ids: req.coin_ids,
                min_height: req.min_height,
                coin_states: Vec::new(),
            },
            receiver: None,
        }
    }

    // ---- the modern wallet-sync surface (chia wallet_protocol codes 94-103, the Sage sync loop) ----

    // WALLET→NODE (chia full_node_api.request_puzzle_state, code 98): the PAGED spent+unspent coin
    // history of a set of puzzle hashes (plus hinted coins), with a reorg-consistency check against
    // the requester's previous peak and an optional subscribe-on-finish side effect. The store-blind
    // default rejects REORG — a node that cannot resolve heights against a chain must not claim a
    // consistent answer (the same verdict chia reaches with no peak, full_node_api.py:2055-2058).
    async fn puzzle_state(
        &self,
        _peer: Bytes32,
        _host: Option<IpAddr>,
        _req: RequestPuzzleState,
    ) -> PuzzleStateReply {
        PuzzleStateReply::Reject(RejectStateReason::REORG)
    }

    // WALLET→NODE (chia full_node_api.request_coin_state, code 101): the coin states of a set of
    // coin ids above the requester's previous peak, same reorg-consistency check, optional
    // subscribe side effect. Store-blind default rejects REORG.
    async fn coin_state(
        &self,
        _peer: Bytes32,
        _host: Option<IpAddr>,
        _req: RequestCoinState,
    ) -> CoinStateReply {
        CoinStateReply::Reject(RejectStateReason::REORG)
    }

    // WALLET→NODE (chia full_node_api.request_fee_estimates, code 89): a fee-rate estimate for each
    // requested epoch timestamp. chia ALWAYS answers one FeeEstimate per requested time — a node
    // with no history returns rate 0, never an error group (full_node_api.py:1940-1955). The
    // store-blind default returns the floor (rate 0) for every requested time; the real node reads
    // the mempool's fee estimator.
    async fn fee_estimates(&self, req: RequestFeeEstimates) -> FeeEstimateGroup {
        FeeEstimateGroup {
            error: None,
            estimates: req
                .time_targets
                .iter()
                .map(|&t| FeeEstimate {
                    error: None,
                    time_target: t,
                    estimated_fee_rate: FeeRate {
                        mojos_per_clvm_cost: 0,
                    },
                })
                .collect(),
        }
    }

    // WALLET→NODE (chia full_node_api.request_remove_puzzle_subscriptions, code 94): drop the
    // peer's puzzle-hash subscriptions — `None` = ALL (returning the prior set), `Some` = the
    // listed subset (returning what was actually removed) (full_node_api.py:1961-1975).
    async fn remove_puzzle_subscriptions(
        &self,
        _peer: Bytes32,
        _puzzle_hashes: Option<Vec<Bytes32>>,
    ) -> Vec<Bytes32> {
        Vec::new()
    }

    // WALLET→NODE (chia full_node_api.request_remove_coin_subscriptions, code 96): the coin-id
    // counterpart (full_node_api.py:1981-1995).
    async fn remove_coin_subscriptions(
        &self,
        _peer: Bytes32,
        _coin_ids: Option<Vec<Bytes32>>,
    ) -> Vec<Bytes32> {
        Vec::new()
    }

    // NODE→WALLET greeting: the current peak as chia full_node.py on_connect (:1000-1008) sends it
    // to a WALLET-type peer the moment its handshake completes — fork_point_with_previous_peak is
    // the peak height itself on connect. `None` (store-blind default / no peak yet) sends nothing,
    // chia's `peak_full is None` posture. Sage drops any peer that stays silent for 2s after the
    // handshake (sage-wallet peer_discovery.rs try_add_peer, options.rs initial_peak=2s), so this
    // greeting is what keeps a wallet connection alive at all.
    async fn wallet_peak(&self) -> Option<NewPeakWallet> {
        None
    }
    // NODE→FULL_NODE greeting: the current peak as chia full_node.py on_connect (:989-998) sends
    // it to a new FULL_NODE peer — `NewPeak(peak.header_hash, peak.height, peak.weight,
    // peak.height, unfinished_reward_block_hash)`, fork point = the peak height itself on connect
    // (same convention as the wallet greeting). `None` (no peak / blind api) sends nothing.
    async fn full_node_peak(&self) -> Option<NewPeak> {
        None
    }
    // NODE→TIMELORD greeting: chia full_node.py on_connect (:1009-1010) `send_peak_to_timelords`
    // — a fresh timelord starts infusing on top of our peak immediately instead of idling until
    // the next peak advance. `None` (no peak, or the peak's ancestry cannot support the
    // difficulty/challenge walks) sends nothing. Boxed: NewPeakTimelord carries a full
    // RewardChainBlock and would bloat every blind-api vtable copy otherwise.
    async fn timelord_peak(&self) -> Option<Box<NewPeakTimelord>> {
        None
    }
    // Mempool sync on connect (chia full_node.py on_connect :967-982): when WE are synced, a new
    // FULL_NODE peer is sent `RequestMempoolTransactions` carrying the BIP158 filter over OUR
    // mempool item ids (chia mempool_manager.get_filter, :436-445) — the peer answers with the
    // transactions we are missing (new peers announce via NewTransaction, pre-2.6.0 peers push
    // RespondTransaction directly; both land in the normal admission seam). `None` = not synced —
    // nothing is requested (chia's `if synced and peak_height is not None` gate).
    async fn mempool_sync_filter(&self) -> Option<Vec<u8>> {
        None
    }
}

// Bridge a subscribed peer's bounded `CoinStateUpdate` receiver to the wire: a per-peer task that
// forwards every update the peak/reorg path pushes into the channel. One task per peer (spawned only on
// the FIRST registration, when the api hands back a receiver). It ENDS on its own when the channel
// closes — the daemon's disconnect reconciliation drops the `WalletNotifier` subscriber, which drops the
// `Sender`, so `recv()` returns `None`; a socket send error ends it too. No leaked task, no unbounded
// spawn (bound every spawn — this one is bounded by the subscriber cap and self-terminating).
fn spawn_coin_state_forwarder(
    counters: Arc<NetCounters>,
    peers: PeerMap,
    peer_id: Bytes32,
    mut rx: tokio::sync::mpsc::Receiver<CoinStateUpdate>,
    version: ChiaProtocolVersion,
) {
    tokio::spawn(async move {
        while let Some(update) = rx.recv().await {
            if send(
                &counters,
                &peers,
                &peer_id,
                ProtocolMessageTypes::CoinStateUpdate,
                &update,
                None,
                version,
            )
            .await
            .is_err()
            {
                break;
            }
        }
    });
}

// What `signage_point_or_eos` serves back — the two message types a
// RequestSignagePointOrEndOfSubSlot can be answered with.
pub enum SignagePointResponse {
    SignagePoint(Box<RespondSignagePoint>),
    EndOfSubSlot(Box<RespondEndOfSubSlot>),
}

// ---- light-wallet query replies (chia full_node_api.py wallet handlers) --------------------------------
// Each wallet request is answered with EXACTLY ONE of a Respond* / Reject* message, or (for the two chia
// handlers that `return None` on a missing body / bad range) no reply at all. Modeling the choice as an
// enum keeps the FullNodeApi implementation store-blind and lets the dispatch layer own the wire send —
// the same shape as `SignagePointResponse` above.

/// `request_block_header` (chia full_node_api.py:1322): the `HeaderBlock` at a height, a
/// `RejectHeaderRequest` when the height is not in the main chain, or silence when the record exists
/// but the block body does not (chia `return None`).
pub enum BlockHeaderReply {
    Respond(Box<HeaderBlock>),
    Reject(u32),
    Silent,
}

/// `request_header_blocks` (chia full_node_api.py:1670, the DEPRECATED shape, code 60): the header
/// blocks in `[start, end]`, a `RejectHeaderBlocks` when a height in the range is unknown, or silence
/// on a bad range (chia `return None`).
pub enum HeaderBlocksReply {
    Respond(Box<RespondHeaderBlocks>),
    Reject(RejectHeaderBlocks),
    Silent,
}

/// `request_block_headers` (chia full_node_api.py:1617, the streamed shape, code 86): the header
/// blocks in `[start, end]` or a `RejectBlockHeaders` (bad range / missing body). This handler never
/// stays silent — a bad range is a `Reject`.
pub enum BlockHeadersReply {
    Respond(Box<RespondBlockHeaders>),
    Reject(RejectBlockHeaders),
}

/// `request_additions` (chia full_node_api.py:1372): coins created at a block grouped by puzzle hash,
/// or a `RejectAdditionsRequest` (fork / too many hashes / unknown height).
pub enum AdditionsReply {
    Respond(Box<RespondAdditions>),
    Reject(RejectAdditionsRequest),
}

/// `request_removals` (chia full_node_api.py:1455): coins spent at a block, or a
/// `RejectRemovalsRequest` (not a tx block / fork / height mismatch).
pub enum RemovalsReply {
    Respond(Box<RespondRemovals>),
    Reject(RejectRemovalsRequest),
}

/// The result of a `RegisterForPhUpdates` (chia full_node_api.py:1805): the initial matching
/// `CoinState` set the wallet gets synchronously, plus — on the peer's FIRST registration — the
/// bounded delivery receiver the dispatch layer bridges to the socket as a live `CoinStateUpdate`
/// forwarder. `receiver` is `None` on a repeat registration (one channel per peer).
pub struct PhRegistration {
    pub response: RespondToPhUpdates,
    pub receiver: Option<tokio::sync::mpsc::Receiver<CoinStateUpdate>>,
}

/// The result of a `RegisterForCoinUpdates` (chia full_node_api.py:1871). See [`PhRegistration`].
pub struct CoinRegistration {
    pub response: RespondToCoinUpdates,
    pub receiver: Option<tokio::sync::mpsc::Receiver<CoinStateUpdate>>,
}

/// `request_puzzle_state` (chia full_node_api.py:2002, code 98): one page of puzzle-hash coin
/// states, or a `RejectPuzzleState` carrying the `RejectStateReason` (REORG on a
/// previous-peak mismatch / unresolvable chain, EXCEEDED_SUBSCRIPTION_LIMIT on an over-cap
/// subscribe). The `Respond` arm carries the peer's delivery receiver on its FIRST
/// subscribe-on-finish (the dispatch layer bridges it to the socket, exactly like
/// [`PhRegistration`]).
pub enum PuzzleStateReply {
    Respond(
        Box<RespondPuzzleState>,
        Option<tokio::sync::mpsc::Receiver<CoinStateUpdate>>,
    ),
    Reject(RejectStateReason),
}

/// `request_coin_state` (chia full_node_api.py:2085, code 101). See [`PuzzleStateReply`].
pub enum CoinStateReply {
    Respond(
        Box<RespondCoinState>,
        Option<tokio::sync::mpsc::Receiver<CoinStateUpdate>>,
    ),
    Reject(RejectStateReason),
}

// Peer-link traffic counters, shared by every handler map and the daemon's broadcast paths (one
// per node). Message counts are per-type (the gossip-health signal chia-exporter lacks — it only
// exposes connection counts); byte totals cover the whole link. Mutex-per-count is fine at
// protocol rates (tens of messages a second, far from contention).
#[derive(Default)]
pub struct NetCounters {
    pub messages_in: std::sync::Mutex<HashMap<&'static str, u64>>,
    pub messages_out: std::sync::Mutex<HashMap<&'static str, u64>>,
    pub bytes_in: std::sync::atomic::AtomicU64,
    pub bytes_out: std::sync::atomic::AtomicU64,
}

// The stable label for a message type (lowercase snake case, chia's own protocol names).
fn msg_label(t: ProtocolMessageTypes) -> &'static str {
    match t {
        ProtocolMessageTypes::Handshake => "handshake",
        ProtocolMessageTypes::NewPeak => "new_peak",
        ProtocolMessageTypes::NewTransaction => "new_transaction",
        ProtocolMessageTypes::RequestTransaction => "request_transaction",
        ProtocolMessageTypes::RespondTransaction => "respond_transaction",
        ProtocolMessageTypes::RequestBlock => "request_block",
        ProtocolMessageTypes::RespondBlock => "respond_block",
        ProtocolMessageTypes::RequestBlocks => "request_blocks",
        ProtocolMessageTypes::RespondBlocks => "respond_blocks",
        ProtocolMessageTypes::RejectBlock => "reject_block",
        ProtocolMessageTypes::RejectBlocks => "reject_blocks",
        ProtocolMessageTypes::NewSignagePointOrEndOfSubSlot => {
            "new_signage_point_or_end_of_sub_slot"
        }
        ProtocolMessageTypes::RequestSignagePointOrEndOfSubSlot => {
            "request_signage_point_or_end_of_sub_slot"
        }
        ProtocolMessageTypes::RespondSignagePoint => "respond_signage_point",
        ProtocolMessageTypes::RespondEndOfSubSlot => "respond_end_of_sub_slot",
        ProtocolMessageTypes::NewUnfinishedBlock => "new_unfinished_block",
        ProtocolMessageTypes::NewUnfinishedBlock2 => "new_unfinished_block2",
        // The unfinished-block PULLS were missing here and surfaced as "other" on the live
        // metrics — which read exactly like "the node never pulls" during the announce-pull
        // triage. Every type the node sends or serves must carry its own label.
        ProtocolMessageTypes::RequestUnfinishedBlock => "request_unfinished_block",
        ProtocolMessageTypes::RequestUnfinishedBlock2 => "request_unfinished_block2",
        ProtocolMessageTypes::RespondUnfinishedBlock => "respond_unfinished_block",
        ProtocolMessageTypes::NewCompactVdf => "new_compact_vdf",
        ProtocolMessageTypes::RequestCompactVdf => "request_compact_vdf",
        ProtocolMessageTypes::RespondCompactVdf => "respond_compact_vdf",
        ProtocolMessageTypes::RequestPeers => "request_peers",
        ProtocolMessageTypes::RespondPeers => "respond_peers",
        ProtocolMessageTypes::RequestMempoolTransactions => "request_mempool_transactions",
        ProtocolMessageTypes::RequestProofOfWeight => "request_proof_of_weight",
        ProtocolMessageTypes::RespondProofOfWeight => "respond_proof_of_weight",
        ProtocolMessageTypes::NewSignagePoint => "new_signage_point",
        ProtocolMessageTypes::DeclareProofOfSpace => "declare_proof_of_space",
        ProtocolMessageTypes::RequestSignedValues => "request_signed_values",
        ProtocolMessageTypes::SignedValues => "signed_values",
        ProtocolMessageTypes::NewInfusionPointVdf => "new_infusion_point_vdf",
        ProtocolMessageTypes::NewSignagePointVdf => "new_signage_point_vdf",
        ProtocolMessageTypes::NewEndOfSubSlotVdf => "new_end_of_sub_slot_vdf",
        ProtocolMessageTypes::RequestCompactProofOfTime => "request_compact_proof_of_time",
        ProtocolMessageTypes::RespondCompactProofOfTime => "respond_compact_proof_of_time",
        // Light-wallet query surface (in + out labels for the gossip-health signal).
        ProtocolMessageTypes::SendTransaction => "send_transaction",
        ProtocolMessageTypes::TransactionAck => "transaction_ack",
        ProtocolMessageTypes::RequestPuzzleSolution => "request_puzzle_solution",
        ProtocolMessageTypes::RespondPuzzleSolution => "respond_puzzle_solution",
        ProtocolMessageTypes::RejectPuzzleSolution => "reject_puzzle_solution",
        ProtocolMessageTypes::RequestBlockHeader => "request_block_header",
        ProtocolMessageTypes::RespondBlockHeader => "respond_block_header",
        ProtocolMessageTypes::RejectHeaderRequest => "reject_header_request",
        ProtocolMessageTypes::RequestHeaderBlocks => "request_header_blocks",
        ProtocolMessageTypes::RespondHeaderBlocks => "respond_header_blocks",
        ProtocolMessageTypes::RejectHeaderBlocks => "reject_header_blocks",
        ProtocolMessageTypes::RequestBlockHeaders => "request_block_headers",
        ProtocolMessageTypes::RespondBlockHeaders => "respond_block_headers",
        ProtocolMessageTypes::RejectBlockHeaders => "reject_block_headers",
        ProtocolMessageTypes::RequestAdditions => "request_additions",
        ProtocolMessageTypes::RespondAdditions => "respond_additions",
        ProtocolMessageTypes::RejectAdditionsRequest => "reject_additions_request",
        ProtocolMessageTypes::RequestRemovals => "request_removals",
        ProtocolMessageTypes::RespondRemovals => "respond_removals",
        ProtocolMessageTypes::RejectRemovalsRequest => "reject_removals_request",
        ProtocolMessageTypes::RequestChildren => "request_children",
        ProtocolMessageTypes::RespondChildren => "respond_children",
        ProtocolMessageTypes::RegisterInterestInPuzzleHash => "register_interest_in_puzzle_hash",
        ProtocolMessageTypes::RespondToPhUpdate => "respond_to_ph_update",
        ProtocolMessageTypes::RegisterInterestInCoin => "register_interest_in_coin",
        ProtocolMessageTypes::RespondToCoinUpdate => "respond_to_coin_update",
        ProtocolMessageTypes::CoinStateUpdate => "coin_state_update",
        ProtocolMessageTypes::NewPeakWallet => "new_peak_wallet",
        ProtocolMessageTypes::RequestPuzzleState => "request_puzzle_state",
        ProtocolMessageTypes::RespondPuzzleState => "respond_puzzle_state",
        ProtocolMessageTypes::RejectPuzzleState => "reject_puzzle_state",
        ProtocolMessageTypes::RequestCoinState => "request_coin_state",
        ProtocolMessageTypes::RespondCoinState => "respond_coin_state",
        ProtocolMessageTypes::RejectCoinState => "reject_coin_state",
        ProtocolMessageTypes::RequestRemovePuzzleSubscriptions => {
            "request_remove_puzzle_subscriptions"
        }
        ProtocolMessageTypes::RespondRemovePuzzleSubscriptions => {
            "respond_remove_puzzle_subscriptions"
        }
        ProtocolMessageTypes::RequestRemoveCoinSubscriptions => "request_remove_coin_subscriptions",
        ProtocolMessageTypes::RespondRemoveCoinSubscriptions => "respond_remove_coin_subscriptions",
        ProtocolMessageTypes::RequestFeeEstimates => "request_fee_estimates",
        ProtocolMessageTypes::RespondFeeEstimates => "respond_fee_estimates",
        _ => "other",
    }
}

impl NetCounters {
    pub fn count_in(&self, t: ProtocolMessageTypes, bytes: usize) {
        *self
            .messages_in
            .lock()
            .expect("net counter lock")
            .entry(msg_label(t))
            .or_insert(0) += 1;
        self.bytes_in
            .fetch_add(bytes as u64, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn count_out(&self, t: ProtocolMessageTypes, bytes: usize) {
        *self
            .messages_out
            .lock()
            .expect("net counter lock")
            .entry(msg_label(t))
            .or_insert(0) += 1;
        self.bytes_out
            .fetch_add(bytes as u64, std::sync::atomic::Ordering::Relaxed);
    }
}

async fn peer_version(peers: &PeerMap, peer_id: &Bytes32) -> ChiaProtocolVersion {
    if let Some(peer) = peers.read().await.get(peer_id).cloned() {
        *peer.protocol_version.read().await
    } else {
        ChiaProtocolVersion::default()
    }
}

// The peer's captured remote IP (`SocketPeer.host`) — the host input to chia `is_trusted_peer`. `None`
// when the peer is gone from the map or its remote addr was never resolved (an outbound dial to an
// unresolved name); a `None` host simply cannot grant host-based trust (node-id trust still applies).
async fn peer_host(peers: &PeerMap, peer_id: &Bytes32) -> Option<IpAddr> {
    peers.read().await.get(peer_id).and_then(|peer| peer.host)
}

// The peer's handshake-recorded node type (chia `WSChiaConnection.connection_type`). `Unknown`
// when the peer is gone from the map or never completed a handshake.
async fn peer_node_type(peers: &PeerMap, peer_id: &Bytes32) -> NodeType {
    if let Some(peer) = peers.read().await.get(peer_id) {
        *peer.node_type.read().await
    } else {
        NodeType::Unknown
    }
}

/// Enforce chia's sender-type rule for the timelord-class inbound messages
/// (`protocol_message_type_to_node_type.py`: new_infusion_point_vdf / new_signage_point_vdf /
/// new_end_of_sub_slot_vdf / respond_compact_proof_of_time are sent only by
/// `{NodeType.TIMELORD}`). A mismatch is chia `_api_call`'s
/// `ProtocolError(INVALID_PROTOCOL_MESSAGE)` → close with the short (10 s) protocol ban. The
/// check runs BEFORE the body is decoded, exactly where chia checks `allowed_senders`.
/// Returns `true` when the sender is a timelord and dispatch may proceed.
async fn require_timelord_sender(
    peers: &PeerMap,
    peer_id: &Bytes32,
    msg_type: ProtocolMessageTypes,
) -> Result<bool, Error> {
    if peer_node_type(peers, peer_id).await == NodeType::Timelord {
        return Ok(true);
    }
    log::warn!(
        "API call type mismatch: peer {peer_id} is not a TIMELORD but sent {msg_type:?}; \
         closing with the short protocol ban (chia allowed_senders check)"
    );
    close_peer(peers, peer_id, Some(BanCause::InvalidProtocol)).await?;
    Ok(false)
}

async fn send<T: ChiaSerialize>(
    counters: &NetCounters,
    peers: &PeerMap,
    peer_id: &Bytes32,
    msg_type: ProtocolMessageTypes,
    body: &T,
    id: Option<u16>,
    version: ChiaProtocolVersion,
) -> Result<(), Error> {
    if let Some(peer) = peers.read().await.get(peer_id).cloned() {
        let bytes = ChiaMessage::new(msg_type, version, body, id)?.to_bytes(version)?;
        counters.count_out(msg_type, bytes.len());
        peer.websocket
            .write()
            .await
            .send(Message::Binary(bytes.into()))
            .await?;
    }
    Ok(())
}

fn decode<T: ChiaSerialize>(msg: &ChiaMessage, version: ChiaProtocolVersion) -> Result<T, Error> {
    T::from_bytes(&mut Cursor::new(msg.data.as_slice()), version)
}

/// chia `CoinStore.MAX_PUZZLE_HASH_BATCH_SIZE = SQLITE_MAX_VARIABLE_NUMBER - 10 = 32690`
/// (coin_store.py:588) — the decode-time list cap CHIA-4203 (chia `b483e59f22`) wires onto
/// `request_puzzle_state.puzzle_hashes`. Kept numerically in sync with
/// `dg_xch_stores::traits::MAX_PUZZLE_HASH_BATCH_SIZE` (this crate is store-blind, so the value is
/// mirrored here; a cross-crate equality test in `full-node` pins the two together).
pub const MAX_PUZZLE_HASH_BATCH_SIZE: u32 = 32_700 - 10;

/// Close a misbehaving peer's connection (chia `peer.close(ban_time)`). All three halves of chia's
/// close: (1) enter the peer's REMOTE host into the timed ban list for `cause`'s duration, so a
/// reconnect within the window is refused at the accept path — read off the peer's injected
/// registry + captured host (`None` on a link without either is a no-op, e.g. an outbound client);
/// (2) evict the peer from the shared map NOW, so the ban is immediate and the map stays bounded
/// even if the socket teardown lags; (3) send a WebSocket Close frame to tear the connection down.
/// Removing the map entry first means the very next handler dispatch for this peer finds nothing to
/// serve. A missing peer (already gone) is a no-op success. `cause == None` closes without banning
/// (a graceful, non-punitive teardown).
async fn close_peer(
    peers: &PeerMap,
    peer_id: &Bytes32,
    cause: Option<BanCause>,
) -> Result<(), Error> {
    let peer = peers.write().await.remove(peer_id);
    if let Some(peer) = peer {
        if let (Some(cause), Some(bans), Some(host)) = (cause, peer.bans.as_ref(), peer.host) {
            bans.ban(host, cause);
        }
        peer.websocket.write().await.close(None).await?;
    }
    Ok(())
}

pub struct FullNodeHandler {
    pub api: Arc<dyn FullNodeApi>,
    pub network_id: String,
    pub server_port: u16,
    // Reply to an inbound Handshake with our own (server role). False on an outbound link where WE
    // initiated the handshake via WsClient::perform_handshake — receiving the peer's reply must record
    // the negotiated version but NOT emit a second handshake (which the peer would reject/close on).
    pub respond_handshake: bool,
    // Per-link traffic counters; the daemon shares one instance across every handler map so
    // /metrics sees the whole node's I/O.
    pub counters: Arc<NetCounters>,
}

impl FullNodeHandler {
    async fn dispatch(
        &self,
        msg: &ChiaMessage,
        peer_id: &Bytes32,
        peers: &PeerMap,
        version: ChiaProtocolVersion,
    ) -> Result<(), Error> {
        match msg.msg_type {
            ProtocolMessageTypes::Handshake => {
                let hs = decode::<Handshake>(msg, version)?;
                let negotiated = ChiaProtocolVersion::from_str(&hs.protocol_version)
                    .expect("ChiaProtocolVersion::from_str is Infallible");
                if let Some(peer) = peers.read().await.get(peer_id).cloned() {
                    *peer.node_type.write().await = NodeType::from(hs.node_type);
                    *peer.protocol_version.write().await = negotiated;
                    // Record the peer's advertised capabilities so the read loop's inbound rate
                    // limiter selects the v1/v2 numbers this peer negotiated (chia
                    // `get_rate_limits_to_use`). Server-role links learn caps here; outbound links
                    // record them in `WsClient::build` after the oneshot handshake.
                    *peer.capabilities.write().await = hs.capabilities.clone();
                }
                if !self.respond_handshake {
                    // Outbound link: we already initiated the handshake; only record the negotiated
                    // version above. Emitting a reply here would be a duplicate mid-stream handshake.
                    return Ok(());
                }
                // RATE_LIMITS_V3 responder mirror (chia a1b12d321, ws_connection.py
                // `perform_handshake`): "If the peer advertises v3, let's advertise v3 as well" —
                // the responder appends the capability to its reply and both sides exchange
                // ConfigureWindowSizes. Our outbound dials keep the default set (chia 2.7.1's
                // `_capabilities` has no v3), so only inbound-initiated links ever activate.
                let peer_is_v3 = peer_supports_v3(&hs.capabilities);
                let mut reply_caps: dg_xch_core::protocols::shared::Capabilities = CAPABILITIES
                    .iter()
                    .map(|e| (e.0, e.1.to_string()))
                    .collect();
                if peer_is_v3 {
                    reply_caps.push((Capability::RateLimitsV3 as u16, "1".to_string()));
                }
                let reply = Handshake {
                    network_id: self.network_id.clone(),
                    protocol_version: negotiated.to_string(),
                    software_version: dg_xch_clients::websocket::version(),
                    server_port: self.server_port,
                    node_type: NodeType::FullNode as u8,
                    capabilities: reply_caps,
                };
                send(
                    &self.counters,
                    peers,
                    peer_id,
                    ProtocolMessageTypes::Handshake,
                    &reply,
                    msg.id,
                    negotiated,
                )
                .await?;
                if peer_is_v3 {
                    // Immediately after the handshake: our ConfigureWindowSizes (chia sequence —
                    // each side sends its settings and validates the peer's). Activation completes
                    // when the peer's configure arrives (the dispatch arm below).
                    if let Some(peer) = peers.read().await.get(peer_id) {
                        peer.v3.offer();
                    }
                    send(
                        &self.counters,
                        peers,
                        peer_id,
                        ProtocolMessageTypes::ConfigureWindowSizes,
                        &configure_message(),
                        None,
                        negotiated,
                    )
                    .await?;
                }
                // chia 0046a3a4e (CHIA-3995): an inbound TIMELORD is accepted only from
                // localhost / an exempt network. The refusal happens right after the handshake
                // completes and BEFORE any greeting, and closes WITHOUT banning — chia's
                // `should_accept_inbound` failure path logs "Inbound limit reached" and calls
                // `connection.close()`, never `ban_peer`.
                if NodeType::from(hs.node_type) == NodeType::Timelord {
                    let host = peer_host(peers, peer_id).await;
                    if !self.api.accept_inbound_timelord(host) {
                        log::info!(
                            "Not accepting inbound TIMELORD connection from {host:?}: \
                             localhost/exempt networks only (chia CHIA-3995)"
                        );
                        return close_peer(peers, peer_id, None).await;
                    }
                }
                // chia full_node.py on_connect (:967-1010): greet the new peer by type the moment
                // its handshake completes. No peak (empty store / blind api) sends nothing —
                // chia's `peak_full is None` posture.
                match NodeType::from(hs.node_type) {
                    // :991-998 — NewPeak of the current peak, then (:967-982, when synced) the
                    // mempool-sync request carrying OUR BIP158 filter; the peer answers with the
                    // transactions we are missing (NewTransaction from 2.6.0+ peers,
                    // RespondTransaction from older ones — both flow through the normal
                    // admission seam).
                    NodeType::FullNode => {
                        if let Some(peak) = self.api.full_node_peak().await {
                            send(
                                &self.counters,
                                peers,
                                peer_id,
                                ProtocolMessageTypes::NewPeak,
                                &peak,
                                None,
                                negotiated,
                            )
                            .await?;
                        }
                        if let Some(filter) = self.api.mempool_sync_filter().await {
                            send(
                                &self.counters,
                                peers,
                                peer_id,
                                ProtocolMessageTypes::RequestMempoolTransactions,
                                &RequestMempoolTransactions { filter },
                                None,
                                negotiated,
                            )
                            .await?;
                        }
                    }
                    // :1000-1008 — a WALLET peer is greeted with the current peak as
                    // NewPeakWallet. Sage drops any peer that has not produced exactly this
                    // message within 2s of connecting (sage-wallet peer_discovery.rs
                    // try_add_peer, options.rs initial_peak) — without it the whole wallet query
                    // surface is unreachable.
                    NodeType::Wallet => {
                        if let Some(peak) = self.api.wallet_peak().await {
                            send(
                                &self.counters,
                                peers,
                                peer_id,
                                ProtocolMessageTypes::NewPeakWallet,
                                &peak,
                                None,
                                negotiated,
                            )
                            .await?;
                        }
                    }
                    // :1009-1010 — send_peak_to_timelords: a fresh timelord starts infusing on
                    // top of our peak immediately.
                    NodeType::Timelord => {
                        if let Some(peak) = self.api.timelord_peak().await {
                            send(
                                &self.counters,
                                peers,
                                peer_id,
                                ProtocolMessageTypes::NewPeakTimelord,
                                &*peak,
                                None,
                                negotiated,
                            )
                            .await?;
                        }
                    }
                    _ => {}
                }
                Ok(())
            }
            ProtocolMessageTypes::NewPeak => {
                self.api.on_new_peak(*peer_id, decode(msg, version)?).await;
                Ok(())
            }
            ProtocolMessageTypes::RequestBlock => {
                let req = decode::<RequestBlock>(msg, version)?;
                match self.api.block_by_height(req.height).await {
                    Some(mut block) => {
                        // chia full_node_api.py:415-416: a headers-only pull
                        // (include_transaction_block=false) strips ONLY transactions_generator
                        // (`block.replace(transactions_generator=None)`); transactions_info and
                        // transactions_generator_ref_list are served untouched.
                        if !req.include_transaction_block {
                            block.transactions_generator = None;
                        }
                        let resp = RespondBlock { block: *block };
                        send(
                            &self.counters,
                            peers,
                            peer_id,
                            ProtocolMessageTypes::RespondBlock,
                            &resp,
                            msg.id,
                            version,
                        )
                        .await
                    }
                    None => {
                        let rej = RejectBlock { height: req.height };
                        send(
                            &self.counters,
                            peers,
                            peer_id,
                            ProtocolMessageTypes::RejectBlock,
                            &rej,
                            msg.id,
                            version,
                        )
                        .await
                    }
                }
            }
            ProtocolMessageTypes::RequestBlocks => {
                let req = decode::<RequestBlocks>(msg, version)?;
                // chia full_node_api.py:425-431: an inverted range (end < start) or one wider than
                // MAX_BLOCK_COUNT_PER_REQUESTS rejects BEFORE the store is touched — without this
                // cap a hostile peer requests the whole chain into one RespondBlocks (memory /
                // serialization self-DoS). chia compares `end - start > cap` on the INCLUSIVE
                // range (the documented off-by-one: a cap-of-32 node serves up to 33 blocks); the
                // short-circuit `end < start` guard keeps the u32 subtraction safe.
                if req.end_height < req.start_height
                    || req.end_height - req.start_height > self.api.max_block_count_per_requests()
                {
                    let rej = RejectBlocks {
                        start_height: req.start_height,
                        end_height: req.end_height,
                    };
                    return send(
                        &self.counters,
                        peers,
                        peer_id,
                        ProtocolMessageTypes::RejectBlocks,
                        &rej,
                        msg.id,
                        version,
                    )
                    .await;
                }
                let mut blocks = Vec::new();
                for h in req.start_height..=req.end_height {
                    if let Some(mut b) = self.api.block_by_height(h).await {
                        // chia full_node_api.py:438-451: headers-only range pulls strip ONLY
                        // transactions_generator per block, exactly like the single-block arm.
                        if !req.include_transaction_block {
                            b.transactions_generator = None;
                        }
                        blocks.push(*b);
                    } else {
                        let rej = RejectBlocks {
                            start_height: req.start_height,
                            end_height: req.end_height,
                        };
                        return send(
                            &self.counters,
                            peers,
                            peer_id,
                            ProtocolMessageTypes::RejectBlocks,
                            &rej,
                            msg.id,
                            version,
                        )
                        .await;
                    }
                }
                let resp = RespondBlocks {
                    start_height: req.start_height,
                    end_height: req.end_height,
                    blocks,
                };
                send(
                    &self.counters,
                    peers,
                    peer_id,
                    ProtocolMessageTypes::RespondBlocks,
                    &resp,
                    msg.id,
                    version,
                )
                .await
            }
            // chia full_node_api.py:482-517 (respond_block / respond_blocks / reject_block /
            // reject_blocks): an unsolicited or late block reply bans the sender
            // (`peer.close(RATE_LIMITER_BAN_SECONDS)`) — a full node never volunteers these. A
            // SOLICITED reply never reaches this dispatch: the read loop's correlation-id fast
            // path (`PendingRequests::deliver`) consumes it for its single waiter and `continue`s
            // before the handler scan, so any of these four types arriving here is by definition
            // unsolicited (no pending waiter) or late (already timed out + cancelled) — the exact
            // two cases chia closes on. chia bans the sender 300s here
            // (`peer.close(RATE_LIMITER_BAN_SECONDS)`, full_node_api.py:489-516); we now do the same
            // via the peer's injected ban registry — close + evict + timed host ban.
            ProtocolMessageTypes::RespondBlock
            | ProtocolMessageTypes::RespondBlocks
            | ProtocolMessageTypes::RejectBlock
            | ProtocolMessageTypes::RejectBlocks => {
                log::warn!(
                    "unsolicited {:?} from peer {peer_id}: closing + banning the connection",
                    msg.msg_type
                );
                close_peer(peers, peer_id, Some(BanCause::RateLimit)).await
            }
            ProtocolMessageTypes::NewTransaction => {
                match self
                    .api
                    .on_new_transaction(*peer_id, decode(msg, version)?)
                    .await
                {
                    TransactionAnnounceAction::Request(req) => {
                        send(
                            &self.counters,
                            peers,
                            peer_id,
                            ProtocolMessageTypes::RequestTransaction,
                            &req,
                            None,
                            version,
                        )
                        .await
                    }
                    // chia peer.close(CONSENSUS_ERROR_BAN_SECONDS) (full_node_api.py:240/258/283):
                    // a zero-cost or already-seen-mismatched tx announcement bans the sender 600s.
                    // Close + evict + timed host ban via the peer's injected registry.
                    TransactionAnnounceAction::Ban => {
                        close_peer(peers, peer_id, Some(BanCause::ConsensusError)).await
                    }
                    TransactionAnnounceAction::Ignore => Ok(()),
                }
            }
            ProtocolMessageTypes::RequestTransaction => {
                let req = decode::<RequestTransaction>(msg, version)?;
                if let Some(tx) = self.api.transaction(req.transaction_id).await {
                    let resp = RespondTransaction { transaction: tx };
                    send(
                        &self.counters,
                        peers,
                        peer_id,
                        ProtocolMessageTypes::RespondTransaction,
                        &resp,
                        msg.id,
                        version,
                    )
                    .await
                } else {
                    Ok(())
                }
            }
            ProtocolMessageTypes::RespondTransaction => {
                let resp = decode::<RespondTransaction>(msg, version)?;
                let host = peer_host(peers, peer_id).await;
                self.api
                    .on_respond_transaction(*peer_id, host, resp.transaction)
                    .await;
                Ok(())
            }
            ProtocolMessageTypes::NewSignagePointOrEndOfSubSlot => {
                if let Some(req) = self
                    .api
                    .on_new_signage_point_or_eos(*peer_id, decode(msg, version)?)
                    .await
                {
                    send(
                        &self.counters,
                        peers,
                        peer_id,
                        ProtocolMessageTypes::RequestSignagePointOrEndOfSubSlot,
                        &req,
                        None,
                        version,
                    )
                    .await
                } else {
                    Ok(())
                }
            }
            ProtocolMessageTypes::RequestSignagePointOrEndOfSubSlot => {
                let req = decode::<RequestSignagePointOrEndOfSubSlot>(msg, version)?;
                match self.api.signage_point_or_eos(req).await {
                    Some(SignagePointResponse::SignagePoint(sp)) => {
                        send(
                            &self.counters,
                            peers,
                            peer_id,
                            ProtocolMessageTypes::RespondSignagePoint,
                            sp.as_ref(),
                            msg.id,
                            version,
                        )
                        .await
                    }
                    Some(SignagePointResponse::EndOfSubSlot(eos)) => {
                        send(
                            &self.counters,
                            peers,
                            peer_id,
                            ProtocolMessageTypes::RespondEndOfSubSlot,
                            eos.as_ref(),
                            msg.id,
                            version,
                        )
                        .await
                    }
                    None => Ok(()),
                }
            }
            ProtocolMessageTypes::RespondSignagePoint => {
                self.api
                    .on_respond_signage_point(*peer_id, decode(msg, version)?)
                    .await;
                Ok(())
            }
            ProtocolMessageTypes::RespondEndOfSubSlot => {
                self.api
                    .on_respond_end_of_sub_slot(*peer_id, decode(msg, version)?)
                    .await;
                Ok(())
            }
            ProtocolMessageTypes::NewUnfinishedBlock => {
                if let Some(req) = self
                    .api
                    .on_new_unfinished_block(*peer_id, decode(msg, version)?)
                    .await
                {
                    send(
                        &self.counters,
                        peers,
                        peer_id,
                        ProtocolMessageTypes::RequestUnfinishedBlock,
                        &req,
                        None,
                        version,
                    )
                    .await
                } else {
                    Ok(())
                }
            }
            ProtocolMessageTypes::NewUnfinishedBlock2 => {
                if let Some(req) = self
                    .api
                    .on_new_unfinished_block2(*peer_id, decode(msg, version)?)
                    .await
                {
                    send(
                        &self.counters,
                        peers,
                        peer_id,
                        ProtocolMessageTypes::RequestUnfinishedBlock2,
                        &req,
                        None,
                        version,
                    )
                    .await
                } else {
                    Ok(())
                }
            }
            ProtocolMessageTypes::RequestUnfinishedBlock => {
                let req = decode::<RequestUnfinishedBlock>(msg, version)?;
                match self.api.unfinished_block(req.unfinished_reward_hash).await {
                    Some(block) => {
                        let resp = RespondUnfinishedBlock {
                            unfinished_block: *block,
                        };
                        send(
                            &self.counters,
                            peers,
                            peer_id,
                            ProtocolMessageTypes::RespondUnfinishedBlock,
                            &resp,
                            msg.id,
                            version,
                        )
                        .await
                    }
                    None => Ok(()),
                }
            }
            ProtocolMessageTypes::RequestUnfinishedBlock2 => {
                let req = decode::<RequestUnfinishedBlock2>(msg, version)?;
                match self
                    .api
                    .unfinished_block2(req.unfinished_reward_hash, req.foliage_hash)
                    .await
                {
                    Some(block) => {
                        let resp = RespondUnfinishedBlock {
                            unfinished_block: *block,
                        };
                        send(
                            &self.counters,
                            peers,
                            peer_id,
                            ProtocolMessageTypes::RespondUnfinishedBlock,
                            &resp,
                            msg.id,
                            version,
                        )
                        .await
                    }
                    None => Ok(()),
                }
            }
            ProtocolMessageTypes::RespondUnfinishedBlock => {
                let resp = decode::<RespondUnfinishedBlock>(msg, version)?;
                self.api
                    .on_respond_unfinished_block(Box::new(resp.unfinished_block))
                    .await;
                Ok(())
            }
            ProtocolMessageTypes::RequestCompactVdf => {
                let req = decode::<RequestCompactVDF>(msg, version)?;
                if let Some(resp) = self.api.compact_vdf(req).await {
                    send(
                        &self.counters,
                        peers,
                        peer_id,
                        ProtocolMessageTypes::RespondCompactVdf,
                        &resp,
                        msg.id,
                        version,
                    )
                    .await
                } else {
                    Ok(())
                }
            }
            ProtocolMessageTypes::NewCompactVdf => {
                // CONSUME: a peer holds a compact proof for one of our stored blocks' VDF fields.
                // Pull it when ours is still bulky (the api decides + guards); the RespondCompactVDF
                // arm below finishes the swap.
                if let Some(req) = self
                    .api
                    .on_new_compact_vdf(*peer_id, decode(msg, version)?)
                    .await
                {
                    send(
                        &self.counters,
                        peers,
                        peer_id,
                        ProtocolMessageTypes::RequestCompactVdf,
                        &req,
                        None,
                        version,
                    )
                    .await
                } else {
                    Ok(())
                }
            }
            ProtocolMessageTypes::RespondCompactVdf => {
                // The pulled compact proof; queue it for the driver to validate + swap + re-gossip
                // (off the read path — a VDF verify never runs on the websocket loop).
                self.api
                    .on_respond_compact_vdf(*peer_id, decode(msg, version)?)
                    .await;
                Ok(())
            }
            ProtocolMessageTypes::RespondCompactProofOfTime => {
                // The bluebox timelord's answer to our RequestCompactProofOfTime solicitation; queue
                // it for the same off-read-path validate + swap + re-gossip as RespondCompactVdf.
                // Timelord-sender-only (chia protocol_message_type_to_node_type).
                if !require_timelord_sender(peers, peer_id, msg.msg_type).await? {
                    return Ok(());
                }
                self.api
                    .on_respond_compact_proof_of_time(*peer_id, decode(msg, version)?)
                    .await;
                Ok(())
            }
            ProtocolMessageTypes::RequestProofOfWeight => {
                // Queue-only: the api implementation hands {peer, tip, id, peer-map} to its worker
                // (see FullNodeApi::on_request_proof_of_weight). Nothing is sent from the read path.
                let req = decode::<RequestProofOfWeight>(msg, version)?;
                self.api
                    .on_request_proof_of_weight(*peer_id, req, msg.id, peers.clone())
                    .await;
                Ok(())
            }
            ProtocolMessageTypes::RequestPeers => {
                let _ = decode::<RequestPeers>(msg, version)?;
                let resp = RespondPeers {
                    peer_list: self.api.gossip_peers().await,
                };
                send(
                    &self.counters,
                    peers,
                    peer_id,
                    ProtocolMessageTypes::RespondPeers,
                    &resp,
                    msg.id,
                    version,
                )
                .await
            }
            ProtocolMessageTypes::RespondPeers => {
                let resp = decode::<RespondPeers>(msg, version)?;
                self.api.on_respond_peers(resp.peer_list).await;
                Ok(())
            }
            ProtocolMessageTypes::DeclareProofOfSpace => {
                // FARMER→NODE: a harvester's proof for a signage point we announced. The api
                // implementation gates on sync, resolves the SP + sub-slot, validates the proof, and
                // (Phase 4 increment 5) assembles the candidate block. On accept it returns the
                // RequestSignedValues to send BACK to this same farmer peer — chia
                // full_node_api.declare_proof_of_space's `peer.send_message(request_signed_values)`.
                // Mirrors the NewTransaction→RequestTransaction reply shape above.
                if let Some(req) = self
                    .api
                    .on_declare_proof_of_space(*peer_id, decode(msg, version)?)
                    .await
                {
                    send(
                        &self.counters,
                        peers,
                        peer_id,
                        ProtocolMessageTypes::RequestSignedValues,
                        &req,
                        None,
                        version,
                    )
                    .await
                } else {
                    Ok(())
                }
            }
            ProtocolMessageTypes::SignedValues => {
                // FARMER→NODE: signatures over the foliage we asked the farmer to sign. Consumed by
                // block assembly (Phase 4); matched here so the read loop routes it instead of
                // logging an unhandled-message ERROR.
                self.api
                    .on_signed_values(*peer_id, decode(msg, version)?)
                    .await;
                Ok(())
            }
            // chia full_node_api.py request_mempool_transactions (:856-869): decode the peer's
            // BIP158 filter and push each mempool item it lacks back as a NewTransaction on this
            // connection (the peer pulls what it wants through the normal announce path). The
            // filter decode + limit live in the api implementation (mempool_manager
            // .get_items_not_in_filter: limit 100, max_checked 5000).
            ProtocolMessageTypes::RequestMempoolTransactions => {
                let req = decode::<RequestMempoolTransactions>(msg, version)?;
                for tx in self.api.mempool_items(req.filter).await {
                    send(
                        &self.counters,
                        peers,
                        peer_id,
                        ProtocolMessageTypes::NewTransaction,
                        &tx,
                        None,
                        version,
                    )
                    .await?;
                }
                Ok(())
            }
            // TIMELORD→NODE infusion-return surface (chia full_node_api new_infusion_point_vdf /
            // new_signage_point_vdf / new_end_of_sub_slot_vdf). Queue-only: each api implementation gates on
            // sync and hands off to the driver — assembly + slot-state validation never run on the read loop.
            // All three are timelord-sender-only (chia protocol_message_type_to_node_type):
            // a non-timelord peer feeding VDF inboxes is a sender-type violation, not free CPU.
            ProtocolMessageTypes::NewInfusionPointVdf => {
                if !require_timelord_sender(peers, peer_id, msg.msg_type).await? {
                    return Ok(());
                }
                self.api
                    .on_new_infusion_point_vdf(*peer_id, decode(msg, version)?)
                    .await;
                Ok(())
            }
            ProtocolMessageTypes::NewSignagePointVdf => {
                if !require_timelord_sender(peers, peer_id, msg.msg_type).await? {
                    return Ok(());
                }
                self.api
                    .on_new_signage_point_vdf(*peer_id, decode(msg, version)?)
                    .await;
                Ok(())
            }
            ProtocolMessageTypes::NewEndOfSubSlotVdf => {
                if !require_timelord_sender(peers, peer_id, msg.msg_type).await? {
                    return Ok(());
                }
                self.api
                    .on_new_end_of_sub_slot_vdf(*peer_id, decode(msg, version)?)
                    .await;
                Ok(())
            }
            // ---- light-wallet query surface (chia wallet_protocol) ------------------------------------
            ProtocolMessageTypes::RequestPuzzleSolution => {
                let req = decode::<RequestPuzzleSolution>(msg, version)?;
                match self.api.puzzle_solution(req.coin_name, req.height).await {
                    Some(response) => {
                        let resp = RespondPuzzleSolution { response };
                        send(
                            &self.counters,
                            peers,
                            peer_id,
                            ProtocolMessageTypes::RespondPuzzleSolution,
                            &resp,
                            msg.id,
                            version,
                        )
                        .await
                    }
                    None => {
                        let rej = RejectPuzzleSolution {
                            coin_name: req.coin_name,
                            height: req.height,
                        };
                        send(
                            &self.counters,
                            peers,
                            peer_id,
                            ProtocolMessageTypes::RejectPuzzleSolution,
                            &rej,
                            msg.id,
                            version,
                        )
                        .await
                    }
                }
            }
            ProtocolMessageTypes::RequestBlockHeader => {
                let req = decode::<RequestBlockHeader>(msg, version)?;
                match self.api.block_header(req.height).await {
                    BlockHeaderReply::Respond(header_block) => {
                        let resp = RespondBlockHeader {
                            header_block: *header_block,
                        };
                        send(
                            &self.counters,
                            peers,
                            peer_id,
                            ProtocolMessageTypes::RespondBlockHeader,
                            &resp,
                            msg.id,
                            version,
                        )
                        .await
                    }
                    BlockHeaderReply::Reject(height) => {
                        let rej = RejectHeaderRequest { height };
                        send(
                            &self.counters,
                            peers,
                            peer_id,
                            ProtocolMessageTypes::RejectHeaderRequest,
                            &rej,
                            msg.id,
                            version,
                        )
                        .await
                    }
                    BlockHeaderReply::Silent => Ok(()),
                }
            }
            ProtocolMessageTypes::RequestHeaderBlocks => {
                let req = decode::<RequestHeaderBlocks>(msg, version)?;
                match self
                    .api
                    .header_blocks(req.start_height, req.end_height)
                    .await
                {
                    HeaderBlocksReply::Respond(resp) => {
                        send(
                            &self.counters,
                            peers,
                            peer_id,
                            ProtocolMessageTypes::RespondHeaderBlocks,
                            resp.as_ref(),
                            msg.id,
                            version,
                        )
                        .await
                    }
                    HeaderBlocksReply::Reject(rej) => {
                        send(
                            &self.counters,
                            peers,
                            peer_id,
                            ProtocolMessageTypes::RejectHeaderBlocks,
                            &rej,
                            msg.id,
                            version,
                        )
                        .await
                    }
                    HeaderBlocksReply::Silent => Ok(()),
                }
            }
            ProtocolMessageTypes::RequestBlockHeaders => {
                let req = decode::<RequestBlockHeaders>(msg, version)?;
                match self
                    .api
                    .block_headers(req.start_height, req.end_height, req.return_filter)
                    .await
                {
                    BlockHeadersReply::Respond(resp) => {
                        send(
                            &self.counters,
                            peers,
                            peer_id,
                            ProtocolMessageTypes::RespondBlockHeaders,
                            resp.as_ref(),
                            msg.id,
                            version,
                        )
                        .await
                    }
                    BlockHeadersReply::Reject(rej) => {
                        send(
                            &self.counters,
                            peers,
                            peer_id,
                            ProtocolMessageTypes::RejectBlockHeaders,
                            &rej,
                            msg.id,
                            version,
                        )
                        .await
                    }
                }
            }
            ProtocolMessageTypes::RequestAdditions => {
                let req = decode::<RequestAdditions>(msg, version)?;
                match self.api.additions(req).await {
                    AdditionsReply::Respond(resp) => {
                        send(
                            &self.counters,
                            peers,
                            peer_id,
                            ProtocolMessageTypes::RespondAdditions,
                            resp.as_ref(),
                            msg.id,
                            version,
                        )
                        .await
                    }
                    AdditionsReply::Reject(rej) => {
                        send(
                            &self.counters,
                            peers,
                            peer_id,
                            ProtocolMessageTypes::RejectAdditionsRequest,
                            &rej,
                            msg.id,
                            version,
                        )
                        .await
                    }
                }
            }
            ProtocolMessageTypes::RequestRemovals => {
                let req = decode::<RequestRemovals>(msg, version)?;
                match self.api.removals(req).await {
                    RemovalsReply::Respond(resp) => {
                        send(
                            &self.counters,
                            peers,
                            peer_id,
                            ProtocolMessageTypes::RespondRemovals,
                            resp.as_ref(),
                            msg.id,
                            version,
                        )
                        .await
                    }
                    RemovalsReply::Reject(rej) => {
                        send(
                            &self.counters,
                            peers,
                            peer_id,
                            ProtocolMessageTypes::RejectRemovalsRequest,
                            &rej,
                            msg.id,
                            version,
                        )
                        .await
                    }
                }
            }
            ProtocolMessageTypes::RequestChildren => {
                let req = decode::<RequestChildren>(msg, version)?;
                let coin_states = self.api.children(req.coin_name).await;
                let resp = RespondChildren { coin_states };
                send(
                    &self.counters,
                    peers,
                    peer_id,
                    ProtocolMessageTypes::RespondChildren,
                    &resp,
                    msg.id,
                    version,
                )
                .await
            }
            ProtocolMessageTypes::RegisterInterestInPuzzleHash => {
                // CHIA-4203 (chia b483e59f22): the subscription cap is applied DURING decode —
                // puzzle hashes past max_subscriptions(peer) are skipped in O(1), never parsed.
                let host = peer_host(peers, peer_id).await;
                let req = RegisterForPhUpdates::from_bytes_limited(
                    &mut Cursor::new(msg.data.as_slice()),
                    version,
                    self.api.max_subscriptions(peer_id, host),
                )?;
                let reg = self.api.register_for_ph_updates(*peer_id, host, req).await;
                // On the peer's first registration, stand up the per-peer CoinStateUpdate forwarder.
                if let Some(rx) = reg.receiver {
                    spawn_coin_state_forwarder(
                        self.counters.clone(),
                        peers.clone(),
                        *peer_id,
                        rx,
                        version,
                    );
                }
                send(
                    &self.counters,
                    peers,
                    peer_id,
                    ProtocolMessageTypes::RespondToPhUpdate,
                    &reg.response,
                    msg.id,
                    version,
                )
                .await
            }
            ProtocolMessageTypes::RegisterInterestInCoin => {
                // CHIA-4203: coin_ids past max_subscriptions(peer) are skipped at decode time.
                let host = peer_host(peers, peer_id).await;
                let req = RegisterForCoinUpdates::from_bytes_limited(
                    &mut Cursor::new(msg.data.as_slice()),
                    version,
                    self.api.max_subscriptions(peer_id, host),
                )?;
                let reg = self
                    .api
                    .register_for_coin_updates(*peer_id, host, req)
                    .await;
                if let Some(rx) = reg.receiver {
                    spawn_coin_state_forwarder(
                        self.counters.clone(),
                        peers.clone(),
                        *peer_id,
                        rx,
                        version,
                    );
                }
                send(
                    &self.counters,
                    peers,
                    peer_id,
                    ProtocolMessageTypes::RespondToCoinUpdate,
                    &reg.response,
                    msg.id,
                    version,
                )
                .await
            }
            // ---- the modern wallet-sync surface (codes 94-103, the Sage sync loop). Every
            // request is answered with exactly one Respond*/Reject* echoing the request id — the
            // silent drop of these four was why Sage could not sync against this node at all.
            ProtocolMessageTypes::RequestPuzzleState => {
                // CHIA-4203: puzzle_hashes past the store batch bound are skipped at decode time
                // (chia wires CoinStore.MAX_PUZZLE_HASH_BATCH_SIZE here).
                let req = RequestPuzzleState::from_bytes_limited(
                    &mut Cursor::new(msg.data.as_slice()),
                    version,
                    MAX_PUZZLE_HASH_BATCH_SIZE,
                )?;
                let host = peer_host(peers, peer_id).await;
                match self.api.puzzle_state(*peer_id, host, req).await {
                    PuzzleStateReply::Respond(resp, receiver) => {
                        // First subscribe-on-finish: stand up the per-peer CoinStateUpdate
                        // forwarder, exactly like RegisterInterestInPuzzleHash.
                        if let Some(rx) = receiver {
                            spawn_coin_state_forwarder(
                                self.counters.clone(),
                                peers.clone(),
                                *peer_id,
                                rx,
                                version,
                            );
                        }
                        send(
                            &self.counters,
                            peers,
                            peer_id,
                            ProtocolMessageTypes::RespondPuzzleState,
                            resp.as_ref(),
                            msg.id,
                            version,
                        )
                        .await
                    }
                    PuzzleStateReply::Reject(reason) => {
                        // chia streams the reason as uint8 (wallet_protocol.RejectPuzzleState).
                        let rej = RejectPuzzleState {
                            reason: reason as u8,
                        };
                        send(
                            &self.counters,
                            peers,
                            peer_id,
                            ProtocolMessageTypes::RejectPuzzleState,
                            &rej,
                            msg.id,
                            version,
                        )
                        .await
                    }
                }
            }
            ProtocolMessageTypes::RequestCoinState => {
                // CHIA-4203's motivating case: a RequestCoinState claiming 1.2M coin_ids cost
                // ~6 s of parse CPU on a Pi4 (our hardware floor) before the handler truncated.
                // The cap now bounds the parse itself.
                let host = peer_host(peers, peer_id).await;
                let req = RequestCoinState::from_bytes_limited(
                    &mut Cursor::new(msg.data.as_slice()),
                    version,
                    self.api.max_subscribe_response_items(peer_id, host),
                )?;
                match self.api.coin_state(*peer_id, host, req).await {
                    CoinStateReply::Respond(resp, receiver) => {
                        if let Some(rx) = receiver {
                            spawn_coin_state_forwarder(
                                self.counters.clone(),
                                peers.clone(),
                                *peer_id,
                                rx,
                                version,
                            );
                        }
                        send(
                            &self.counters,
                            peers,
                            peer_id,
                            ProtocolMessageTypes::RespondCoinState,
                            resp.as_ref(),
                            msg.id,
                            version,
                        )
                        .await
                    }
                    CoinStateReply::Reject(reason) => {
                        let rej = RejectCoinState { reason };
                        send(
                            &self.counters,
                            peers,
                            peer_id,
                            ProtocolMessageTypes::RejectCoinState,
                            &rej,
                            msg.id,
                            version,
                        )
                        .await
                    }
                }
            }
            ProtocolMessageTypes::RequestFeeEstimates => {
                let req = decode::<RequestFeeEstimates>(msg, version)?;
                let estimates = self.api.fee_estimates(req).await;
                let resp = RespondFeeEstimates { estimates };
                send(
                    &self.counters,
                    peers,
                    peer_id,
                    ProtocolMessageTypes::RespondFeeEstimates,
                    &resp,
                    msg.id,
                    version,
                )
                .await
            }
            ProtocolMessageTypes::RequestRemovePuzzleSubscriptions => {
                let req = decode::<RequestRemovePuzzleSubscriptions>(msg, version)?;
                let puzzle_hashes = self
                    .api
                    .remove_puzzle_subscriptions(*peer_id, req.puzzle_hashes)
                    .await;
                let resp = RespondRemovePuzzleSubscriptions { puzzle_hashes };
                send(
                    &self.counters,
                    peers,
                    peer_id,
                    ProtocolMessageTypes::RespondRemovePuzzleSubscriptions,
                    &resp,
                    msg.id,
                    version,
                )
                .await
            }
            ProtocolMessageTypes::RequestRemoveCoinSubscriptions => {
                let req = decode::<RequestRemoveCoinSubscriptions>(msg, version)?;
                let coin_ids = self
                    .api
                    .remove_coin_subscriptions(*peer_id, req.coin_ids)
                    .await;
                let resp = RespondRemoveCoinSubscriptions { coin_ids };
                send(
                    &self.counters,
                    peers,
                    peer_id,
                    ProtocolMessageTypes::RespondRemoveCoinSubscriptions,
                    &resp,
                    msg.id,
                    version,
                )
                .await
            }
            ProtocolMessageTypes::SendTransaction => {
                // chia full_node_api.py::send_transaction — the wallet's spend submit. The api
                // returns the ack (never None): SUCCESS/PENDING/FAILED + chia's Err.name. The
                // reply echoes the request id, so a wallet's request/response correlation (the
                // Sage RequestOrReject path) resolves on it.
                let req = decode::<SendTransaction>(msg, version)?;
                let ack = self.api.send_transaction(*peer_id, req).await;
                send(
                    &self.counters,
                    peers,
                    peer_id,
                    ProtocolMessageTypes::TransactionAck,
                    &ack,
                    msg.id,
                    version,
                )
                .await
            }
            ProtocolMessageTypes::ConfigureWindowSizes => {
                // The peer's RATE_LIMITS_V3 settings (chia a1b12d321). Only legitimate on a link
                // where the capability was negotiated (we mirrored it in our handshake reply);
                // otherwise, and on any validation failure (empty / oversized / bounding one of
                // OUR unlimited types), chia raises INVALID_HANDSHAKE — close with the short
                // protocol ban.
                let Some(peer) = peers.read().await.get(peer_id).cloned() else {
                    return Ok(());
                };
                if !peer.v3.is_offered() {
                    log::warn!(
                        "Peer {peer_id} sent ConfigureWindowSizes without negotiating \
                         RATE_LIMITS_V3; closing"
                    );
                    return close_peer(peers, peer_id, Some(BanCause::InvalidProtocol)).await;
                }
                let cfg = decode::<ConfigureWindowSizes>(msg, version)?;
                match settings_from_configure(&cfg) {
                    Ok(settings) => {
                        peer.v3.activate(settings);
                        Ok(())
                    }
                    Err(e) => {
                        log::warn!("Peer {peer_id} sent an invalid ConfigureWindowSizes: {e}");
                        close_peer(peers, peer_id, Some(BanCause::InvalidProtocol)).await
                    }
                }
            }
            ProtocolMessageTypes::Error => {
                // The `error` protocol message (chia ede354c58, code 255): a CNI ≥ 0.0.35 peer
                // reports a handler-side ApiError in place of a typed reject. chia's `_api_call`
                // decodes it, logs a WARNING, and carries on — no ban, no disconnect
                // (ws_connection.py:490-493). Tolerant parse only; we do not emit Error frames.
                match decode::<ErrorMessage>(msg, version) {
                    Ok(err) => {
                        log::warn!(
                            "Peer {peer_id} sent protocol Error: code={} message={:?}",
                            err.code,
                            err.message
                        );
                    }
                    Err(e) => {
                        log::warn!("Peer {peer_id} sent an undecodable protocol Error: {e:?}");
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

#[async_trait]
impl MessageHandler for FullNodeHandler {
    async fn handle(
        &self,
        msg: Arc<ChiaMessage>,
        peer_id: Arc<Bytes32>,
        peers: PeerMap,
    ) -> Result<(), Error> {
        let size = msg.data.as_slice().len();
        self.counters.count_in(msg.msg_type, size);
        // Inbound rate limiting is enforced upstream at the read loop (chia's `inbound_rate_limiter`
        // in `_read_one_message`), which charges EVERY inbound message — including solicited replies
        // consumed by the correlation fast-path — before dispatch and closes on violation. The
        // handler therefore trusts that a message reaching it is within budget.
        let version = peer_version(&peers, &peer_id).await;
        self.dispatch(&msg, &peer_id, &peers, version).await
    }
}

fn served(msg_type: ProtocolMessageTypes) -> bool {
    matches!(
        msg_type,
        ProtocolMessageTypes::Handshake
            | ProtocolMessageTypes::NewPeak
            | ProtocolMessageTypes::RequestBlock
            | ProtocolMessageTypes::RequestBlocks
            // The four block replies (chia full_node_api.py:482-517): solicited ones are consumed
            // by the read loop's correlation-id fast path before the handler scan ever runs, so
            // matching them here only catches unsolicited/late ones — dispatched to the close arm.
            | ProtocolMessageTypes::RespondBlock
            | ProtocolMessageTypes::RespondBlocks
            | ProtocolMessageTypes::RejectBlock
            | ProtocolMessageTypes::RejectBlocks
            | ProtocolMessageTypes::NewTransaction
            | ProtocolMessageTypes::RequestTransaction
            | ProtocolMessageTypes::RespondTransaction
            | ProtocolMessageTypes::RespondUnfinishedBlock
            | ProtocolMessageTypes::NewUnfinishedBlock
            | ProtocolMessageTypes::NewUnfinishedBlock2
            | ProtocolMessageTypes::RequestUnfinishedBlock
            | ProtocolMessageTypes::RequestUnfinishedBlock2
            | ProtocolMessageTypes::NewSignagePointOrEndOfSubSlot
            | ProtocolMessageTypes::RequestSignagePointOrEndOfSubSlot
            | ProtocolMessageTypes::RespondSignagePoint
            | ProtocolMessageTypes::RespondEndOfSubSlot
            | ProtocolMessageTypes::RequestCompactVdf
            // The compact-VDF consume flow: announce in, pulled proof in (dispatched above).
            | ProtocolMessageTypes::NewCompactVdf
            | ProtocolMessageTypes::RespondCompactVdf
            | ProtocolMessageTypes::RequestPeers
            | ProtocolMessageTypes::RespondPeers
            | ProtocolMessageTypes::RequestProofOfWeight
            // Gossip broadcast we match only to graceful-ignore (see the dispatch arm). Kept in the
            // filter so it never surfaces as an unhandled "No Matches" ERROR on a peer link.
            | ProtocolMessageTypes::RequestMempoolTransactions
            // Farmer interface (Phase 3): declared proofs in (validated + made candidate), signed
            // foliage values in (Phase 4 assembly). NewSignagePoint / RequestSignedValues are
            // node→farmer sends, not inbound-served here.
            | ProtocolMessageTypes::DeclareProofOfSpace
            | ProtocolMessageTypes::SignedValues
            // Timelord infusion-return surface: infusion-point / signage-point / end-of-sub-slot VDFs the
            // timelord sends back so the node can finish its own farmed unfinished block into a peak.
            | ProtocolMessageTypes::NewInfusionPointVdf
            | ProtocolMessageTypes::NewSignagePointVdf
            | ProtocolMessageTypes::NewEndOfSubSlotVdf
            // The bluebox return: a timelord's compact proof answering our RequestCompactProofOfTime
            // solicitation (dispatched to the consume path). Kept here so it never surfaces as an
            // unhandled "No Matches" ERROR on the timelord link.
            | ProtocolMessageTypes::RespondCompactProofOfTime
            // Light-wallet query surface (chia wallet_protocol): spend submit (acked), puzzle/
            // solution, header blocks, additions/removals, and coin children — each answered with
            // a Respond*/Reject*/Ack body.
            | ProtocolMessageTypes::SendTransaction
            | ProtocolMessageTypes::RequestPuzzleSolution
            | ProtocolMessageTypes::RequestBlockHeader
            | ProtocolMessageTypes::RequestHeaderBlocks
            | ProtocolMessageTypes::RequestBlockHeaders
            | ProtocolMessageTypes::RequestAdditions
            | ProtocolMessageTypes::RequestRemovals
            | ProtocolMessageTypes::RequestChildren
            // Coin-state subscriptions: register interest (initial state reply + a live CoinStateUpdate
            // forwarder). CoinStateUpdate itself is a node->wallet send, not inbound-served.
            | ProtocolMessageTypes::RegisterInterestInPuzzleHash
            | ProtocolMessageTypes::RegisterInterestInCoin
            // The modern wallet-sync surface (the Sage sync loop): paged puzzle-hash state,
            // coin-id state, and subscription removal. NewPeakWallet is a node->wallet send.
            | ProtocolMessageTypes::RequestPuzzleState
            | ProtocolMessageTypes::RequestCoinState
            | ProtocolMessageTypes::RequestRemovePuzzleSubscriptions
            | ProtocolMessageTypes::RequestRemoveCoinSubscriptions
            // Fee estimation (chia wallet_protocol code 89): the wallet asks for fee-rate
            // estimates at a set of target times; answered with a RespondFeeEstimates group.
            | ProtocolMessageTypes::RequestFeeEstimates
            // The `error` protocol message (chia code 255): tolerated + logged, never banned —
            // chia's own posture (ws_connection.py `_api_call` warns and returns). Matching it
            // here keeps a conforming CNI peer's ApiError report off the unknown-type close path.
            | ProtocolMessageTypes::Error
            // RATE_LIMITS_V3 configure exchange (chia code 111): the peer's window settings,
            // validated + stored by the dispatch arm.
            | ProtocolMessageTypes::ConfigureWindowSizes
    )
}

fn build_handlers(
    api: Arc<dyn FullNodeApi>,
    network_id: String,
    server_port: u16,
    respond_handshake: bool,
    counters: Arc<NetCounters>,
) -> HashMap<Uuid, Arc<ChiaMessageHandler>> {
    let handler = Arc::new(FullNodeHandler {
        api,
        network_id,
        server_port,
        respond_handshake,
        counters,
    });
    let mut map = HashMap::new();
    map.insert(
        Uuid::new_v4(),
        Arc::new(ChiaMessageHandler::new(
            Arc::new(ChiaMessageFilter {
                msg_type: None,
                id: None,
                custom_fn: Some(Box::new(|m| served(m.msg_type))),
            }),
            handler,
        )),
    );
    map
}

// Register the full-node handler set on a `WebsocketServer`'s handler map:
// one dispatcher keyed by a single Uuid (O(1) register/unregister). Server role — replies to an inbound
// peer's Handshake.
#[must_use]
pub fn full_node_handlers(
    api: Arc<dyn FullNodeApi>,
    network_id: String,
    server_port: u16,
) -> HashMap<Uuid, Arc<ChiaMessageHandler>> {
    build_handlers(api, network_id, server_port, true, Arc::default())
}

// Server-role handlers sharing the node's traffic counters (the daemon's constructor; the
// plain variant keeps a private instance for tests/tools).
#[must_use]
pub fn full_node_handlers_counted(
    api: Arc<dyn FullNodeApi>,
    network_id: String,
    server_port: u16,
    counters: Arc<NetCounters>,
) -> HashMap<Uuid, Arc<ChiaMessageHandler>> {
    build_handlers(api, network_id, server_port, true, counters)
}

// The same dispatcher for an OUTBOUND peer link (we dialed the peer): tip announcements (NewPeak), block
// serving, and graceful-ignore of gossip. Does NOT reply to Handshake — the outbound `WsClient` already
// performed the handshake, so a reply here would be a duplicate. This is what closes the live-deploy
// "No Matches for Message: NewPeak" gap on our own peer connections.
#[must_use]
pub fn full_node_handlers_client(
    api: Arc<dyn FullNodeApi>,
    network_id: String,
    server_port: u16,
) -> HashMap<Uuid, Arc<ChiaMessageHandler>> {
    build_handlers(api, network_id, server_port, false, Arc::default())
}

#[must_use]
pub fn full_node_handlers_client_counted(
    api: Arc<dyn FullNodeApi>,
    network_id: String,
    server_port: u16,
    counters: Arc<NetCounters>,
) -> HashMap<Uuid, Arc<ChiaMessageHandler>> {
    build_handlers(api, network_id, server_port, false, counters)
}

#[cfg(test)]
mod tests {
    use super::served;
    use dg_xch_core::protocols::ProtocolMessageTypes;

    // Red-first: the dispatch filter must MATCH tip announcements, block requests, the pure-gossip
    // broadcasts (so they graceful-ignore instead of logging "No Matches"), AND the four block
    // replies (a solicited one is consumed by the read loop's correlation-id fast path before the
    // handler scan, so a match here is by definition unsolicited/late → the close arm, chia
    // full_node_api.py:482-517). RespondProofOfWeight stays oneshot-owned and unmatched — chia
    // only logs it (full_node_api.py:398-401, no ban), and ours falls to the read loop's
    // no-match drop.
    #[test]
    fn filter_matches_gossip_but_not_oneshot_responses() {
        for t in [
            ProtocolMessageTypes::Handshake,
            ProtocolMessageTypes::NewPeak,
            ProtocolMessageTypes::RequestBlock,
            ProtocolMessageTypes::RequestBlocks,
            ProtocolMessageTypes::RespondBlock,
            ProtocolMessageTypes::RespondBlocks,
            ProtocolMessageTypes::RejectBlock,
            ProtocolMessageTypes::RejectBlocks,
            ProtocolMessageTypes::NewCompactVdf,
            ProtocolMessageTypes::NewSignagePointOrEndOfSubSlot,
            ProtocolMessageTypes::NewUnfinishedBlock2,
            ProtocolMessageTypes::RequestMempoolTransactions,
            ProtocolMessageTypes::RequestProofOfWeight,
            ProtocolMessageTypes::NewInfusionPointVdf,
            ProtocolMessageTypes::NewSignagePointVdf,
            ProtocolMessageTypes::NewEndOfSubSlotVdf,
            ProtocolMessageTypes::SendTransaction,
            ProtocolMessageTypes::RequestPuzzleSolution,
            ProtocolMessageTypes::RequestBlockHeader,
            ProtocolMessageTypes::RequestHeaderBlocks,
            ProtocolMessageTypes::RequestBlockHeaders,
            ProtocolMessageTypes::RequestAdditions,
            ProtocolMessageTypes::RequestRemovals,
            ProtocolMessageTypes::RequestChildren,
            ProtocolMessageTypes::RegisterInterestInPuzzleHash,
            ProtocolMessageTypes::RegisterInterestInCoin,
            ProtocolMessageTypes::RequestPuzzleState,
            ProtocolMessageTypes::RequestCoinState,
            ProtocolMessageTypes::RequestRemovePuzzleSubscriptions,
            ProtocolMessageTypes::RequestRemoveCoinSubscriptions,
            ProtocolMessageTypes::RequestFeeEstimates,
        ] {
            assert!(
                served(t),
                "{t:?} must be handled (dispatched or graceful-ignored)"
            );
        }
        assert!(
            !served(ProtocolMessageTypes::RespondProofOfWeight),
            "RespondProofOfWeight is oneshot-owned and must not match the dispatch filter"
        );
    }
}
