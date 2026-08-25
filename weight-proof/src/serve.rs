// The serving (construction) half of the Chia weight proof — the prover-side mirror of this crate's
// validator, ported from the construction half of `chia/full_node/weight_proof.py`
// (`WeightProofHandler` plus its module-level construction helpers). Every mirrored function cites its
// reference span as `chia weight_proof.py:<lines>`; the store surface is the node's own
// `dg_xch_stores::BlockStore`, so what this builds is exactly what the daemon's p2p arm serves.
//
// Sampling reuses the SAME CPython-`random.Random` port and the SAME `_get_weights_for_sampling` /
// `_sample_sub_epoch` mirrors the validator uses (`crate::py_random`, `crate::get_weights_for_sampling`,
// `crate::sample_sub_epoch`) — one implementation on both sides, so the builder's sampled sub-epoch set
// is byte-for-byte the set any chia validator derives from the same seed (same rng call order:
// seed → the `int(queries)+1` `random()` draws → sort).

use crate::WeightProofError;
use dg_xch_core::blockchain::block_record::BlockRecord;
use dg_xch_core::blockchain::end_of_subslot_bundle::EndOfSubSlotBundle;
use dg_xch_core::blockchain::header_block::HeaderBlock;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::blockchain::sub_epoch_summary::SubEpochSummary;
use dg_xch_core::blockchain::vdf_info::VdfInfo;
use dg_xch_core::blockchain::weight_proof::{
    SubEpochChallengeSegment, SubEpochData, SubEpochSegments, SubSlotData, WeightProof,
};
use dg_xch_core::consensus::constants::ConsensusConstants;
use dg_xch_core::consensus::vdf_info_computation::get_signage_point_vdf_info;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use dg_xch_stores::{BlockStore, StoreError};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::Cursor;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info};

// The on-disk encoding version for persisted SubEpochSegments (the store holds the bytes
// opaquely — chia block_store.py:170 stores `bytes(SubEpochSegments(segments))`). Pinned to the
// same version the store backends pin for record blobs; the segment types serialize identically
// across current protocol versions, but a pin keeps persisted bytes stable by construction.
const SEGMENT_STORE_VERSION: ChiaProtocolVersion = ChiaProtocolVersion::Chia0_0_37;

/// Why a proof could not be produced. The two `is_refusal` variants mirror chia's silent no-reply
/// paths (`chia/full_node/full_node_api.py:359-364` unknown tip, `chia weight_proof.py:86-88` short
/// chain); everything else is a real build failure worth logging loudly.
#[derive(Debug)]
pub enum ServeError {
    /// The requested tip is not a block we hold (chia full_node_api.py:362-364 → no reply).
    UnknownTip(Bytes32),
    /// Tip height below `WEIGHT_PROOF_RECENT_BLOCKS` (chia weight_proof.py:86-88 → no reply).
    ChainTooShort { height: u32, required: u32 },
    /// Fewer than two sub-epoch summaries at-or-below the tip (chia weight_proof.py:127-129 → no reply).
    NotEnoughSubEpochs,
    /// The store errored.
    Store(StoreError),
    /// A main-chain height had no record/body — the store cannot back a proof to this tip.
    MissingBlock(u32),
    /// A record/header referenced by hash during segment construction was outside the loaded span.
    MissingRecord(Bytes32),
    /// A structural invariant the reference asserts did not hold while building.
    Build(String),
}

impl fmt::Display for ServeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServeError::UnknownTip(tip) => write!(f, "unknown tip {tip}"),
            ServeError::ChainTooShort { height, required } => {
                write!(
                    f,
                    "chain too short for weight proof: tip {height} < {required}"
                )
            }
            ServeError::NotEnoughSubEpochs => write!(f, "not enough sub epochs"),
            ServeError::Store(e) => write!(f, "store error: {e}"),
            ServeError::MissingBlock(h) => write!(f, "missing block at height {h}"),
            ServeError::MissingRecord(hh) => write!(f, "missing record {hh}"),
            ServeError::Build(msg) => write!(f, "weight proof build failed: {msg}"),
        }
    }
}

impl std::error::Error for ServeError {}

impl From<StoreError> for ServeError {
    fn from(e: StoreError) -> Self {
        ServeError::Store(e)
    }
}

impl From<std::io::Error> for ServeError {
    fn from(e: std::io::Error) -> Self {
        ServeError::Build(e.to_string())
    }
}

impl From<WeightProofError> for ServeError {
    fn from(e: WeightProofError) -> Self {
        ServeError::Build(format!("{e:?}"))
    }
}

impl ServeError {
    /// True for the chia-mirrored refusal paths where the peer simply gets no reply.
    #[must_use]
    pub fn is_refusal(&self) -> bool {
        matches!(
            self,
            ServeError::UnknownTip(_)
                | ServeError::ChainTooShort { .. }
                | ServeError::NotEnoughSubEpochs
        )
    }
}

// An in-RAM view of one contiguous main-chain span — the counterpart of the reference's
// `get_block_records_in_range` + `get_header_blocks_in_range(tx_filter=False)` dict pair plus
// `height_to_hash` (chia weight_proof.py:305-310). Heights that don't resolve in the store are simply
// absent (chia's range fetch likewise collects only existing hashes); a later lookup miss errors.
struct ChainCache {
    height_to_hash: HashMap<u32, Bytes32>,
    records: HashMap<Bytes32, BlockRecord>,
    headers: HashMap<Bytes32, HeaderBlock>,
}

impl ChainCache {
    fn hash_at(&self, height: u32) -> Result<Bytes32, ServeError> {
        self.height_to_hash
            .get(&height)
            .copied()
            .ok_or(ServeError::MissingBlock(height))
    }

    fn record(&self, hh: &Bytes32) -> Result<&BlockRecord, ServeError> {
        self.records.get(hh).ok_or(ServeError::MissingRecord(*hh))
    }

    fn header(&self, hh: &Bytes32) -> Result<&HeaderBlock, ServeError> {
        self.headers.get(hh).ok_or(ServeError::MissingRecord(*hh))
    }

    fn header_at(&self, height: u32) -> Result<&HeaderBlock, ServeError> {
        self.header(&self.hash_at(height)?)
    }
}

// The builder's mutable state, all under one async lock (see `WeightProofServer::state`).
struct ServeState {
    // Whole-proof cache keyed by tip — chia's `self.tip`/`self.proof` (weight_proof.py:72-73, checked
    // and refreshed under the lock at weight_proof.py:90-99).
    proof: Option<(Bytes32, Arc<WeightProof>)>,
    // The sub-epoch-summary index: every main-chain record carrying `sub_epoch_summary_included`,
    // ascending by height. Chia's `BlockchainInterface.get_ses_heights`/`get_ses` serve this from an
    // in-RAM height map; our store deliberately has no ses schema, so it is DERIVED by walking records
    // (the same approach `sub_epoch_summaries_of` takes from a proof) — incrementally, so the full walk
    // is paid once per server, then only the delta above `walked_to`.
    ses_blocks: Vec<BlockRecord>,
    walked_to: Option<u32>,
    // Built segments keyed by the ses block's header hash, LRU-bounded to `MAX_SAMPLES` sub-epochs
    // — the hot layer over the store's durable `sub_epoch_segments_v3` rows (chia keeps the same
    // two tiers: `ses_challenge_cache` in block_store.py over the persisted table). A miss here
    // falls through to `BlockStore::get_sub_epoch_segments` before any block walking; a build
    // persists through `persist_sub_epoch_segments` (chia weight_proof.py:288-297).
    segments: VecDeque<(Bytes32, Arc<Vec<SubEpochChallengeSegment>>)>,
}

impl ServeState {
    fn cached_segments(&mut self, hh: &Bytes32) -> Option<Arc<Vec<SubEpochChallengeSegment>>> {
        let idx = self.segments.iter().position(|(k, _)| k == hh)?;
        // Move the hit to the back so eviction pops the least-recently-used ses first.
        let entry = self.segments.remove(idx)?;
        let segs = entry.1.clone();
        self.segments.push_back(entry);
        Some(segs)
    }

    fn cache_segments(&mut self, hh: Bytes32, segs: Arc<Vec<SubEpochChallengeSegment>>) {
        self.segments.push_back((hh, segs));
        while self.segments.len() > crate::MAX_SAMPLES {
            self.segments.pop_front();
        }
    }
}

/// The construction-side `WeightProofHandler` (chia weight_proof.py:61-99): builds — and caches — the
/// weight proof for a requested tip out of a `BlockStore`. One instance per node; the internal lock is
/// chia's `self.lock` and doubles as the async single-flight: concurrent requests for the same tip
/// serialize on it, the first builds, the rest return the cached proof.
pub struct WeightProofServer<S: ?Sized> {
    store: Arc<S>,
    constants: ConsensusConstants,
    state: Mutex<ServeState>,
}

impl<S> WeightProofServer<S>
where
    S: BlockStore + Send + Sync + ?Sized,
{
    #[must_use]
    pub fn new(store: Arc<S>, constants: ConsensusConstants) -> Self {
        WeightProofServer {
            store,
            constants,
            state: Mutex::new(ServeState {
                proof: None,
                ses_blocks: Vec::new(),
                walked_to: None,
                segments: VecDeque::new(),
            }),
        }
    }

    /// chia `WeightProofHandler.get_proof_of_weight` (chia weight_proof.py:80-99): refuse an unknown
    /// tip or a chain shorter than `WEIGHT_PROOF_RECENT_BLOCKS`, then — under the single-flight lock —
    /// return the cached proof when it already attests this tip, else build and cache.
    ///
    /// # Errors
    /// Returns a refusal variant ([`ServeError::is_refusal`]) on the chia no-reply paths, otherwise a
    /// store/build error.
    pub async fn get_proof_of_weight(&self, tip: Bytes32) -> Result<Arc<WeightProof>, ServeError> {
        // chia weight_proof.py:81-84 — `try_block_record(tip)` unknown → refuse.
        let tip_rec = self
            .store
            .get_block_record(&tip)
            .await?
            .ok_or(ServeError::UnknownTip(tip))?;
        // chia weight_proof.py:86-88 — tip below WEIGHT_PROOF_RECENT_BLOCKS → refuse.
        if tip_rec.height < self.constants.weight_proof_recent_blocks {
            return Err(ServeError::ChainTooShort {
                height: tip_rec.height,
                required: self.constants.weight_proof_recent_blocks,
            });
        }
        // chia weight_proof.py:90-99 — the lock + tip-keyed cache. Held across the whole build on
        // purpose: that IS the single-flight (chia holds `self.lock` across `_create_proof_of_weight`
        // the same way), and the builder task is the only latency-tolerant consumer.
        let mut st = self.state.lock().await;
        if let Some((cached_tip, wp)) = &st.proof
            && *cached_tip == tip
        {
            return Ok(wp.clone());
        }
        let wp = Arc::new(self.create_proof_of_weight(&mut st, &tip_rec).await?);
        st.proof = Some((tip, wp.clone()));
        Ok(wp)
    }

    /// chia `WeightProofHandler._create_proof_of_weight` (chia weight_proof.py:111-175): recent chain,
    /// sub-epoch data, seed-derived sampling, and per-sampled-sub-epoch segment construction.
    async fn create_proof_of_weight(
        &self,
        st: &mut ServeState,
        tip_rec: &BlockRecord,
    ) -> Result<WeightProof, ServeError> {
        info!(tip = %tip_rec.header_hash, height = tip_rec.height, "create weight proof");
        // The ses index must reach the tip before anything else (chia's get_ses_heights is pre-built).
        self.extend_ses_index(st, tip_rec.height).await?;
        let ses_blocks = st.ses_blocks.clone();

        // chia weight_proof.py:122-124.
        let recent_chain = self.get_recent_chain(&ses_blocks, tip_rec.height).await?;

        // chia weight_proof.py:126-129 — needs at least two summaries.
        if ses_blocks.len() <= 1 {
            return Err(ServeError::NotEnoughSubEpochs);
        }

        // chia weight_proof.py:131-135 — the genesis record opens the first sub-epoch's weight band.
        let mut prev_ses_block = self
            .store
            .get_block_record_by_height(0)
            .await?
            .ok_or(ServeError::MissingBlock(0))?;

        // chia weight_proof.py:136 / get_sub_epoch_data (101-109) / _create_sub_epoch_data (716-725).
        let mut sub_epochs: Vec<SubEpochData> = Vec::new();
        for ses_block in &ses_blocks {
            if ses_block.height > tip_rec.height {
                break;
            }
            let ses = ses_block
                .sub_epoch_summary_included
                .as_ref()
                .ok_or_else(|| ServeError::Build("ses block without summary".into()))?;
            sub_epochs.push(create_sub_epoch_data(ses));
        }

        // chia weight_proof.py:137-140 — seed from the SECOND-TO-LAST summary at-or-below the tip,
        // then the sampling draws in the reference's exact rng call order.
        let seed = get_seed_for_proof(&ses_blocks, tip_rec.height)?;
        let mut rng = crate::py_random::PyRandom::new(seed.as_ref());
        let weight_to_check =
            crate::get_weights_for_sampling(&mut rng, tip_rec.weight, &recent_chain)?;

        // chia weight_proof.py:141-173 — sample each sub-epoch's weight band; build (or reuse) the
        // challenge segments for every sampled one, capped at MAX_SAMPLES.
        let mut sample_n = 0usize;
        let mut sub_epoch_segments: Vec<SubEpochChallengeSegment> = Vec::new();
        for (sub_epoch_n, ses_block) in ses_blocks.iter().enumerate() {
            if ses_block.height > tip_rec.height {
                break;
            }
            if sample_n >= crate::MAX_SAMPLES {
                debug!("reached sampled sub epoch cap");
                break;
            }
            if ses_block.sub_epoch_summary_included.is_none() {
                return Err(ServeError::Build("ses block without summary".into()));
            }
            if crate::sample_sub_epoch(
                prev_ses_block.weight,
                ses_block.weight,
                weight_to_check.as_deref(),
            ) {
                sample_n += 1;
                let segs = match st.cached_segments(&ses_block.header_hash) {
                    Some(segs) => segs,
                    None => {
                        // chia __create_persist_segment (weight_proof.py:288-297): the persisted
                        // store is checked BEFORE building; only a miss pays the sub-epoch block
                        // walk, and what it builds is persisted for every later build (and every
                        // later restart — segments below a served tip never change). The LRU
                        // above is the hot layer; the store is the durable one.
                        let got = match self
                            .store
                            .get_sub_epoch_segments(&ses_block.header_hash)
                            .await?
                        {
                            Some(bytes) => Arc::new(
                                SubEpochSegments::from_bytes(
                                    &mut Cursor::new(&bytes[..]),
                                    SEGMENT_STORE_VERSION,
                                )?
                                .challenge_segments,
                            ),
                            None => {
                                let sub_epoch_n = u32::try_from(sub_epoch_n).map_err(|_| {
                                    ServeError::Build("sub_epoch_n overflow".into())
                                })?;
                                let built = SubEpochSegments {
                                    challenge_segments: self
                                        .create_sub_epoch_segments(
                                            ses_block,
                                            &prev_ses_block,
                                            sub_epoch_n,
                                        )
                                        .await?,
                                };
                                // chia weight_proof.py:296-297 / block_store.py:164-171: persist
                                // the SubEpochSegments wrapper's bytes under the ses block hash.
                                self.store
                                    .persist_sub_epoch_segments(
                                        &ses_block.header_hash,
                                        &built.to_bytes(SEGMENT_STORE_VERSION)?,
                                    )
                                    .await?;
                                Arc::new(built.challenge_segments)
                            }
                        };
                        st.cache_segments(ses_block.header_hash, got.clone());
                        got
                    }
                };
                sub_epoch_segments.extend(segs.iter().cloned());
            }
            prev_ses_block = ses_block.clone();
        }

        debug!(sub_epochs = sub_epochs.len(), "sub_epochs");
        Ok(WeightProof {
            sub_epochs,
            sub_epoch_segments,
            recent_chain_data: recent_chain,
        })
    }

    // Extend the derived ses index up to `upto` (inclusive). The main chain below an already-walked
    // height is immutable for our purposes (ses blocks sit at least a sub-epoch below any served tip,
    // far deeper than tip-local reorgs), so the walk never re-visits.
    async fn extend_ses_index(&self, st: &mut ServeState, upto: u32) -> Result<(), ServeError> {
        let start = match st.walked_to {
            Some(w) if w >= upto => return Ok(()),
            Some(w) => w.saturating_add(1),
            None => 0,
        };
        for h in start..=upto {
            let rec = self
                .store
                .get_block_record_by_height(h)
                .await?
                .ok_or(ServeError::MissingBlock(h))?;
            if rec.sub_epoch_summary_included.is_some() {
                st.ses_blocks.push(rec);
            }
        }
        st.walked_to = Some(upto);
        Ok(())
    }

    /// chia `WeightProofHandler._get_recent_chain` (chia weight_proof.py:190-234): headers from the
    /// block BEFORE the second-to-last sub-epoch summary at-or-below the tip, up to the tip.
    async fn get_recent_chain(
        &self,
        ses_blocks: &[BlockRecord],
        tip_height: u32,
    ) -> Result<Vec<HeaderBlock>, ServeError> {
        // chia weight_proof.py:193-200 — min_height = (second ses at-or-below tip) - 1.
        let mut min_height = 0u32;
        let mut count_ses = 0usize;
        for b in ses_blocks.iter().rev() {
            if b.height <= tip_height {
                count_ses += 1;
            }
            if count_ses == 2 {
                min_height = b.height.saturating_sub(1);
                break;
            }
        }
        debug!(start = min_height, end = tip_height, "recent chain span");

        // chia weight_proof.py:202-203 — load the span's headers (tx_filter=False) and records. Every
        // height in the span must resolve (the reference asserts each height_to_hash).
        let span = usize::try_from(tip_height - min_height + 1)
            .map_err(|_| ServeError::Build("recent chain span overflow".into()))?;
        let mut headers: Vec<HeaderBlock> = Vec::with_capacity(span);
        let mut records: Vec<BlockRecord> = Vec::with_capacity(span);
        for h in min_height..=tip_height {
            let rec = self
                .store
                .get_block_record_by_height(h)
                .await?
                .ok_or(ServeError::MissingBlock(h))?;
            let block = self
                .store
                .get_block(&rec.header_hash)
                .await?
                .ok_or(ServeError::MissingBlock(h))?;
            headers.push(block.get_block_header());
            records.push(rec);
        }
        let at = |h: u32| usize::try_from(h - min_height).expect("span bounded above");

        // chia weight_proof.py:204-227 — walk down from the tip until two summaries are collected,
        // then prepend one more block (the block before the second summary).
        let mut recent_chain: VecDeque<HeaderBlock> = VecDeque::new();
        let mut ses_count = 0usize;
        let mut curr_height = tip_height;
        while ses_count < 2 {
            if curr_height == 0 {
                break;
            }
            recent_chain.push_front(headers[at(curr_height)].clone());
            if records[at(curr_height)]
                .sub_epoch_summary_included
                .is_some()
            {
                ses_count += 1;
            }
            curr_height -= 1;
        }
        recent_chain.push_front(headers[at(curr_height)].clone());

        info!(
            start = recent_chain
                .front()
                .map(HeaderBlock::height)
                .unwrap_or_default(),
            end = recent_chain
                .back()
                .map(HeaderBlock::height)
                .unwrap_or_default(),
            "recent chain"
        );
        Ok(recent_chain.into())
    }

    /// chia `WeightProofHandler.get_prev_two_slots_height` (chia weight_proof.py:336-352): the height
    /// two sub-slot starts below the sub-epoch start (the reference's 50-record batches are only its
    /// cache refill; point-gets are semantically identical).
    async fn get_prev_two_slots_height(&self, se_start: &BlockRecord) -> Result<u32, ServeError> {
        let mut slot = 0usize;
        let mut curr_rec = se_start.clone();
        while slot < 2 && curr_rec.height > 0 {
            if curr_rec.first_in_sub_slot() {
                slot += 1;
            }
            let h = curr_rec.height - 1;
            curr_rec = self
                .store
                .get_block_record_by_height(h)
                .await?
                .ok_or(ServeError::MissingBlock(h))?;
        }
        Ok(curr_rec.height)
    }

    // Load `[start, end]` as a ChainCache. Chia loads the same span twice (records + tx_filter=False
    // headers, chia weight_proof.py:305-310); one pass here. Heights past the peak simply don't resolve.
    async fn load_chain(&self, start: u32, end: u32) -> Result<ChainCache, ServeError> {
        let mut cache = ChainCache {
            height_to_hash: HashMap::new(),
            records: HashMap::new(),
            headers: HashMap::new(),
        };
        for h in start..=end {
            let Some(rec) = self.store.get_block_record_by_height(h).await? else {
                continue;
            };
            let Some(block) = self.store.get_block(&rec.header_hash).await? else {
                continue;
            };
            cache.height_to_hash.insert(h, rec.header_hash);
            cache
                .headers
                .insert(rec.header_hash, block.get_block_header());
            cache.records.insert(rec.header_hash, rec);
        }
        Ok(cache)
    }

    /// chia `WeightProofHandler.__create_sub_epoch_segments` (chia weight_proof.py:299-334): scan the
    /// sub-epoch's span for challenge blocks; each yields one challenge segment.
    async fn create_sub_epoch_segments(
        &self,
        ses_block: &BlockRecord,
        se_start: &BlockRecord,
        sub_epoch_n: u32,
    ) -> Result<Vec<SubEpochChallengeSegment>, ServeError> {
        let start_height = self.get_prev_two_slots_height(se_start).await?;
        let end_height = ses_block
            .height
            .saturating_add(self.constants.max_sub_slot_blocks);
        let cache = self.load_chain(start_height, end_height).await?;

        let mut segments: Vec<SubEpochChallengeSegment> = Vec::new();
        let mut curr_hash = se_start.header_hash;
        let mut height = se_start.height;
        let mut first = true;
        loop {
            let curr_height = cache.header(&curr_hash)?.height();
            if curr_height >= ses_block.height {
                break;
            }
            if cache
                .record(&curr_hash)?
                .is_challenge_block(self.constants.min_blocks_per_challenge_block)
            {
                debug!(
                    segment = segments.len(),
                    height = curr_height,
                    "challenge segment"
                );
                let (seg, end) =
                    self.create_challenge_segment(&cache, &curr_hash, sub_epoch_n, first)?;
                segments.push(seg);
                height = end;
                first = false;
            } else {
                height = height.saturating_add(1);
            }
            curr_hash = cache.hash_at(height)?;
        }
        debug!(next_sub_epoch_start = height, "sub epoch segments done");
        Ok(segments)
    }

    /// chia `WeightProofHandler._create_challenge_segment` (chia weight_proof.py:354-399).
    fn create_challenge_segment(
        &self,
        cache: &ChainCache,
        hh: &Bytes32,
        sub_epoch_n: u32,
        first_segment_in_sub_epoch: bool,
    ) -> Result<(SubEpochChallengeSegment, u32), ServeError> {
        let header_block = cache.header(hh)?;
        // VDFs from sub slots before the challenge block (chia weight_proof.py:366-373).
        let (mut sub_slots, first_rc_end_of_slot_vdf) =
            self.first_sub_slot_vdfs(cache, hh, first_segment_in_sub_epoch)?;
        // The challenge block's own VDFs (chia weight_proof.py:375-382).
        sub_slots.push(challenge_block_vdfs(&self.constants, cache, hh)?);
        // VDFs from the slot after the challenge block to end of slot (chia weight_proof.py:384-393).
        let (end_slots, end_height) =
            self.slot_end_vdf(cache, header_block.height().saturating_add(1))?;
        sub_slots.extend(end_slots);
        // chia weight_proof.py:394-399 — only a sub-epoch's first segment (past sub-epoch 0) carries
        // the first reward-chain end-of-slot VDF.
        let rc_slot_end_info = if first_segment_in_sub_epoch && sub_epoch_n != 0 {
            first_rc_end_of_slot_vdf
        } else {
            None
        };
        Ok((
            SubEpochChallengeSegment {
                sub_epoch_n,
                sub_slots,
                rc_slot_end_info,
            },
            end_height,
        ))
    }

    /// chia `WeightProofHandler.__first_sub_slot_vdfs` (chia weight_proof.py:402-476): the challenge
    /// chain VDFs from the segment's slot start up to (not including) the challenge block.
    fn first_sub_slot_vdfs(
        &self,
        cache: &ChainCache,
        hh: &Bytes32,
        first_in_sub_epoch: bool,
    ) -> Result<(Vec<SubSlotData>, Option<VdfInfo>), ServeError> {
        let header_block = cache.header(hh)?;
        let header_block_sub_rec = cache.record(hh)?;

        // Find the slot start (chia weight_proof.py:411-427).
        let mut curr_sub_rec = header_block_sub_rec;
        let mut first_rc_end_of_slot_vdf = None;
        if first_in_sub_epoch && curr_sub_rec.height > 0 {
            while curr_sub_rec.sub_epoch_summary_included.is_none() {
                curr_sub_rec = cache.record(&curr_sub_rec.prev_hash)?;
            }
            first_rc_end_of_slot_vdf = Some(self.first_rc_end_of_slot_vdf(cache, hh)?);
        } else if header_block_sub_rec.overflow && header_block_sub_rec.first_in_sub_slot() {
            let mut sub_slots_num = 2i64;
            while sub_slots_num > 0 && curr_sub_rec.height > 0 {
                if curr_sub_rec.first_in_sub_slot() {
                    let finished = curr_sub_rec
                        .finished_challenge_slot_hashes
                        .as_ref()
                        .ok_or_else(|| {
                            ServeError::Build(
                                "first_in_sub_slot without challenge slot hashes".into(),
                            )
                        })?;
                    sub_slots_num -= i64::try_from(finished.len())
                        .map_err(|_| ServeError::Build("slot hash count overflow".into()))?;
                }
                curr_sub_rec = cache.record(&curr_sub_rec.prev_hash)?;
            }
        } else {
            while !curr_sub_rec.first_in_sub_slot() && curr_sub_rec.height > 0 {
                curr_sub_rec = cache.record(&curr_sub_rec.prev_hash)?;
            }
        }

        // Collect per-block ip VDFs + finished-slot VDFs up to the challenge block
        // (chia weight_proof.py:429-468).
        let mut curr = cache.header(&curr_sub_rec.header_hash)?;
        let mut sub_slots_data: Vec<SubSlotData> = Vec::new();
        let mut tmp_sub_slots_data: Vec<SubSlotData> = Vec::new();
        while curr.height() < header_block.height() {
            if curr.first_in_sub_slot() {
                // If not blue boxed, keep the collected block VDFs (chia weight_proof.py:437-439).
                let first_slot = curr
                    .finished_sub_slots
                    .first()
                    .ok_or_else(|| ServeError::Build("first_in_sub_slot without slots".into()))?;
                if !blue_boxed_end_of_slot(first_slot) {
                    sub_slots_data.append(&mut tmp_sub_slots_data);
                }
                for sub_slot in &curr.finished_sub_slots {
                    let curr_icc_info = sub_slot
                        .infused_challenge_chain
                        .map(|icc| icc.infused_challenge_chain_end_of_slot_vdf);
                    sub_slots_data.push(handle_finished_slots(sub_slot, curr_icc_info));
                }
                tmp_sub_slots_data.clear();
            }
            // chia weight_proof.py:447-462 — a bare ip-VDF entry per block.
            tmp_sub_slots_data.push(SubSlotData {
                proof_of_space: None,
                cc_signage_point: None,
                cc_infusion_point: None,
                icc_infusion_point: None,
                cc_sp_vdf_info: None,
                signage_point_index: Some(curr.reward_chain_block.signage_point_index),
                cc_slot_end: None,
                icc_slot_end: None,
                cc_slot_end_info: None,
                icc_slot_end_info: None,
                cc_ip_vdf_info: Some(curr.reward_chain_block.challenge_chain_ip_vdf),
                icc_ip_vdf_info: curr.reward_chain_block.infused_challenge_chain_ip_vdf,
                total_iters: Some(curr.total_iters()),
            });
            curr = cache.header_at(curr.height().saturating_add(1))?;
        }

        if !tmp_sub_slots_data.is_empty() {
            sub_slots_data.append(&mut tmp_sub_slots_data);
        }

        // The challenge block's own finished slots (chia weight_proof.py:470-474).
        for sub_slot in &header_block.finished_sub_slots {
            let curr_icc_info = sub_slot
                .infused_challenge_chain
                .map(|icc| icc.infused_challenge_chain_end_of_slot_vdf);
            sub_slots_data.push(handle_finished_slots(sub_slot, curr_icc_info));
        }
        Ok((sub_slots_data, first_rc_end_of_slot_vdf))
    }

    /// chia `WeightProofHandler.first_rc_end_of_slot_vdf` (chia weight_proof.py:478-487): the
    /// reward-chain end-of-slot VDF of the sub-epoch's opening slot (found by walking back to the
    /// ses-carrying block).
    fn first_rc_end_of_slot_vdf(
        &self,
        cache: &ChainCache,
        hh: &Bytes32,
    ) -> Result<VdfInfo, ServeError> {
        let mut curr = cache.record(hh)?;
        while curr.height > 0 && curr.sub_epoch_summary_included.is_none() {
            curr = cache.record(&curr.prev_hash)?;
        }
        let header = cache.header(&curr.header_hash)?;
        Ok(header
            .finished_sub_slots
            .last()
            .ok_or_else(|| ServeError::Build("ses block without finished sub slots".into()))?
            .reward_chain
            .end_of_slot_vdf)
    }

    /// chia `WeightProofHandler.__slot_end_vdf` (chia weight_proof.py:489-522): all VDFs from the
    /// first sub slot after the challenge block through the last sub slot before the next challenge
    /// block. Returns the collected entries and the next challenge block's height.
    fn slot_end_vdf(
        &self,
        cache: &ChainCache,
        start_height: u32,
    ) -> Result<(Vec<SubSlotData>, u32), ServeError> {
        debug!(start_height, "slot end vdf");
        let mut curr = cache.header_at(start_height)?;
        let mut curr_header_hash = cache.hash_at(start_height)?;
        let mut sub_slots_data: Vec<SubSlotData> = Vec::new();
        let mut tmp_sub_slots_data: Vec<SubSlotData> = Vec::new();
        while !cache
            .record(&curr_header_hash)?
            .is_challenge_block(self.constants.min_blocks_per_challenge_block)
        {
            if curr.first_in_sub_slot() {
                sub_slots_data.append(&mut tmp_sub_slots_data);
                // Collected end-of-slot VDFs (chia weight_proof.py:504-512).
                let curr_prev_header_hash = curr.prev_header_hash();
                for (idx, sub_slot) in curr.finished_sub_slots.iter().enumerate() {
                    let prev_rec = cache.record(&curr_prev_header_hash)?;
                    let eos_vdf_iters = if idx == 0 {
                        prev_rec
                            .sub_slot_iters
                            .checked_sub(prev_rec.ip_iters(&self.constants)?)
                            .ok_or_else(|| ServeError::Build("eos_vdf_iters underflow".into()))?
                    } else {
                        prev_rec.sub_slot_iters
                    };
                    sub_slots_data.push(handle_end_of_slot(sub_slot, eos_vdf_iters)?);
                }
                tmp_sub_slots_data.clear();
            }
            tmp_sub_slots_data.push(handle_block_vdfs(&self.constants, cache, curr)?);
            let next_height = curr.height().saturating_add(1);
            curr = cache.header_at(next_height)?;
            curr_header_hash = cache.hash_at(next_height)?;
        }

        if !tmp_sub_slots_data.is_empty() {
            sub_slots_data.append(&mut tmp_sub_slots_data);
        }
        debug!(
            end_height = curr.height(),
            slots = sub_slots_data.len(),
            "slot end vdf done"
        );
        Ok((sub_slots_data, curr.height()))
    }
}

/// chia `WeightProofHandler.handle_block_vdfs` (chia weight_proof.py:524-568): one non-challenge
/// block's signage/infusion-point VDFs, with the cc-sp iteration count recomputed from
/// `get_signage_point_vdf_info` for non-normalized proofs.
fn handle_block_vdfs(
    constants: &ConsensusConstants,
    cache: &ChainCache,
    curr: &HeaderBlock,
) -> Result<SubSlotData, ServeError> {
    let block_record = cache.record(&curr.header_hash()?)?;

    let mut icc_ip_proof = None;
    let mut icc_ip_info = None;
    if curr.infused_challenge_chain_ip_proof.is_some() {
        let info = curr
            .reward_chain_block
            .infused_challenge_chain_ip_vdf
            .ok_or_else(|| ServeError::Build("icc ip proof without icc ip vdf".into()))?;
        icc_ip_proof = curr.infused_challenge_chain_ip_proof.clone();
        icc_ip_info = Some(info);
    }

    let mut cc_sp_proof = None;
    let mut cc_sp_info = None;
    if let Some(sp_proof) = &curr.challenge_chain_sp_proof {
        let sp_vdf = curr
            .reward_chain_block
            .challenge_chain_sp_vdf
            .ok_or_else(|| ServeError::Build("cc sp proof without cc sp vdf".into()))?;
        let mut cc_sp_vdf_info = sp_vdf;
        if !sp_proof.normalized_to_identity {
            let prev_b = if curr.height() == 0 {
                None
            } else {
                Some(cache.record(&curr.prev_header_hash())?)
            };
            let (_, _, _, _, cc_vdf_iters, _) = get_signage_point_vdf_info(
                constants,
                &curr.finished_sub_slots,
                block_record.overflow,
                prev_b,
                &cache.records,
                block_record.sp_total_iters(constants)?,
                block_record.sp_iters(constants)?,
            )?;
            cc_sp_vdf_info = VdfInfo {
                challenge: sp_vdf.challenge,
                number_of_iterations: cc_vdf_iters,
                output: sp_vdf.output,
            };
        }
        cc_sp_proof = Some(sp_proof.clone());
        cc_sp_info = Some(cc_sp_vdf_info);
    }

    Ok(SubSlotData {
        proof_of_space: None,
        cc_signage_point: cc_sp_proof,
        cc_infusion_point: Some(curr.challenge_chain_ip_proof.clone()),
        icc_infusion_point: icc_ip_proof,
        cc_sp_vdf_info: cc_sp_info,
        signage_point_index: Some(curr.reward_chain_block.signage_point_index),
        cc_slot_end: None,
        icc_slot_end: None,
        cc_slot_end_info: None,
        icc_slot_end_info: None,
        cc_ip_vdf_info: Some(curr.reward_chain_block.challenge_chain_ip_vdf),
        icc_ip_vdf_info: icc_ip_info,
        total_iters: Some(curr.total_iters()),
    })
}

/// chia `_challenge_block_vdfs` (chia weight_proof.py:728-769): the challenge block's entry — proof of
/// space plus its signage/infusion-point VDFs.
fn challenge_block_vdfs(
    constants: &ConsensusConstants,
    cache: &ChainCache,
    hh: &Bytes32,
) -> Result<SubSlotData, ServeError> {
    let header_block = cache.header(hh)?;
    let block_rec = cache.record(hh)?;
    let prev_b = if header_block.height() == 0 {
        None
    } else {
        Some(cache.record(&header_block.prev_header_hash())?)
    };
    // chia weight_proof.py:734-742 — always recomputed, used only for the non-normalized cc-sp info.
    let (_, _, _, _, cc_vdf_iters, _) = get_signage_point_vdf_info(
        constants,
        &header_block.finished_sub_slots,
        block_rec.overflow,
        prev_b,
        &cache.records,
        block_rec.sp_total_iters(constants)?,
        block_rec.sp_iters(constants)?,
    )?;

    let mut cc_sp_info = None;
    if let Some(sp_vdf) = &header_block.reward_chain_block.challenge_chain_sp_vdf {
        cc_sp_info = Some(*sp_vdf);
        let sp_proof = header_block
            .challenge_chain_sp_proof
            .as_ref()
            .ok_or_else(|| ServeError::Build("cc sp vdf without cc sp proof".into()))?;
        if !sp_proof.normalized_to_identity {
            cc_sp_info = Some(VdfInfo {
                challenge: sp_vdf.challenge,
                number_of_iterations: cc_vdf_iters,
                output: sp_vdf.output,
            });
        }
    }
    Ok(SubSlotData {
        proof_of_space: Some(header_block.reward_chain_block.proof_of_space.clone()),
        cc_signage_point: header_block.challenge_chain_sp_proof.clone(),
        cc_infusion_point: Some(header_block.challenge_chain_ip_proof.clone()),
        icc_infusion_point: None,
        cc_sp_vdf_info: cc_sp_info,
        signage_point_index: Some(header_block.reward_chain_block.signage_point_index),
        cc_slot_end: None,
        icc_slot_end: None,
        cc_slot_end_info: None,
        icc_slot_end_info: None,
        cc_ip_vdf_info: Some(header_block.reward_chain_block.challenge_chain_ip_vdf),
        icc_ip_vdf_info: header_block
            .reward_chain_block
            .infused_challenge_chain_ip_vdf,
        total_iters: Some(block_rec.total_iters),
    })
}

/// chia `handle_finished_slots` (chia weight_proof.py:772-795): a finished sub slot as a slot-end entry.
fn handle_finished_slots(
    end_of_slot: &EndOfSubSlotBundle,
    icc_end_of_slot_info: Option<VdfInfo>,
) -> SubSlotData {
    SubSlotData {
        proof_of_space: None,
        cc_signage_point: None,
        cc_infusion_point: None,
        icc_infusion_point: None,
        cc_sp_vdf_info: None,
        signage_point_index: None,
        cc_slot_end: Some(end_of_slot.proofs.challenge_chain_slot_proof.clone()),
        icc_slot_end: end_of_slot
            .proofs
            .infused_challenge_chain_slot_proof
            .clone(),
        cc_slot_end_info: Some(end_of_slot.challenge_chain.challenge_chain_end_of_slot_vdf),
        icc_slot_end_info: icc_end_of_slot_info,
        cc_ip_vdf_info: None,
        icc_ip_vdf_info: None,
        total_iters: None,
    }
}

/// chia `handle_end_of_slot` (chia weight_proof.py:798-836): a collected end-of-slot entry with the
/// cc/icc infos rewritten to the true eos iteration count unless the proofs are normalized.
fn handle_end_of_slot(
    sub_slot: &EndOfSubSlotBundle,
    eos_vdf_iters: u64,
) -> Result<SubSlotData, ServeError> {
    // The reference asserts both the icc chain and its proof exist here.
    let icc = sub_slot
        .infused_challenge_chain
        .as_ref()
        .ok_or_else(|| ServeError::Build("end of slot without infused challenge chain".into()))?;
    let icc_proof = sub_slot
        .proofs
        .infused_challenge_chain_slot_proof
        .as_ref()
        .ok_or_else(|| ServeError::Build("end of slot without icc slot proof".into()))?;
    let icc_info = if icc_proof.normalized_to_identity {
        icc.infused_challenge_chain_end_of_slot_vdf
    } else {
        VdfInfo {
            challenge: icc.infused_challenge_chain_end_of_slot_vdf.challenge,
            number_of_iterations: eos_vdf_iters,
            output: icc.infused_challenge_chain_end_of_slot_vdf.output,
        }
    };
    let cc_info = if sub_slot
        .proofs
        .challenge_chain_slot_proof
        .normalized_to_identity
    {
        sub_slot.challenge_chain.challenge_chain_end_of_slot_vdf
    } else {
        VdfInfo {
            challenge: sub_slot
                .challenge_chain
                .challenge_chain_end_of_slot_vdf
                .challenge,
            number_of_iterations: eos_vdf_iters,
            output: sub_slot
                .challenge_chain
                .challenge_chain_end_of_slot_vdf
                .output,
        }
    };
    Ok(SubSlotData {
        proof_of_space: None,
        cc_signage_point: None,
        cc_infusion_point: None,
        icc_infusion_point: None,
        cc_sp_vdf_info: None,
        signage_point_index: None,
        cc_slot_end: Some(sub_slot.proofs.challenge_chain_slot_proof.clone()),
        icc_slot_end: Some(icc_proof.clone()),
        cc_slot_end_info: Some(cc_info),
        icc_slot_end_info: Some(icc_info),
        cc_ip_vdf_info: None,
        icc_ip_vdf_info: None,
        total_iters: None,
    })
}

/// chia `blue_boxed_end_of_slot` (chia weight_proof.py:1631-1638): both slot proofs normalized.
fn blue_boxed_end_of_slot(sub_slot: &EndOfSubSlotBundle) -> bool {
    sub_slot
        .proofs
        .challenge_chain_slot_proof
        .normalized_to_identity
        && sub_slot
            .proofs
            .infused_challenge_chain_slot_proof
            .as_ref()
            .is_none_or(|p| p.normalized_to_identity)
}

/// chia `_create_sub_epoch_data` (chia weight_proof.py:716-725).
fn create_sub_epoch_data(ses: &SubEpochSummary) -> SubEpochData {
    SubEpochData {
        reward_chain_hash: ses.reward_chain_hash,
        num_blocks_overflow: ses.num_blocks_overflow,
        new_sub_slot_iters: ses.new_sub_slot_iters,
        new_difficulty: ses.new_difficulty,
    }
}

/// chia `WeightProofHandler.get_seed_for_proof` (chia weight_proof.py:177-188): the hash of the
/// SECOND-TO-LAST sub-epoch summary at-or-below the tip.
fn get_seed_for_proof(ses_blocks: &[BlockRecord], tip_height: u32) -> Result<Bytes32, ServeError> {
    let mut count = 0usize;
    for b in ses_blocks.iter().rev() {
        if b.height <= tip_height {
            count += 1;
        }
        if count == 2 {
            let ses = b
                .sub_epoch_summary_included
                .as_ref()
                .ok_or_else(|| ServeError::Build("ses block without summary".into()))?;
            return Ok(ses.hash()?);
        }
    }
    Err(ServeError::NotEnoughSubEpochs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use dg_xch_core::blockchain::challenge_chain_subslot::ChallengeChainSubSlot;
    use dg_xch_core::blockchain::class_group_element::ClassgroupElement;
    use dg_xch_core::blockchain::foliage::Foliage;
    use dg_xch_core::blockchain::foliage_block_data::FoliageBlockData;
    use dg_xch_core::blockchain::full_block::FullBlock;
    use dg_xch_core::blockchain::infused_challenge_chain_subslot::InfusedChallengeChainSubSlot;
    use dg_xch_core::blockchain::pool_target::PoolTarget;
    use dg_xch_core::blockchain::proof_of_space::ProofOfSpace;
    use dg_xch_core::blockchain::reward_chain_block::RewardChainBlock;
    use dg_xch_core::blockchain::reward_chain_subslot::RewardChainSubSlot;
    use dg_xch_core::blockchain::sized_bytes::{Bytes48, Bytes96};
    use dg_xch_core::blockchain::subslot_bundle::SubSlotBundle;
    use dg_xch_core::blockchain::subslot_proofs::SubSlotProofs;
    use dg_xch_core::blockchain::unsized_bytes::UnsizedBytes;
    use dg_xch_core::blockchain::vdf_proof::VdfProof;
    use dg_xch_core::clvm::program::SerializedProgram;
    use dg_xch_core::consensus::constants::MAINNET;
    use dg_xch_stores::types::{BatchHandle, BlockStatus, Savepoint};
    use dg_xch_stores::{BlockStore, StoreError};
    use std::collections::HashMap;

    // An in-memory BlockStore double: exactly the query surface the builder touches
    // (get_block_record / get_block_record_by_height / get_block, plus the persisted-segment
    // seam); everything else errors. `block_gets`/`segment_persists` count store traffic so the
    // tests can assert a rebuild served from persisted segments instead of walking blocks.
    struct MemStore {
        by_height: HashMap<u32, BlockRecord>,
        by_hash: HashMap<Bytes32, BlockRecord>,
        blocks: HashMap<Bytes32, FullBlock>,
        segments: std::sync::Mutex<HashMap<Bytes32, Vec<u8>>>,
        block_gets: std::sync::atomic::AtomicUsize,
        segment_persists: std::sync::atomic::AtomicUsize,
    }

    fn unsupported() -> StoreError {
        StoreError::Corrupt("unsupported in MemStore".into())
    }

    #[async_trait]
    impl BlockStore for MemStore {
        async fn get_block_record(&self, hh: &Bytes32) -> Result<Option<BlockRecord>, StoreError> {
            Ok(self.by_hash.get(hh).cloned())
        }
        async fn get_block_record_by_height(
            &self,
            h: u32,
        ) -> Result<Option<BlockRecord>, StoreError> {
            Ok(self.by_height.get(&h).cloned())
        }
        async fn get_peak(&self) -> Result<Option<(Bytes32, u32)>, StoreError> {
            Ok(self
                .by_height
                .keys()
                .max()
                .and_then(|h| self.by_height.get(h).map(|r| (r.header_hash, r.height))))
        }
        async fn min_record_height(&self) -> Result<Option<u32>, StoreError> {
            Ok(self.by_height.keys().min().copied())
        }
        async fn get_block(&self, hh: &Bytes32) -> Result<Option<FullBlock>, StoreError> {
            self.block_gets
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(self.blocks.get(hh).cloned())
        }
        async fn get_sub_epoch_segments(
            &self,
            ses_hash: &Bytes32,
        ) -> Result<Option<Vec<u8>>, StoreError> {
            Ok(self
                .segments
                .lock()
                .expect("segments")
                .get(ses_hash)
                .cloned())
        }
        async fn persist_sub_epoch_segments(
            &self,
            ses_hash: &Bytes32,
            bytes: &[u8],
        ) -> Result<(), StoreError> {
            self.segment_persists
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.segments
                .lock()
                .expect("segments")
                .insert(*ses_hash, bytes.to_vec());
            Ok(())
        }
        async fn add_block_records(&self, _records: &[BlockRecord]) -> Result<(), StoreError> {
            Err(unsupported())
        }
        async fn add_block_records_in(
            &self,
            _batch: &mut BatchHandle,
            _records: &[BlockRecord],
        ) -> Result<(), StoreError> {
            Err(unsupported())
        }
        async fn begin(&self) -> Result<BatchHandle, StoreError> {
            Err(unsupported())
        }
        async fn append_many(
            &self,
            _batch: &mut BatchHandle,
            _blocks: &[FullBlock],
        ) -> Result<(), StoreError> {
            Err(unsupported())
        }
        async fn commit(&self, _batch: BatchHandle) -> Result<(), StoreError> {
            Err(unsupported())
        }
        async fn get_unassociated(&self, _limit: usize) -> Result<Vec<u32>, StoreError> {
            Ok(Vec::new())
        }
        async fn set_peak(&self, _new_peak: &Bytes32) -> Result<u64, StoreError> {
            Err(unsupported())
        }
        async fn set_peak_in(
            &self,
            _batch: &mut BatchHandle,
            _new_peak: &Bytes32,
        ) -> Result<u64, StoreError> {
            Err(unsupported())
        }
        async fn get_status(&self, _hh: &Bytes32) -> Result<BlockStatus, StoreError> {
            Err(unsupported())
        }
        async fn set_status(&self, _hh: &Bytes32, _s: BlockStatus) -> Result<(), StoreError> {
            Err(unsupported())
        }
        async fn set_status_in(
            &self,
            _batch: &mut BatchHandle,
            _hh: &Bytes32,
            _s: BlockStatus,
        ) -> Result<(), StoreError> {
            Err(unsupported())
        }
        async fn savepoint(&self) -> Result<Savepoint, StoreError> {
            Err(unsupported())
        }
        async fn rollback(&self, _sp: Savepoint) -> Result<u64, StoreError> {
            Err(unsupported())
        }
        async fn get_generator_at_height(
            &self,
            _h: u32,
        ) -> Result<Option<SerializedProgram>, StoreError> {
            Ok(None)
        }
        async fn build_indexes(&self) -> Result<(), StoreError> {
            Ok(())
        }
    }

    fn h32(n: u32) -> Bytes32 {
        let mut b = [0u8; 32];
        b[..4].copy_from_slice(&n.to_be_bytes());
        b[4] = 0xab; // distinguish from Bytes32::default()
        Bytes32::from(b)
    }

    fn zero_vdf_info() -> VdfInfo {
        VdfInfo {
            challenge: Bytes32::default(),
            number_of_iterations: 1,
            output: ClassgroupElement::get_default_element(),
        }
    }

    fn zero_proof() -> VdfProof {
        VdfProof {
            witness_type: 0,
            witness: UnsizedBytes::new(Vec::new()),
            normalized_to_identity: false,
        }
    }

    fn slot_bundle() -> SubSlotBundle {
        SubSlotBundle {
            challenge_chain: ChallengeChainSubSlot {
                challenge_chain_end_of_slot_vdf: zero_vdf_info(),
                infused_challenge_chain_sub_slot_hash: None,
                subepoch_summary_hash: None,
                new_sub_slot_iters: None,
                new_difficulty: None,
            },
            infused_challenge_chain: Some(InfusedChallengeChainSubSlot {
                infused_challenge_chain_end_of_slot_vdf: zero_vdf_info(),
            }),
            reward_chain: RewardChainSubSlot {
                end_of_slot_vdf: zero_vdf_info(),
                challenge_chain_sub_slot_hash: Bytes32::default(),
                infused_challenge_chain_sub_slot_hash: None,
                deficit: MAINNET.min_blocks_per_challenge_block,
            },
            proofs: SubSlotProofs {
                challenge_chain_slot_proof: zero_proof(),
                infused_challenge_chain_slot_proof: Some(zero_proof()),
                reward_chain_slot_proof: zero_proof(),
            },
        }
    }

    fn ses(n: u8) -> SubEpochSummary {
        SubEpochSummary {
            prev_subepoch_summary_hash: h32(u32::from(n)),
            reward_chain_hash: h32(1000 + u32::from(n)),
            num_blocks_overflow: n,
            new_difficulty: None,
            new_sub_slot_iters: None,
        }
    }

    // One synthetic main-chain block: unique foliage (hence header hash) per height, weight = height+1,
    // total_iters = (height+1) * 10_000_000 (far above any ip/sp iters the constants derive), slot
    // starts and ses carriers as flagged. Structurally consistent for CONSTRUCTION (the builder never
    // verifies VDFs/PoSpace — validation-grade chains need real proofs and are gated behind a corpus).
    struct ChainSpec {
        len: u32,
        slot_every: u32,
        ses_heights: Vec<(u32, SubEpochSummary)>,
        challenge_heights: Vec<u32>,
    }

    fn build_chain(spec: &ChainSpec) -> MemStore {
        let mut store = MemStore {
            by_height: HashMap::new(),
            by_hash: HashMap::new(),
            blocks: HashMap::new(),
            segments: std::sync::Mutex::new(HashMap::new()),
            block_gets: std::sync::atomic::AtomicUsize::new(0),
            segment_persists: std::sync::atomic::AtomicUsize::new(0),
        };
        let mut prev_hash = Bytes32::default();
        for h in 0..spec.len {
            let first_in_sub_slot = h > 0 && h.is_multiple_of(spec.slot_every);
            let ses_included = spec
                .ses_heights
                .iter()
                .find(|(sh, _)| *sh == h)
                .map(|(_, s)| *s);
            let is_challenge = spec.challenge_heights.contains(&h);
            // A ses-carrying block is the first block of the slot that commits the summary: its
            // opening finished sub slot carries `subepoch_summary_hash = hash(ses)` on chain. The
            // validator's `_get_last_ses_hash` mirror reads exactly that commitment, so ses heights
            // must be slot starts here (as on the real chain).
            let finished_sub_slots = if first_in_sub_slot {
                let mut bundle = slot_bundle();
                if let Some(s) = &ses_included {
                    bundle.challenge_chain.subepoch_summary_hash =
                        Some(s.hash().expect("hash test ses"));
                }
                vec![bundle]
            } else {
                assert!(
                    ses_included.is_none(),
                    "test chain: ses heights must be slot starts"
                );
                Vec::new()
            };
            let block = FullBlock {
                finished_sub_slots,
                reward_chain_block: RewardChainBlock {
                    weight: u128::from(h) + 1,
                    height: h,
                    total_iters: (u128::from(h) + 1) * 10_000_000,
                    signage_point_index: 0,
                    pos_ss_cc_challenge_hash: Bytes32::default(),
                    proof_of_space: ProofOfSpace {
                        challenge: Bytes32::default(),
                        pool_public_key: None,
                        pool_contract_puzzle_hash: Some(Bytes32::default()),
                        plot_public_key: Bytes48::default(),
                        size: 32,
                        proof: Vec::new().into(),
                    },
                    challenge_chain_sp_vdf: None,
                    challenge_chain_sp_signature: Bytes96::default(),
                    challenge_chain_ip_vdf: zero_vdf_info(),
                    reward_chain_sp_vdf: None,
                    reward_chain_sp_signature: Bytes96::default(),
                    reward_chain_ip_vdf: zero_vdf_info(),
                    infused_challenge_chain_ip_vdf: None,
                    is_transaction_block: false,
                },
                challenge_chain_sp_proof: None,
                challenge_chain_ip_proof: zero_proof(),
                reward_chain_sp_proof: None,
                reward_chain_ip_proof: zero_proof(),
                infused_challenge_chain_ip_proof: None,
                foliage: Foliage {
                    prev_block_hash: prev_hash,
                    reward_block_hash: h32(h), // unique foliage → unique header hash
                    foliage_block_data: FoliageBlockData {
                        unfinished_reward_block_hash: Bytes32::default(),
                        pool_target: PoolTarget {
                            puzzle_hash: Bytes32::default(),
                            max_height: 0,
                        },
                        pool_signature: None,
                        farmer_reward_puzzle_hash: Bytes32::default(),
                        extension_data: Bytes32::default(),
                    },
                    foliage_block_data_signature: Bytes96::default(),
                    foliage_transaction_block_hash: None,
                    foliage_transaction_block_signature: None,
                },
                foliage_transaction_block: None,
                transactions_info: None,
                transactions_generator: None,
                transactions_generator_ref_list: Vec::new(),
            };
            let header_hash = block.header_hash().expect("hash fake block");
            let record = BlockRecord {
                header_hash,
                prev_hash,
                height: h,
                weight: u128::from(h) + 1,
                total_iters: (u128::from(h) + 1) * 10_000_000,
                signage_point_index: 0,
                challenge_vdf_output: ClassgroupElement::get_default_element(),
                infused_challenge_vdf_output: None,
                reward_infusion_new_challenge: Bytes32::default(),
                challenge_block_info_hash: Bytes32::default(),
                sub_slot_iters: MAINNET.sub_slot_iters_starting,
                pool_puzzle_hash: Bytes32::default(),
                farmer_puzzle_hash: Bytes32::default(),
                required_iters: 1,
                deficit: if is_challenge {
                    MAINNET.min_blocks_per_challenge_block - 1
                } else {
                    MAINNET.min_blocks_per_challenge_block
                },
                overflow: false,
                prev_transaction_block_height: 0,
                timestamp: None,
                prev_transaction_block_hash: None,
                fees: None,
                reward_claims_incorporated: None,
                finished_challenge_slot_hashes: if first_in_sub_slot {
                    Some(vec![h32(2000 + h)])
                } else {
                    None
                },
                finished_infused_challenge_slot_hashes: None,
                finished_reward_slot_hashes: if first_in_sub_slot {
                    Some(vec![h32(3000 + h)])
                } else {
                    None
                },
                sub_epoch_summary_included: ses_included,
            };
            store.by_height.insert(h, record.clone());
            store.by_hash.insert(header_hash, record);
            store.blocks.insert(header_hash, block);
            prev_hash = header_hash;
        }
        store
    }

    fn server_over(spec: &ChainSpec) -> WeightProofServer<MemStore> {
        WeightProofServer::new(Arc::new(build_chain(spec)), MAINNET)
    }

    // Red-first, hand-derived from chia full_node_api.py:362-364: a tip we do not hold is refused
    // (no reply) — never built, never a panic.
    #[tokio::test]
    async fn refuses_unknown_tip() {
        let server = server_over(&ChainSpec {
            len: 10,
            slot_every: 4,
            ses_heights: vec![],
            challenge_heights: vec![],
        });
        let err = server
            .get_proof_of_weight(h32(999_999))
            .await
            .expect_err("unknown tip must refuse");
        assert!(matches!(err, ServeError::UnknownTip(_)));
        assert!(err.is_refusal());
    }

    // Red-first, hand-derived from chia weight_proof.py:86-88: a known tip below
    // WEIGHT_PROOF_RECENT_BLOCKS (mainnet 1000) is refused.
    #[tokio::test]
    async fn refuses_chain_shorter_than_weight_proof_recent_blocks() {
        let spec = ChainSpec {
            len: 500,
            slot_every: 10,
            ses_heights: vec![(400, ses(0))],
            challenge_heights: vec![],
        };
        let store = build_chain(&spec);
        let tip = store.by_height[&499].header_hash;
        let server = WeightProofServer::new(Arc::new(store), MAINNET);
        let err = server
            .get_proof_of_weight(tip)
            .await
            .expect_err("short chain must refuse");
        assert!(
            matches!(
                err,
                ServeError::ChainTooShort {
                    height: 499,
                    required: 1000
                }
            ),
            "got {err:?}"
        );
        assert!(err.is_refusal());
    }

    // Red-first, hand-derived from chia weight_proof.py:190-234: for ses blocks at 400 and 800 and
    // tip 1050, min_height = 800-1 … no — the walk collects TWO summaries going down (800 then 400),
    // so the chain must span from the block BEFORE the second summary (height 399) to the tip. Every
    // header must carry the tx_filter=False empty BIP158 filter byte (generator_tools.py:13-51).
    #[tokio::test]
    async fn recent_chain_spans_block_before_second_last_ses_to_tip() {
        let spec = ChainSpec {
            len: 1051,
            slot_every: 10,
            ses_heights: vec![(400, ses(0)), (800, ses(1))],
            challenge_heights: vec![],
        };
        let store = build_chain(&spec);
        let ses_blocks = vec![store.by_height[&400].clone(), store.by_height[&800].clone()];
        let server = WeightProofServer::new(Arc::new(store), MAINNET);
        let chain = server
            .get_recent_chain(&ses_blocks, 1050)
            .await
            .expect("recent chain builds");
        assert_eq!(chain.first().map(HeaderBlock::height), Some(399));
        assert_eq!(chain.last().map(HeaderBlock::height), Some(1050));
        assert_eq!(chain.len(), 652);
        for header in &chain {
            assert_eq!(
                header.transactions_filter.as_slice(),
                &[0u8],
                "tx_filter=False headers carry the one-byte empty BIP158 filter"
            );
        }
    }

    // Red-first, hand-derived from chia weight_proof.py:336-352: from a start at height 10 with slot
    // starts at 8 and 4, the walk counts the two slot starts and STILL steps below the second — the
    // reference assigns `curr_rec = blocks[height-1]` after the count, so the answer is 3, not 4.
    #[tokio::test]
    async fn prev_two_slots_height_steps_below_the_second_slot_start() {
        let spec = ChainSpec {
            len: 12,
            slot_every: 4,
            ses_heights: vec![],
            challenge_heights: vec![],
        };
        let store = build_chain(&spec);
        let se_start = store.by_height[&10].clone();
        let server = WeightProofServer::new(Arc::new(store), MAINNET);
        assert_eq!(
            server
                .get_prev_two_slots_height(&se_start)
                .await
                .expect("walk"),
            3
        );
    }

    // Red-first, hand-derived from chia weight_proof.py:177-188: the sampling seed is the hash of
    // the SECOND-TO-LAST summary at-or-below the tip — for summaries at 400/800/1200 and tip 1050,
    // that is the summary at 400 (1200 is above the tip and must not count).
    #[test]
    fn seed_is_hash_of_second_to_last_summary_at_or_below_tip() {
        let spec = ChainSpec {
            len: 1251,
            slot_every: 10,
            ses_heights: vec![(400, ses(0)), (800, ses(1)), (1200, ses(2))],
            challenge_heights: vec![],
        };
        let store = build_chain(&spec);
        let ses_blocks = vec![
            store.by_height[&400].clone(),
            store.by_height[&800].clone(),
            store.by_height[&1200].clone(),
        ];
        let seed = get_seed_for_proof(&ses_blocks, 1050).expect("seed");
        assert_eq!(seed, ses(0).hash().expect("hash"));
    }

    // The construction smoke test on a synthetic minimal chain, hand-derived from the reference:
    //
    // Chain: 1101 blocks, slot starts every 10, summaries at 400 (ses0) and 800 (ses1), challenge
    // blocks (deficit 15) at 405 and 801, tip 1100. Weights grow by 1 per block, so the recent chain
    // (399..=1100) spans 702/1101 of the total weight: delta≈0.64 ⇒ prob_of_adv_succeeding =
    // 1 - ln(0.5)/ln(delta) < 0 ⇒ `_get_weights_for_sampling` returns None ⇒ EVERY sub-epoch is
    // sampled (chia weight_proof.py:667-684, 687-712 — the `weight_to_check is None` short-circuit).
    //
    // Expected segments, walked by hand through weight_proof.py:299-522:
    // - sub-epoch 0 (heights 0..400): no challenge blocks ⇒ zero segments.
    // - sub-epoch 1 (heights 400..800): one challenge block at 405 ⇒ ONE segment, sub_epoch_n=1,
    //   and — being the sub-epoch's first segment past sub-epoch 0 — it carries rc_slot_end_info
    //   (weight_proof.py:394-399), which is ses0's slot-opening reward-chain end-of-slot VDF
    //   (weight_proof.py:478-487).
    //   Its sub_slots, in order:
    //   · 1 finished-slot entry for slot start 400 (weight_proof.py:441-445)
    //   · 5 bare ip entries for blocks 400..=404 (weight_proof.py:447-462)
    //   · 1 challenge-block entry (proof of space present; weight_proof.py:728-769)
    //   · then __slot_end_vdf over 406..=800 until the next challenge block at 801
    //     (weight_proof.py:489-522): a block entry per height (395) plus an end-of-slot entry per
    //     slot start 410,420,…,800 (40) = 435
    //   ⇒ 442 sub_slots total, exactly one carrying a proof of space, 41 carrying cc_slot_end.
    #[tokio::test]
    async fn builds_one_segment_per_challenge_block_with_hand_derived_shape() {
        let spec = ChainSpec {
            len: 1101,
            slot_every: 10,
            ses_heights: vec![(400, ses(0)), (800, ses(1))],
            challenge_heights: vec![405, 801],
        };
        let store = build_chain(&spec);
        let tip = store.by_height[&1100].header_hash;
        let server = WeightProofServer::new(Arc::new(store), MAINNET);
        let wp = server.get_proof_of_weight(tip).await.expect("proof builds");

        // Sub-epoch data mirrors the two summaries at-or-below the tip (weight_proof.py:101-109).
        assert_eq!(wp.sub_epochs.len(), 2);
        assert_eq!(wp.sub_epochs[0], create_sub_epoch_data(&ses(0)));
        assert_eq!(wp.sub_epochs[1], create_sub_epoch_data(&ses(1)));

        // Recent chain: block before ses0 (399) to tip (1100).
        assert_eq!(
            wp.recent_chain_data.first().map(HeaderBlock::height),
            Some(399)
        );
        assert_eq!(
            wp.recent_chain_data.last().map(HeaderBlock::height),
            Some(1100)
        );

        // One segment total: sub-epoch 0 has no challenge blocks, sub-epoch 1 has exactly one.
        assert_eq!(wp.sub_epoch_segments.len(), 1);
        let seg = &wp.sub_epoch_segments[0];
        assert_eq!(seg.sub_epoch_n, 1);
        // First segment of a non-zero sub-epoch carries the rc end-of-slot VDF of ses0's slot.
        assert_eq!(seg.rc_slot_end_info, Some(zero_vdf_info()));

        assert_eq!(seg.sub_slots.len(), 442);
        assert_eq!(
            seg.sub_slots
                .iter()
                .filter(|s| s.proof_of_space.is_some())
                .count(),
            1,
            "exactly the challenge block carries a proof of space"
        );
        assert_eq!(
            seg.sub_slots
                .iter()
                .filter(|s| s.cc_slot_end.is_some())
                .count(),
            41,
            "one end-of-slot entry per slot start in the segment span"
        );
        // The challenge-block entry sits right after the pre-challenge entries (index 6).
        assert!(seg.sub_slots[6].proof_of_space.is_some());
        assert_eq!(
            seg.sub_slots[6].total_iters,
            Some(406u128 * 10_000_000),
            "challenge entry carries the record's total_iters"
        );

        // The whole-proof cache: a second request for the same tip returns the SAME proof (chia
        // weight_proof.py:90-99 — `self.proof` under the handler lock).
        let again = server.get_proof_of_weight(tip).await.expect("cached");
        assert!(
            Arc::ptr_eq(&wp, &again),
            "same tip must hit the tip-keyed cache, not rebuild"
        );
    }

    // The oracle-gate regression: a proof built by the SERVER must pass the VALIDATOR's phase 2 —
    // this crate's mirror of chia `_validate_sub_epoch_summaries` (chia weight_proof.py:840-870):
    // `_get_last_ses_hash` (1572-1590) reads the on-chain summary commitment out of the recent
    // chain's finished sub slots, `_map_sub_epoch_summaries` (873-910) rebuilds the summary chain
    // from our emitted SubEpochData anchored on GENESIS_CHALLENGE, and the last reconstructed hash
    // must equal the commitment. Red on any builder defect in summary selection, ordering, or field
    // mapping — the exact failure class the live oracle gate checks first. (The 2026-08 oracle RED
    // at tip 830000 was the DRIVER anchoring on chia's un-overridden DEFAULT_CONSTANTS genesis —
    // sha256("") — not a builder defect; this test pins our side of that contract permanently.)
    //
    // The chain carries properly LINKED summaries: ses0.prev = GENESIS_CHALLENGE,
    // ses1.prev = hash(ses0), and the ses blocks' opening slots commit the hashes on chain.
    #[tokio::test]
    async fn built_proof_passes_validator_phase2_summary_anchor() {
        let ses0 = SubEpochSummary {
            prev_subepoch_summary_hash: MAINNET.genesis_challenge,
            reward_chain_hash: h32(1000),
            num_blocks_overflow: 3,
            new_difficulty: None,
            new_sub_slot_iters: None,
        };
        let ses1 = SubEpochSummary {
            prev_subepoch_summary_hash: ses0.hash().expect("hash ses0"),
            reward_chain_hash: h32(1001),
            num_blocks_overflow: 5,
            new_difficulty: None,
            new_sub_slot_iters: None,
        };
        let spec = ChainSpec {
            len: 1101,
            slot_every: 10,
            ses_heights: vec![(400, ses0), (800, ses1)],
            challenge_heights: vec![],
        };
        let store = build_chain(&spec);
        let tip = store.by_height[&1100].header_hash;
        let server = WeightProofServer::new(Arc::new(store), MAINNET);
        let wp = server.get_proof_of_weight(tip).await.expect("proof builds");

        let summaries = crate::sub_epoch_summaries_of(&wp, &MAINNET).expect(
            "the validator's _validate_sub_epoch_summaries mirror must accept a served proof \
             (genesis-anchored chain terminating in the recent chain's on-chain commitment)",
        );
        assert_eq!(summaries.len(), 2);
        assert_eq!(
            summaries[0], ses0,
            "reconstructed ses0 mirrors the stored summary"
        );
        assert_eq!(
            summaries[1], ses1,
            "reconstructed ses1 mirrors the stored summary"
        );
    }

    // Red-first, hand-derived from chia weight_proof.py:288-297 (__create_persist_segment) +
    // block_store.py:164-171: every sampled sub-epoch's built segments are persisted through the
    // store, keyed by the ses block's header hash, as the ChiaSerialize bytes of a
    // SubEpochSegments wrapper (block_store.py:170). On the smoke chain both sub-epochs are
    // sampled (weight_to_check is None): sub-epoch 0 persists an EMPTY list (chia persists the
    // empty build result too — only a None build errors), sub-epoch 1 persists its one segment.
    #[tokio::test]
    async fn built_segments_are_persisted_keyed_by_ses_hash() {
        use dg_xch_core::blockchain::weight_proof::SubEpochSegments;
        use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};

        let spec = ChainSpec {
            len: 1101,
            slot_every: 10,
            ses_heights: vec![(400, ses(0)), (800, ses(1))],
            challenge_heights: vec![405, 801],
        };
        let store = Arc::new(build_chain(&spec));
        let tip = store.by_height[&1100].header_hash;
        let ses0_hash = store.by_height[&400].header_hash;
        let ses1_hash = store.by_height[&800].header_hash;
        let server = WeightProofServer::new(store.clone(), MAINNET);
        let wp = server.get_proof_of_weight(tip).await.expect("proof builds");

        assert_eq!(
            store
                .segment_persists
                .load(std::sync::atomic::Ordering::Relaxed),
            2,
            "one persist per sampled sub-epoch"
        );
        let decode = |hh: &Bytes32| -> Vec<SubEpochChallengeSegment> {
            let bytes = store
                .segments
                .lock()
                .expect("segments")
                .get(hh)
                .cloned()
                .expect("persisted under the ses block hash");
            SubEpochSegments::from_bytes(
                &mut std::io::Cursor::new(&bytes[..]),
                ChiaProtocolVersion::Chia0_0_37,
            )
            .expect("persisted bytes decode as SubEpochSegments")
            .challenge_segments
        };
        assert_eq!(
            decode(&ses0_hash),
            Vec::new(),
            "sub-epoch 0 built no segments"
        );
        assert_eq!(
            decode(&ses1_hash),
            wp.sub_epoch_segments,
            "sub-epoch 1's persisted segments are exactly the served ones"
        );
    }

    // Red-first, hand-derived from chia weight_proof.py:288-292: get_sub_epoch_challenge_segments
    // is checked BEFORE building, so a fresh handler over a store that already holds the segments
    // must not walk the sub-epoch spans again. Restart-shaped: server B is a brand-new instance
    // (empty in-memory LRU) over the same store server A persisted into. The block-read budget
    // proves it: A's build reads the recent chain (heights 399..=1100 → 702 get_block calls) PLUS
    // both segment spans — 0..=528 (529) and 389..=928 (540; se_start 400 is itself a slot start,
    // so the two-slots walk stops one below 390) — while B's build must spend exactly the
    // recent-chain 702 and nothing else, persist nothing new, and serve the identical proof.
    #[tokio::test]
    async fn fresh_server_over_same_store_rebuilds_from_persisted_segments() {
        let spec = ChainSpec {
            len: 1101,
            slot_every: 10,
            ses_heights: vec![(400, ses(0)), (800, ses(1))],
            challenge_heights: vec![405, 801],
        };
        let store = Arc::new(build_chain(&spec));
        let tip = store.by_height[&1100].header_hash;

        let server_a = WeightProofServer::new(store.clone(), MAINNET);
        let wp_a = server_a
            .get_proof_of_weight(tip)
            .await
            .expect("first build");
        let gets_a = store.block_gets.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(gets_a, 702 + 529 + 540, "first build walks all three spans");

        let server_b = WeightProofServer::new(store.clone(), MAINNET);
        let wp_b = server_b
            .get_proof_of_weight(tip)
            .await
            .expect("rebuild over the persisted store");
        let gets_b = store.block_gets.load(std::sync::atomic::Ordering::Relaxed) - gets_a;
        assert_eq!(
            gets_b, 702,
            "rebuild reads only the recent chain — segments come from the store, not a block walk"
        );
        assert_eq!(
            store
                .segment_persists
                .load(std::sync::atomic::Ordering::Relaxed),
            2,
            "rebuild persists nothing new"
        );
        assert_eq!(
            *wp_a, *wp_b,
            "persisted segments reproduce the identical proof"
        );
    }
}
