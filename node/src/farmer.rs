//! The farmer interface (node/consensus side): validating a farmer's declared proof of space
//! against a signage point we have accepted, holding the accepted declaration as a candidate, and
//! assembling the unfinished block from an accepted proof.
//!
//! This module is pure (no store, no locks): [`validate_declared_proof`] takes closures for the
//! two slot-state lookups so the early-return ladder is unit-testable offline. The daemon passes
//! `SlotState`-backed closures.

use dg_xch_core::blockchain::block_record::BlockRecord;
use dg_xch_core::blockchain::pool_target::PoolTarget;
use dg_xch_core::blockchain::signage_point::SignagePoint;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::blockchain::subslot_bundle::SubSlotBundle;
use dg_xch_core::blockchain::unfinished_block::UnfinishedBlock;
use dg_xch_core::consensus::constants::ConsensusConstants;
use dg_xch_core::consensus::pot_iterations::{
    calculate_ip_iters, calculate_iterations_quality, calculate_sp_iters,
};
use dg_xch_core::consensus::producer::{
    FarmerSignatures, RewardBlockClaim, calculate_infusion_point_total_iters,
    create_unfinished_block_with_sigs, g2_infinity,
};
use dg_xch_core::protocols::farmer::{
    DeclareProofOfSpace, NewSignagePoint, RequestSignedValues, SPVDFSourceData,
    SignagePointSourceData,
};
use dg_xch_pos::verify_and_get_quality_string;
use std::collections::{HashMap, VecDeque};

/// The outcome of validating a `DeclareProofOfSpace`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeclareVerdict {
    /// No accepted signage point matches `challenge_chain_sp`.
    UnknownSignagePoint,
    /// index > 0 and the SP's reward-chain output no longer matches — a stale point that has been
    /// superseded.
    StaleSignagePoint,
    /// The SP's challenge sits in no sub slot we hold.
    UnknownSubSlot,
    /// `verify_and_get_quality_string` rejected the proof.
    InvalidProof,
    /// Accepted; carries the proof's quality string.
    Accepted(Bytes32),
}

impl DeclareVerdict {
    /// The stable metric/event label for this verdict — the `result` dimension of
    /// `fullnode_producer_declares_validated_total` and the `producer.declare.*` event reason.
    /// A bounded enum, safe as a Prometheus label.
    #[must_use]
    pub fn result_label(&self) -> &'static str {
        match self {
            DeclareVerdict::UnknownSignagePoint => "unknown_signage_point",
            DeclareVerdict::StaleSignagePoint => "stale_signage_point",
            DeclareVerdict::UnknownSubSlot => "unknown_sub_slot",
            DeclareVerdict::InvalidProof => "pospace_verify_fail",
            DeclareVerdict::Accepted(_) => "accepted",
        }
    }
}

/// The challenge-chain challenge a declared proof is filtered/validated against: the SP hash
/// itself at signage-point index 0, otherwise the SP's challenge-chain VDF challenge.
#[must_use]
pub fn cc_challenge_hash(declare: &DeclareProofOfSpace, sp: &SignagePoint) -> Bytes32 {
    if declare.signage_point_index == 0 {
        // The sub-slot-start SP has no cc VDF; the cc challenge IS the SP hash.
        declare.challenge_chain_sp
    } else {
        // index > 0 always carries a real cc VDF; the fallback to the SP hash only guards a
        // malformed all-None SP reaching here, which the accept ladder rejects.
        sp.cc_vdf
            .as_ref()
            .map_or(declare.challenge_chain_sp, |v| v.challenge)
    }
}

/// Validate a farmer's `DeclareProofOfSpace` against our slot state. Returns the quality string
/// on accept. `lookup_sp(cc_sp)` resolves an accepted signage point by its challenge-chain SP
/// hash; `sub_slot_present(cc)` reports whether we hold the sub slot for challenge `cc`. `height`
/// is the current peak height (0 pre-genesis), feeding the plot filter.
///
/// The caller (daemon) is expected to have already dropped the message if the node is syncing —
/// tip-context objects cannot validate mid-sync.
pub fn validate_declared_proof(
    constants: &ConsensusConstants,
    declare: &DeclareProofOfSpace,
    height: u32,
    lookup_sp: impl Fn(&Bytes32) -> Option<SignagePoint>,
    sub_slot_present: impl Fn(&Bytes32) -> bool,
) -> DeclareVerdict {
    // The proof must be for a signage point we have accepted.
    let Some(sp) = lookup_sp(&declare.challenge_chain_sp) else {
        return DeclareVerdict::UnknownSignagePoint;
    };
    // For index > 0, the SP's reward-chain output must still be the current one; a mismatch means
    // the farmer is answering a superseded signage point.
    if declare.signage_point_index > 0
        && sp.rc_vdf.as_ref().and_then(|v| v.output.hash().ok()) != Some(declare.reward_chain_sp)
    {
        return DeclareVerdict::StaleSignagePoint;
    }
    let cc_hash = cc_challenge_hash(declare, &sp);
    // A non-genesis SP must belong to a sub slot we hold.
    if declare.challenge_hash != constants.genesis_challenge && !sub_slot_present(&cc_hash) {
        return DeclareVerdict::UnknownSubSlot;
    }
    // The proof of space itself must verify against our checker.
    match verify_and_get_quality_string(
        &declare.proof_of_space,
        constants,
        cc_hash,
        declare.challenge_chain_sp,
        height,
    ) {
        Some(quality) => DeclareVerdict::Accepted(quality),
        None => DeclareVerdict::InvalidProof,
    }
}

/// Build the farmer-protocol `NewSignagePoint` we push to farming peers when we accept a signage
/// point. Distinct from the full-node `NewSignagePointOrEndOfSubSlot` gossip: this carries the
/// difficulty/SSI context a farmer needs to look up plots. `None` only if the SP's VDF outputs
/// fail to hash.
#[must_use]
pub fn new_signage_point_for_farmers(
    sp: &SignagePoint,
    challenge_hash: Bytes32,
    difficulty: u64,
    sub_slot_iters: u64,
    signage_point_index: u8,
    peak_height: u32,
    last_tx_height: u32,
) -> Option<NewSignagePoint> {
    // A normal (index > 0) signage point carries its cc/rc SP-VDF outputs as
    // sp_source_data.vdf_data so a farmer/harvester can reconstruct what it signs.
    let cc_vdf = sp.cc_vdf.as_ref()?;
    let rc_vdf = sp.rc_vdf.as_ref()?;
    Some(NewSignagePoint {
        challenge_hash,
        challenge_chain_sp: cc_vdf.output.hash().ok()?,
        reward_chain_sp: rc_vdf.output.hash().ok()?,
        difficulty,
        sub_slot_iters,
        signage_point_index,
        peak_height,
        last_tx_height,
        sp_source_data: Some(SignagePointSourceData {
            sub_slot_data: None,
            vdf_data: Some(SPVDFSourceData {
                cc_vdf: cc_vdf.output,
                rc_vdf: rc_vdf.output,
            }),
        }),
    })
}

/// The `rc_prev` for a `NewUnfinishedBlockTimelord`: the last reward-chain infusion before this
/// block's signage point. At signage-point index 0 it is the pos sub-slot's reward-chain hash,
/// falling back to the genesis challenge when the pos sub-slot IS genesis and is not held as a
/// finished sub-slot; `None` when we hold no such sub slot. At index > 0 it is the SP's
/// reward-chain VDF challenge.
///
/// `pos_sub_slot_rc_hash` is resolved by the caller from the slot state (index 0 only) so this
/// stays pure.
#[must_use]
pub fn timelord_rc_prev(
    genesis_challenge: Bytes32,
    signage_point_index: u8,
    pos_ss_cc_challenge_hash: Bytes32,
    reward_chain_sp_vdf: Option<&dg_xch_core::blockchain::vdf_info::VdfInfo>,
    pos_sub_slot_rc_hash: Option<Bytes32>,
) -> Option<Bytes32> {
    if signage_point_index == 0 {
        pos_sub_slot_rc_hash.or_else(|| {
            (pos_ss_cc_challenge_hash == genesis_challenge).then_some(genesis_challenge)
        })
    } else {
        reward_chain_sp_vdf.map(|v| v.challenge)
    }
}

/// An accepted declaration, held until a block is assembled from it (or it ages out).
#[derive(Debug, Clone)]
pub struct AcceptedProof {
    pub declare: DeclareProofOfSpace,
    pub quality_string: Bytes32,
}

/// Bounded FIFO of accepted proof declarations, keyed by quality string. Unlike the generator seed
/// cache (whose eviction could wall body validation), evicting a candidate here is harmless: the
/// only consequence is we do not build a block from that particular proof, and the next signage
/// point brings fresh declarations. So a plain capacity bound is correct.
#[derive(Debug)]
pub struct ProofCandidateStore {
    map: HashMap<Bytes32, AcceptedProof>,
    order: VecDeque<Bytes32>,
    cap: usize,
}

impl ProofCandidateStore {
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap: cap.max(1),
        }
    }

    /// Insert (or refresh) an accepted proof. FIFO-evicts the oldest past the cap.
    pub fn insert(&mut self, proof: AcceptedProof) {
        let key = proof.quality_string;
        if self.map.insert(key, proof).is_none() {
            self.order.push_back(key);
        }
        while self.map.len() > self.cap {
            match self.order.pop_front() {
                Some(oldest) => {
                    self.map.remove(&oldest);
                }
                None => break,
            }
        }
    }

    #[must_use]
    pub fn get(&self, quality_string: &Bytes32) -> Option<&AcceptedProof> {
        self.map.get(quality_string)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl Default for ProofCandidateStore {
    fn default() -> Self {
        // A slot carries 64 signage points; a few slots of candidates is ample headroom.
        Self::new(256)
    }
}

/// A candidate unfinished block awaiting the farmer's foliage signatures. At
/// `declare_proof_of_space` time the node builds the block with placeholder foliage signatures
/// and stores it here keyed by the proof's quality string; at `signed_values` time it is
/// retrieved, its placeholder foliage signatures are spliced with the farmer's real ones, and the
/// finished block is propagated.
///
/// Bounded FIFO like [`ProofCandidateStore`]: evicting an un-signed candidate is harmless (the
/// farmer simply gets no block from that one proof; the next signage point brings fresh
/// candidates). `height` is carried alongside for the caller's logging.
///
/// No "backup" empty candidate is kept per quality string: self-farmed-block validation is
/// deferred to the driver (`ub_inbox` → `process_ub_inbox`), so a backup fallback would need a
/// driver→farmer re-request seam that does not exist. The producer's re-run gate
/// (`Mempool::create_block_generator` re-executes the emitted generator through our own
/// validator) covers the generator-bug case; remaining consensus-edge failures lose the single
/// farm win.
#[derive(Debug)]
pub struct CandidateBlockStore {
    map: HashMap<
        Bytes32,
        (
            u32,
            dg_xch_core::blockchain::unfinished_block::UnfinishedBlock,
        ),
    >,
    order: VecDeque<Bytes32>,
    cap: usize,
}

impl CandidateBlockStore {
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap: cap.max(1),
        }
    }

    /// Insert (or refresh) a candidate for `quality_string`. FIFO-evicts the oldest past the cap.
    pub fn insert(
        &mut self,
        quality_string: Bytes32,
        height: u32,
        block: dg_xch_core::blockchain::unfinished_block::UnfinishedBlock,
    ) {
        if self.map.insert(quality_string, (height, block)).is_none() {
            self.order.push_back(quality_string);
        }
        while self.map.len() > self.cap {
            match self.order.pop_front() {
                Some(oldest) => {
                    self.map.remove(&oldest);
                }
                None => break,
            }
        }
    }

    /// The candidate for `quality_string`.
    #[must_use]
    pub fn get(
        &self,
        quality_string: &Bytes32,
    ) -> Option<&(
        u32,
        dg_xch_core::blockchain::unfinished_block::UnfinishedBlock,
    )> {
        self.map.get(quality_string)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl Default for CandidateBlockStore {
    fn default() -> Self {
        Self::new(256)
    }
}

// Candidate assembly (pure). The daemon resolves the store/slot-derived inputs (peak, prev-block
// backtrack, finished sub-slots, reward-claim walk) and hands them to these functions. Kept pure
// (no store, no locks) so the consensus arithmetic is unit-testable offline, like
// `validate_declared_proof` above.

/// Difficulty + sub-slot-iters at the candidate's sub-slot.
/// Pre-hard-fork / near-genesis (`peak` is `None` or `peak.height <= MAX_SUB_SLOT_BLOCKS`) uses the
/// starting constants; otherwise the peak's own difficulty (`peak.weight - prev_weight`) and
/// `peak.sub_slot_iters`, each overridden by any epoch transition carried in the finished sub-slots being
/// added (`challenge_chain.new_difficulty` / `.new_sub_slot_iters`).
///
/// `peak` carries the peak record and its previous block's weight (`block_record(peak.prev_hash).weight`)
/// — the daemon looks the previous weight up, since `SlotState`/this fn hold no store.
#[must_use]
pub fn candidate_difficulty_and_ssi(
    constants: &ConsensusConstants,
    peak: Option<(&BlockRecord, u128)>,
    finished_sub_slots: &[SubSlotBundle],
) -> (u64, u64) {
    match peak {
        Some((peak, prev_weight)) if peak.height > constants.max_sub_slot_blocks => {
            // The weight difference is a single block's difficulty and fits u64 by consensus;
            // clamp defensively rather than panic.
            let mut difficulty =
                u64::try_from(peak.weight.saturating_sub(prev_weight)).unwrap_or(u64::MAX);
            let mut sub_slot_iters = peak.sub_slot_iters;
            for ss in finished_sub_slots {
                if let Some(new_difficulty) = ss.challenge_chain.new_difficulty {
                    difficulty = new_difficulty;
                }
                if let Some(new_ssi) = ss.challenge_chain.new_sub_slot_iters {
                    sub_slot_iters = new_ssi;
                }
            }
            (difficulty, sub_slot_iters)
        }
        _ => (
            constants.difficulty_starting,
            constants.sub_slot_iters_starting,
        ),
    }
}

/// The candidate's iteration positions.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct CandidateIters {
    pub required_iters: u64,
    pub sp_iters: u64,
    pub ip_iters: u64,
    /// Becomes `RewardChainBlockUnfinished.total_iters`.
    pub infusion_point_total_iters: u128,
    /// `total_iters_pos_slot + sp_iters` — the empty-block-coercion key.
    pub candidate_sp_total_iters: u128,
}

/// Resolve `required_iters` / `sp_iters` / `ip_iters` / `infusion_point_total_iters`, reusing the
/// same `calculate_*` functions the validator uses (no reimplemented consensus math).
///
/// Returns `None` when the proof does not pass the iters filter: `calculate_ip_iters` errors when
/// `required_iters == 0` or `required_iters >= sp_interval_iters` (the pospace filter gate — the
/// proof is too weak to infuse), so we bail with no block rather than build an invalid candidate.
#[must_use]
// The pospace-to-candidate-iters computation binds this many independent inputs (quality, pos
// size, difficulty, ssi, sp index, cc-sp hash, slot iters); allow the arity.
#[allow(clippy::too_many_arguments)]
pub fn resolve_candidate_iters(
    constants: &ConsensusConstants,
    quality_string: Bytes32,
    pos_size: u8,
    difficulty: u64,
    sub_slot_iters: u64,
    signage_point_index: u8,
    // The cc SP output hash the quality is bound to.
    cc_sp_hash: Bytes32,
    total_iters_pos_slot: u128,
) -> Option<CandidateIters> {
    let required_iters = calculate_iterations_quality(
        constants.difficulty_constant_factor,
        quality_string,
        pos_size,
        difficulty,
        cc_sp_hash,
    );
    let sp_iters = calculate_sp_iters(constants, sub_slot_iters, signage_point_index).ok()?;
    let ip_iters = calculate_ip_iters(
        constants,
        sub_slot_iters,
        signage_point_index,
        required_iters,
    )
    .ok()?;
    let infusion_point_total_iters = calculate_infusion_point_total_iters(
        total_iters_pos_slot,
        sp_iters,
        ip_iters,
        sub_slot_iters,
    );
    let candidate_sp_total_iters = total_iters_pos_slot + u128::from(sp_iters);
    Some(CandidateIters {
        required_iters,
        sp_iters,
        ip_iters,
        infusion_point_total_iters,
        candidate_sp_total_iters,
    })
}

/// The block-store-derived previous-block linkage for a candidate. The daemon resolves these from
/// the store and passes them in.
#[derive(Clone, Debug)]
pub struct CandidatePrev {
    /// Always `true` for genesis.
    pub is_transaction_block: bool,
    /// `foliage.prev_block_hash` — `GENESIS_CHALLENGE` at height 0, else `prev_b.header_hash`.
    pub prev_block_hash: Bytes32,
    /// `foliage_transaction_block.prev_transaction_block_hash` — `GENESIS_CHALLENGE` when the
    /// previous transaction block is genesis, else that block's `header_hash`. Only consumed for
    /// a tx block.
    pub prev_transaction_block_hash: Bytes32,
    /// The previous transaction block's height — the produce-path mempool gate: the mempool's
    /// peak (its time-lock reference frame) must be exactly this block before its items may be
    /// assembled into the candidate; a mismatch yields an empty block, the conservative outcome.
    /// `0` at genesis (the mempool has no peak yet, so the gate fails closed).
    pub prev_transaction_block_height: u32,
    /// `reward_claims_incorporated` inputs — the prev transaction block (with its `fees`)
    /// followed by the non-transaction blocks between it and the transaction block before it
    /// (`fees == 0`). Empty for a non-tx candidate and for genesis.
    pub reward_claims: Vec<RewardBlockClaim>,
}

/// Assemble the candidate `UnfinishedBlock` + the `RequestSignedValues` the farmer must sign.
/// Reuses [`create_unfinished_block_with_sigs`]; the two signage-point signatures come verbatim
/// from `declare` and the two foliage signatures are [`g2_infinity`] placeholders spliced later
/// at `signed_values`.
///
/// `transactions` is the mempool-assembled block generator payload (built by
/// `Mempool::create_block_generator`), or `None` for an empty block. The daemon passes `Some`
/// only when the candidate IS a transaction block, the empty-block coercion
/// (`candidate_sp_total_iters <= tx_peak.total_iters`) did not fire, and the mempool's peak
/// matches the candidate's previous transaction block. The generator is never attached to a
/// non-tx unfinished candidate, so it never carries a dangling generator on the wire.
///
/// `sp` is the signage point's VDFs: `Some` for signage-point index > 0, `None` at index 0 (the
/// sub-slot start / genesis first SP), where the VDFs are nulled.
///
/// Returns `None` only if the assembled foliage/reward block fails to serialize for hashing.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn assemble_candidate(
    constants: &ConsensusConstants,
    declare: &DeclareProofOfSpace,
    quality_string: Bytes32,
    sp: Option<&SignagePoint>,
    finished_sub_slots: Vec<SubSlotBundle>,
    iters: &CandidateIters,
    height: u32,
    prev: &CandidatePrev,
    // The mempool-assembled transactions, None for an empty block.
    transactions: Option<&dg_xch_core::consensus::producer::BlockTransactions>,
    // GENESIS_PRE_FARM at genesis, else pos.pool_contract_puzzle_hash / declare.
    pool_target: PoolTarget,
    // GENESIS_PRE_FARM_FARMER at genesis, else declare.farmer_puzzle_hash.
    farmer_reward_puzzle_hash: Bytes32,
    timestamp: u64,
    // The candidate's `pos_ss_cc_challenge_hash`.
    cc_challenge_hash: Bytes32,
) -> Option<(UnfinishedBlock, RequestSignedValues)> {
    // Index 0 nulls the VDFs; index > 0 carries the real signage-point VDFs/proofs into the
    // reward block + unfinished block.
    let (cc_sp_vdf, cc_sp_proof, rc_sp_vdf, rc_sp_proof) = match sp {
        // The SP fields are already `Option`, so an index-0 sub-slot-start SP (all-None)
        // naturally threads `None` into the reward/unfinished block.
        Some(sp) => (
            sp.cc_vdf,
            sp.cc_proof.clone(),
            sp.rc_vdf,
            sp.rc_proof.clone(),
        ),
        None => (None, None, None, None),
    };
    // cc_sp_hash -> declare.challenge_chain_sp_signature; rc_sp_hash ->
    // declare.reward_chain_sp_signature; foliage hashes -> infinity placeholder (spliced at
    // signed_values). The node never holds the plot key.
    let farmer_sigs = FarmerSignatures {
        challenge_chain_sp_signature: declare.challenge_chain_sp_signature,
        reward_chain_sp_signature: declare.reward_chain_sp_signature,
        foliage_block_data_signature: g2_infinity(),
        foliage_transaction_block_signature: g2_infinity(),
    };
    let block = create_unfinished_block_with_sigs(
        constants,
        iters.infusion_point_total_iters,
        declare.signage_point_index,
        declare.proof_of_space.clone(),
        cc_challenge_hash,
        cc_sp_vdf,
        cc_sp_proof,
        rc_sp_vdf,
        rc_sp_proof,
        finished_sub_slots,
        height,
        prev.is_transaction_block,
        &prev.reward_claims,
        // Some(..) farms the mempool's transactions into this candidate; None is a valid empty
        // transaction block with reward coins only.
        transactions,
        prev.prev_block_hash,
        prev.prev_transaction_block_hash,
        pool_target,
        declare.pool_signature,
        farmer_reward_puzzle_hash,
        timestamp,
        b"",
        farmer_sigs,
    )
    .ok()?;

    // RequestSignedValues carries the two foliage hashes the farmer signs.
    // foliage_transaction_block_hash is zeros for a non-transaction block.
    let foliage_block_data_hash = block.foliage.foliage_block_data.hash().ok()?;
    let foliage_transaction_block_hash = block
        .foliage
        .foliage_transaction_block_hash
        .unwrap_or_default();
    // The signature-source-data fields are populated only when the farmer asked for them
    // (include_signature_source_data); lets a source-data farmer verify what it signs.
    let (foliage_block_data, foliage_transaction_block_data, rc_block_unfinished) =
        if declare.include_signature_source_data {
            (
                Some(block.foliage.foliage_block_data),
                block.foliage_transaction_block,
                Some(block.reward_chain_block.clone()),
            )
        } else {
            (None, None, None)
        };
    let request = RequestSignedValues {
        quality_string,
        foliage_block_data_hash,
        foliage_transaction_block_hash,
        foliage_block_data,
        foliage_transaction_block_data,
        rc_block_unfinished,
    };
    Some((block, request))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dg_xch_core::blockchain::class_group_element::ClassgroupElement;
    use dg_xch_core::blockchain::proof_of_space::{ProofBytes, ProofOfSpace};
    use dg_xch_core::blockchain::sized_bytes::Bytes48;
    use dg_xch_core::blockchain::unsized_bytes::UnsizedBytes;
    use dg_xch_core::blockchain::vdf_info::VdfInfo;
    use dg_xch_core::blockchain::vdf_proof::VdfProof;
    use dg_xch_core::consensus::constants::MAINNET;

    fn vdf(challenge: u8) -> VdfInfo {
        VdfInfo {
            challenge: Bytes32::from([challenge; 32]),
            number_of_iterations: 1,
            output: ClassgroupElement::get_default_element(),
        }
    }

    fn empty_proof() -> VdfProof {
        VdfProof {
            witness_type: 0,
            witness: UnsizedBytes::default(),
            normalized_to_identity: false,
        }
    }

    fn sp(cc_challenge: u8) -> SignagePoint {
        SignagePoint {
            cc_vdf: Some(vdf(cc_challenge)),
            cc_proof: Some(empty_proof()),
            rc_vdf: Some(vdf(cc_challenge.wrapping_add(1))),
            rc_proof: Some(empty_proof()),
        }
    }

    // A proof of space with a pool key present (so verify does not early-reject on the null-pool
    // guard) but bogus proof bytes — it reaches, and fails, verify_and_get_quality_string.
    fn declare(index: u8, cc_sp: u8, challenge_hash: Bytes32) -> DeclareProofOfSpace {
        DeclareProofOfSpace {
            challenge_hash,
            challenge_chain_sp: Bytes32::from([cc_sp; 32]),
            signage_point_index: index,
            reward_chain_sp: Bytes32::from([cc_sp.wrapping_add(1); 32]),
            proof_of_space: ProofOfSpace {
                challenge: Bytes32::from([9; 32]),
                pool_public_key: Some(Bytes48::from([1; 48])),
                pool_contract_puzzle_hash: None,
                plot_public_key: Bytes48::from([2; 48]),
                size: 32,
                proof: ProofBytes::from(vec![0u8; 64]),
            },
            challenge_chain_sp_signature: Default::default(),
            reward_chain_sp_signature: Default::default(),
            farmer_puzzle_hash: Bytes32::from([3; 32]),
            pool_target: None,
            pool_signature: None,
            include_signature_source_data: false,
        }
    }

    #[test]
    fn unknown_signage_point_is_rejected() {
        let d = declare(1, 5, Bytes32::from([7; 32]));
        let v = validate_declared_proof(&MAINNET, &d, 100, |_| None, |_| true);
        assert_eq!(v, DeclareVerdict::UnknownSignagePoint);
    }

    #[test]
    fn stale_signage_point_is_rejected() {
        // index > 0, but the accepted SP's rc output hashes to something other than reward_chain_sp.
        let d = declare(3, 5, Bytes32::from([7; 32]));
        let accepted = sp(40); // rc output is the default element -> its hash != d.reward_chain_sp
        let v =
            validate_declared_proof(&MAINNET, &d, 100, move |_| Some(accepted.clone()), |_| true);
        assert_eq!(v, DeclareVerdict::StaleSignagePoint);
    }

    #[test]
    fn unknown_sub_slot_is_rejected() {
        // index 0 so the rc-recency check is skipped; non-genesis challenge; sub slot absent.
        let d = declare(0, 5, Bytes32::from([7; 32]));
        let accepted = sp(5);
        let v = validate_declared_proof(
            &MAINNET,
            &d,
            100,
            move |_| Some(accepted.clone()),
            |_| false,
        );
        assert_eq!(v, DeclareVerdict::UnknownSubSlot);
    }

    #[test]
    fn valid_lookup_reaches_pospace_check_and_rejects_bogus_proof() {
        // Everything up to the PoSpace check passes; the bogus proof bytes fail verification.
        // (The ACCEPTED path needs a genuine plot proof — the live dg_fast_farmer gate.)
        let d = declare(0, 5, Bytes32::from([7; 32]));
        let accepted = sp(5);
        let v =
            validate_declared_proof(&MAINNET, &d, 100, move |_| Some(accepted.clone()), |_| true);
        assert_eq!(v, DeclareVerdict::InvalidProof);
    }

    #[test]
    fn cc_challenge_hash_follows_the_index() {
        let sp0 = sp(5);
        let d0 = declare(0, 5, Bytes32::from([7; 32]));
        assert_eq!(cc_challenge_hash(&d0, &sp0), d0.challenge_chain_sp);
        let d1 = declare(7, 5, Bytes32::from([7; 32]));
        assert_eq!(
            cc_challenge_hash(&d1, &sp0),
            sp0.cc_vdf.as_ref().unwrap().challenge
        );
    }

    #[test]
    fn index_zero_declare_validates_against_the_sub_slot() {
        // Index-0 path: the all-None sub-slot-start SP resolves, the rc-recency check is skipped,
        // cc_hash == challenge_chain_sp, and the sub-slot presence check passes — reaching (and
        // here failing) the PoSpace verify, never UnknownSignagePoint.
        let d = declare(0, 5, Bytes32::from([7; 32]));
        let sub_slot_start = SignagePoint::sub_slot_start();
        let v = validate_declared_proof(
            &MAINNET,
            &d,
            100,
            move |_| Some(sub_slot_start.clone()),
            |_| true,
        );
        assert_eq!(
            v,
            DeclareVerdict::InvalidProof,
            "index-0 declare reaches the PoSpace verify (bogus proof), not UnknownSignagePoint"
        );
    }

    #[test]
    fn timelord_rc_prev_index_gt_0_uses_sp_reward_chain_challenge() {
        // index > 0: rc_prev = reward_chain_sp_vdf.challenge.
        let rc = vdf(9);
        let got = timelord_rc_prev(
            MAINNET.genesis_challenge,
            5,
            Bytes32::from([1; 32]),
            Some(&rc),
            None,
        );
        assert_eq!(got, Some(rc.challenge));
    }

    #[test]
    fn timelord_rc_prev_index_0_uses_resolved_sub_slot_rc_hash() {
        // index 0 with the pos sub-slot held: rc_prev = that sub-slot's reward-chain hash.
        let resolved = Bytes32::from([44; 32]);
        let got = timelord_rc_prev(
            MAINNET.genesis_challenge,
            0,
            Bytes32::from([1; 32]),
            None,
            Some(resolved),
        );
        assert_eq!(got, Some(resolved));
    }

    #[test]
    fn timelord_rc_prev_index_0_genesis_falls_back_to_genesis_challenge() {
        // index 0, pos sub-slot not held but it IS the genesis challenge: rc_prev = genesis.
        let got = timelord_rc_prev(
            MAINNET.genesis_challenge,
            0,
            MAINNET.genesis_challenge,
            None,
            None,
        );
        assert_eq!(got, Some(MAINNET.genesis_challenge));
    }

    #[test]
    fn timelord_rc_prev_index_0_missing_non_genesis_sub_slot_is_none() {
        // index 0, pos sub-slot unknown and not genesis -> None.
        let got = timelord_rc_prev(
            MAINNET.genesis_challenge,
            0,
            Bytes32::from([1; 32]),
            None,
            None,
        );
        assert_eq!(got, None);
    }

    #[test]
    fn assemble_index_zero_candidate_nulls_the_sp_vdfs() {
        // Index-0: the reward/unfinished block carries no signage VDFs. Passing sp=None threads
        // that through.
        let declare = declare(0, 5, MAINNET.genesis_challenge);
        let prev = CandidatePrev {
            is_transaction_block: true,
            prev_block_hash: MAINNET.genesis_challenge,
            prev_transaction_block_hash: MAINNET.genesis_challenge,
            prev_transaction_block_height: 0,
            reward_claims: Vec::new(),
        };
        let pool_target = PoolTarget {
            puzzle_hash: MAINNET.genesis_pre_farm_pool_puzzle_hash,
            max_height: 0,
        };
        let (block, _request) = assemble_candidate(
            &MAINNET,
            &declare,
            Bytes32::from([42; 32]),
            None, // index-0: sub-slot-start, no signage VDFs
            Vec::new(),
            &iters(),
            0,
            &prev,
            None,
            pool_target,
            MAINNET.genesis_pre_farm_farmer_puzzle_hash,
            1_700_000_000,
            MAINNET.genesis_challenge,
        )
        .expect("index-0 genesis candidate assembles");
        assert_eq!(block.reward_chain_block.signage_point_index, 0);
        assert!(block.reward_chain_block.challenge_chain_sp_vdf.is_none());
        assert!(block.reward_chain_block.reward_chain_sp_vdf.is_none());
        assert!(block.challenge_chain_sp_proof.is_none());
        assert!(block.reward_chain_sp_proof.is_none());
    }

    #[test]
    fn candidate_store_is_bounded_fifo() {
        let mut store = ProofCandidateStore::new(4);
        for i in 0..10u8 {
            store.insert(AcceptedProof {
                declare: declare(0, i, Bytes32::from([7; 32])),
                quality_string: Bytes32::from([i; 32]),
            });
        }
        assert_eq!(store.len(), 4, "bounded to the cap");
        assert!(
            store.get(&Bytes32::from([9; 32])).is_some(),
            "newest retained"
        );
        assert!(
            store.get(&Bytes32::from([0; 32])).is_none(),
            "oldest evicted"
        );
    }

    // A minimal BlockRecord with only the fields the difficulty selection reads set meaningfully.
    fn br(height: u32, weight: u128, sub_slot_iters: u64) -> BlockRecord {
        BlockRecord {
            header_hash: Bytes32::from([height as u8; 32]),
            prev_hash: Bytes32::from([height.wrapping_sub(1) as u8; 32]),
            height,
            weight,
            total_iters: 0,
            signage_point_index: 0,
            challenge_vdf_output: ClassgroupElement::get_default_element(),
            infused_challenge_vdf_output: None,
            reward_infusion_new_challenge: Bytes32::default(),
            challenge_block_info_hash: Bytes32::default(),
            sub_slot_iters,
            pool_puzzle_hash: Bytes32::default(),
            farmer_puzzle_hash: Bytes32::default(),
            required_iters: 1,
            deficit: 0,
            overflow: false,
            prev_transaction_block_height: 0,
            timestamp: None,
            prev_transaction_block_hash: None,
            fees: None,
            reward_claims_incorporated: None,
            finished_challenge_slot_hashes: None,
            finished_infused_challenge_slot_hashes: None,
            finished_reward_slot_hashes: None,
            sub_epoch_summary_included: None,
        }
    }

    #[test]
    fn difficulty_starting_when_no_peak_or_near_genesis() {
        // No peak, or peak height <= MAX_SUB_SLOT_BLOCKS, uses the starting consts.
        assert_eq!(
            candidate_difficulty_and_ssi(&MAINNET, None, &[]),
            (MAINNET.difficulty_starting, MAINNET.sub_slot_iters_starting)
        );
        let low = br(MAINNET.max_sub_slot_blocks, 1_000, 999);
        assert_eq!(
            candidate_difficulty_and_ssi(&MAINNET, Some((&low, 940)), &[]),
            (MAINNET.difficulty_starting, MAINNET.sub_slot_iters_starting)
        );
    }

    #[test]
    fn difficulty_is_peak_weight_delta_past_the_starting_window() {
        // difficulty = peak.weight - prev.weight; ssi = peak.sub_slot_iters.
        let peak = br(MAINNET.max_sub_slot_blocks + 72, 1_000, 12_345);
        assert_eq!(
            candidate_difficulty_and_ssi(&MAINNET, Some((&peak, 940)), &[]),
            (60, 12_345)
        );
    }

    #[test]
    fn iters_filter_rejects_an_out_of_range_required_iters() {
        // An impossibly hard difficulty pushes required_iters to u64::MAX, so calculate_ip_iters
        // errors (required_iters >= sp_interval_iters) and we bail with no candidate.
        let out = resolve_candidate_iters(
            &MAINNET,
            Bytes32::from([3; 32]),
            32,
            u64::MAX,
            MAINNET.sub_slot_iters_starting,
            2,
            Bytes32::from([9; 32]),
            0,
        );
        assert!(out.is_none(), "unfarmable proof must not yield a candidate");
    }

    // Fixed iters so the assembly tests do not depend on a quality-hash landing in range.
    fn iters() -> CandidateIters {
        CandidateIters {
            required_iters: 100,
            sp_iters: 10,
            ip_iters: 20,
            infusion_point_total_iters: 30,
            candidate_sp_total_iters: 10,
        }
    }

    #[test]
    fn assemble_genesis_candidate_is_a_transaction_block() {
        // Genesis: prev_b None, height 0, transaction block, pre-farm targets.
        let declare = declare(2, 5, MAINNET.genesis_challenge);
        let sp = sp(5);
        let prev = CandidatePrev {
            is_transaction_block: true,
            prev_block_hash: MAINNET.genesis_challenge,
            prev_transaction_block_hash: MAINNET.genesis_challenge,
            prev_transaction_block_height: 0,
            reward_claims: Vec::new(),
        };
        let pool_target = PoolTarget {
            puzzle_hash: MAINNET.genesis_pre_farm_pool_puzzle_hash,
            max_height: 0,
        };
        let (block, request) = assemble_candidate(
            &MAINNET,
            &declare,
            Bytes32::from([42; 32]),
            Some(&sp),
            Vec::new(),
            &iters(),
            0,
            &prev,
            None,
            pool_target,
            MAINNET.genesis_pre_farm_farmer_puzzle_hash,
            1_700_000_000,
            MAINNET.genesis_challenge,
        )
        .expect("genesis candidate assembles");

        // Reward block carries the infusion-point iters + the declared SP index/challenge verbatim.
        assert_eq!(block.reward_chain_block.total_iters, 30);
        assert_eq!(block.reward_chain_block.signage_point_index, 2);
        assert_eq!(
            block.reward_chain_block.pos_ss_cc_challenge_hash,
            MAINNET.genesis_challenge
        );
        // Index > 0 keeps the real SP VDFs (not the null-out).
        assert!(block.reward_chain_block.challenge_chain_sp_vdf.is_some());
        assert!(block.reward_chain_block.reward_chain_sp_vdf.is_some());
        // SP signatures come verbatim from the declare; foliage signatures are infinity placeholders.
        assert_eq!(
            block.reward_chain_block.challenge_chain_sp_signature,
            declare.challenge_chain_sp_signature
        );
        assert_eq!(block.foliage.foliage_block_data_signature, g2_infinity());
        // Genesis is a transaction block: foliage carries a transaction block + hash.
        assert!(block.foliage_transaction_block.is_some());
        assert!(block.foliage.foliage_transaction_block_hash.is_some());
        assert_eq!(block.foliage.prev_block_hash, MAINNET.genesis_challenge);
        // RequestSignedValues exposes exactly the two hashes the farmer signs.
        assert_eq!(request.quality_string, Bytes32::from([42; 32]));
        assert_eq!(
            request.foliage_block_data_hash,
            block.foliage.foliage_block_data.hash().unwrap()
        );
        assert_eq!(
            request.foliage_transaction_block_hash,
            block.foliage.foliage_transaction_block_hash.unwrap()
        );
    }

    #[test]
    fn assemble_non_transaction_candidate_has_zeroed_ftb_hash() {
        // A non-transaction candidate builds no foliage transaction block; its hash in
        // RequestSignedValues is zeros.
        let declare = declare(3, 5, Bytes32::from([7; 32]));
        let sp = sp(5);
        let prev = CandidatePrev {
            is_transaction_block: false,
            prev_block_hash: Bytes32::from([1; 32]),
            prev_transaction_block_hash: MAINNET.genesis_challenge,
            prev_transaction_block_height: 0,
            reward_claims: Vec::new(),
        };
        let pool_target = PoolTarget {
            puzzle_hash: Bytes32::from([8; 32]),
            max_height: 0,
        };
        let (block, request) = assemble_candidate(
            &MAINNET,
            &declare,
            Bytes32::from([1; 32]),
            Some(&sp),
            Vec::new(),
            &iters(),
            101,
            &prev,
            None,
            pool_target,
            Bytes32::from([2; 32]),
            1_700_000_000,
            Bytes32::from([9; 32]),
        )
        .expect("non-tx candidate assembles");
        assert!(block.foliage_transaction_block.is_none());
        assert!(block.foliage.foliage_transaction_block_hash.is_none());
        assert_eq!(request.foliage_transaction_block_hash, Bytes32::default());
    }

    // A normal-index farmer signage point must carry vdf_data (the cc/rc SP-VDF outputs), never
    // sub_slot_data, and survive a wire round-trip.
    #[test]
    fn normal_signage_point_carries_vdf_source_data() {
        use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
        let point = sp(5);
        let out =
            new_signage_point_for_farmers(&point, Bytes32::from([5u8; 32]), 100, 200, 7, 9, 8)
                .expect("cc/rc VDFs present");
        assert_eq!(out.signage_point_index, 7);
        let src = out
            .sp_source_data
            .as_ref()
            .expect("sp_source_data populated at a normal index");
        assert!(src.vdf_data.is_some(), "a normal index must carry vdf_data");
        assert!(
            src.sub_slot_data.is_none(),
            "a normal index must NOT carry sub_slot_data"
        );
        let v = src.vdf_data.as_ref().unwrap();
        assert_eq!(v.cc_vdf, point.cc_vdf.as_ref().unwrap().output);
        assert_eq!(v.rc_vdf, point.rc_vdf.as_ref().unwrap().output);
        let bytes = out.to_bytes(ChiaProtocolVersion::Chia0_0_37).unwrap();
        let back = NewSignagePoint::from_bytes(
            &mut std::io::Cursor::new(bytes.as_slice()),
            ChiaProtocolVersion::Chia0_0_37,
        )
        .unwrap();
        assert_eq!(back, out);
    }
}
