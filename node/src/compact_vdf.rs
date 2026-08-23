//! Compact-VDF (bluebox) pipeline primitives — the pure field-dispatch, guard, and
//! proof-swap logic behind the full node's `RequestCompactVDF` / `NewCompactVDF` /
//! `RespondCompactVDF` handling (production-parity plan, Phase 1.5).
//!
//! A bluebox timelord normalizes a block's bulky Wesolowski VDF proofs to compact
//! (`normalized_to_identity`, `witness_type == 0`) form and gossips them; peers pull the
//! compact proof and swap it into their stored block, shrinking the on-disk chain. This
//! module holds the store-blind, consensus-blind half of that exchange so it is unit-testable
//! against a real stored block without a running node.
//!
//! Chia oracle: `chia/full_node/full_node.py` (`_needs_compact_proof`,
//! `_can_accept_compact_proof`, `_replace_proof`, `request_compact_vdf`) and
//! `chia/types/blockchain_format/vdf.py` (`CompressibleVDFField`).

use dg_xch_core::blockchain::full_block::FullBlock;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::blockchain::vdf_info::VdfInfo;
use dg_xch_core::blockchain::vdf_proof::VdfProof;
use dg_xch_core::consensus::constants::ConsensusConstants;
use dg_xch_core::protocols::timelord::RequestCompactProofOfTime;
use dg_xch_vdf::{default_classgroup_element, validate_vdf_info};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Which VDF field of a block a compact proof refers to. Wire value is the `field_vdf: u8` of
/// the compact-VDF messages. Mirrors chia `CompressibleVDFField`
/// (chia/types/blockchain_format/vdf.py:80-84).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum CompressibleVdfField {
    CcEosVdf = 1,
    IccEosVdf = 2,
    CcSpVdf = 3,
    CcIpVdf = 4,
}

impl CompressibleVdfField {
    /// Decode the wire `field_vdf` byte; `None` for an unknown discriminant (the caller stays
    /// silent rather than panicking — chia constructs `CompressibleVDFField(int(...))` which
    /// would raise, and its handlers are wrapped so an invalid field is dropped).
    #[must_use]
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::CcEosVdf),
            2 => Some(Self::IccEosVdf),
            3 => Some(Self::CcSpVdf),
            4 => Some(Self::CcIpVdf),
            _ => None,
        }
    }
}

/// A proof is compact when its witness has been normalized to the identity element and carries
/// no intermediate witnesses. Chia treats `witness_type == 0 and normalized_to_identity` as the
/// already-compact predicate (`_needs_compact_proof`) and `witness_type > 0 or not
/// normalized_to_identity` as the not-compact rejection (`_can_accept_compact_proof`).
#[must_use]
pub fn is_compact(proof: &VdfProof) -> bool {
    proof.witness_type == 0 && proof.normalized_to_identity
}

/// The stored proof for `field` whose start-of-slot `VdfInfo` equals `vdf_info`, if any.
/// Mirrors the field dispatch shared by chia's `_needs_compact_proof`, `_replace_proof`, and
/// `request_compact_vdf`.
fn stored_proof_for<'a>(
    block: &'a FullBlock,
    field: CompressibleVdfField,
    vdf_info: &VdfInfo,
) -> Option<&'a VdfProof> {
    match field {
        CompressibleVdfField::CcEosVdf => block.finished_sub_slots.iter().find_map(|ss| {
            (&ss.challenge_chain.challenge_chain_end_of_slot_vdf == vdf_info)
                .then_some(&ss.proofs.challenge_chain_slot_proof)
        }),
        CompressibleVdfField::IccEosVdf => block.finished_sub_slots.iter().find_map(|ss| {
            ss.infused_challenge_chain.as_ref().and_then(|icc| {
                (&icc.infused_challenge_chain_end_of_slot_vdf == vdf_info)
                    .then_some(())
                    .and(ss.proofs.infused_challenge_chain_slot_proof.as_ref())
            })
        }),
        CompressibleVdfField::CcSpVdf => block
            .reward_chain_block
            .challenge_chain_sp_vdf
            .as_ref()
            .filter(|sp| *sp == vdf_info)
            .and(block.challenge_chain_sp_proof.as_ref()),
        CompressibleVdfField::CcIpVdf => (&block.reward_chain_block.challenge_chain_ip_vdf
            == vdf_info)
            .then_some(&block.challenge_chain_ip_proof),
    }
}

/// SERVE arm (chia `full_node.request_compact_vdf`): return OUR stored proof for `field` at
/// `vdf_info` **only when it is already compact** — otherwise stay silent (`None`). A peer asking
/// us to compress a proof we have not compressed ourselves gets nothing.
#[must_use]
pub fn serve_compact(block: &FullBlock, field: u8, vdf_info: &VdfInfo) -> Option<VdfProof> {
    let field = CompressibleVdfField::from_u8(field)?;
    let proof = stored_proof_for(block, field, vdf_info)?;
    is_compact(proof).then(|| proof.clone())
}

/// Does this block still carry a **bulky** proof for `field` at `vdf_info`? True only when the
/// field is present, its `VdfInfo` matches, and its proof is not yet compact — the exact
/// condition under which a compact replacement is wanted. Mirrors chia `_needs_compact_proof`
/// (returns True only for found-and-bulky; False for compact, absent, or a `VdfInfo` mismatch).
#[must_use]
pub fn needs_compact_proof(block: &FullBlock, field: u8, vdf_info: &VdfInfo) -> bool {
    let Some(field) = CompressibleVdfField::from_u8(field) else {
        return false;
    };
    stored_proof_for(block, field, vdf_info).is_some_and(|p| !is_compact(p))
}

/// Verify an offered compact proof stands on its own: it is compact, and it validates from the
/// default class-group element (the normalized-to-identity entry — chia `_can_accept_compact_proof`
/// calls `validate_vdf(proof, constants, ClassgroupElement.get_default_element(), vdf_info)`).
#[must_use]
pub fn validate_compact_proof(
    constants: &ConsensusConstants,
    vdf_info: &VdfInfo,
    proof: &VdfProof,
) -> bool {
    is_compact(proof)
        && validate_vdf_info(
            constants,
            &default_classgroup_element(),
            vdf_info,
            proof,
            None,
        )
}

/// Full admission gate for an offered compact proof (chia `_can_accept_compact_proof`):
/// - not too recent (`peak_height - height >= 5` — chia will not compactify a recent block);
/// - the proof is compact and validates from the default element;
/// - we still hold a bulky proof for that exact field/`VdfInfo` (`needs_compact_proof`).
///
/// The whole-block `is_fully_compactified` short-circuit chia checks first is a store-side
/// bookkeeping flag we do not maintain; the per-field `needs_compact_proof` below is the
/// behaviourally decisive guard for the field in question, so it is omitted deliberately.
#[must_use]
pub fn can_accept_compact_proof(
    constants: &ConsensusConstants,
    block: &FullBlock,
    field: u8,
    vdf_info: &VdfInfo,
    proof: &VdfProof,
    peak_height: u32,
    height: u32,
) -> bool {
    if peak_height.saturating_sub(height) < 5 {
        return false;
    }
    if !validate_compact_proof(constants, vdf_info, proof) {
        return false;
    }
    needs_compact_proof(block, field, vdf_info)
}

/// REPLACE (chia `_replace_proof`): return a copy of `block` with the compact `new_proof` swapped
/// in for the single `field` whose `VdfInfo` matches `vdf_info`; `None` when nothing matches. Only
/// the one proof changes, so the block's `header_hash` is unaffected — the same block, a smaller
/// witness. The caller re-writes the body under the same header hash and re-gossips `NewCompactVDF`.
#[must_use]
pub fn replace_proof(
    block: &FullBlock,
    field: u8,
    vdf_info: &VdfInfo,
    new_proof: &VdfProof,
) -> Option<FullBlock> {
    let field = CompressibleVdfField::from_u8(field)?;
    let mut nb = block.clone();
    match field {
        CompressibleVdfField::CcEosVdf => {
            let ss = nb
                .finished_sub_slots
                .iter_mut()
                .find(|ss| &ss.challenge_chain.challenge_chain_end_of_slot_vdf == vdf_info)?;
            ss.proofs.challenge_chain_slot_proof = new_proof.clone();
        }
        CompressibleVdfField::IccEosVdf => {
            let ss = nb.finished_sub_slots.iter_mut().find(|ss| {
                ss.infused_challenge_chain
                    .as_ref()
                    .is_some_and(|icc| &icc.infused_challenge_chain_end_of_slot_vdf == vdf_info)
            })?;
            ss.proofs.infused_challenge_chain_slot_proof = Some(new_proof.clone());
        }
        CompressibleVdfField::CcSpVdf => {
            if nb
                .reward_chain_block
                .challenge_chain_sp_vdf
                .as_ref()
                .is_none_or(|sp| sp != vdf_info)
            {
                return None;
            }
            nb.challenge_chain_sp_proof = Some(new_proof.clone());
        }
        CompressibleVdfField::CcIpVdf => {
            if &nb.reward_chain_block.challenge_chain_ip_vdf != vdf_info {
                return None;
            }
            nb.challenge_chain_ip_proof = new_proof.clone();
        }
    }
    Some(nb)
}

/// Every VDF field of `block` whose proof is still bulky, as `(field_vdf, start-of-slot VdfInfo)`
/// pairs — the solicitation list chia's `broadcast_uncompact_blocks` builds to hand blueboxes
/// (chia enumerates CC_EOS + ICC_EOS per finished sub slot, then CC_SP and CC_IP). Order matches
/// chia: sub-slot EOS fields first, then CC_SP, then CC_IP.
#[must_use]
pub fn uncompact_fields(block: &FullBlock) -> Vec<(u8, VdfInfo)> {
    let mut out = Vec::new();
    for ss in &block.finished_sub_slots {
        if !is_compact(&ss.proofs.challenge_chain_slot_proof) {
            out.push((
                CompressibleVdfField::CcEosVdf as u8,
                ss.challenge_chain.challenge_chain_end_of_slot_vdf,
            ));
        }
        if let (Some(icc), Some(proof)) = (
            ss.infused_challenge_chain.as_ref(),
            ss.proofs.infused_challenge_chain_slot_proof.as_ref(),
        ) && !is_compact(proof)
        {
            out.push((
                CompressibleVdfField::IccEosVdf as u8,
                icc.infused_challenge_chain_end_of_slot_vdf,
            ));
        }
    }
    if let (Some(sp_vdf), Some(sp_proof)) = (
        block.reward_chain_block.challenge_chain_sp_vdf.as_ref(),
        block.challenge_chain_sp_proof.as_ref(),
    ) && !is_compact(sp_proof)
    {
        out.push((CompressibleVdfField::CcSpVdf as u8, *sp_vdf));
    }
    if !is_compact(&block.challenge_chain_ip_proof) {
        out.push((
            CompressibleVdfField::CcIpVdf as u8,
            block.reward_chain_block.challenge_chain_ip_vdf,
        ));
    }
    out
}

/// Dedup key for one solicited compact-proof-of-time request: the block's header hash, the
/// compressible field byte, and the field's start-of-slot VDF challenge + iteration count. The
/// challenge is what distinguishes multiple CC_EOS/ICC_EOS sub-slots of the SAME block (they
/// share a field byte but each finished sub slot has a distinct challenge), so the key does not
/// collapse them into one — a block with two bulky EOS slots solicits both.
type SolicitKey = (Bytes32, u8, Bytes32, u64);

fn solicit_key(req: &RequestCompactProofOfTime) -> SolicitKey {
    (
        req.header_hash,
        req.field_vdf,
        req.new_proof_of_time.challenge,
        req.new_proof_of_time.number_of_iterations,
    )
}

/// Bounded time-to-live + capacity dedup of already-solicited compact-proof requests.
///
/// WHY (audit note): unlike chia's `broadcast_uncompact_blocks`, which samples RANDOM
/// not-compactified heights each tick (so it naturally spreads its requests and rarely repeats a
/// field before a timelord has answered), our scan sweeps a FIXED recent window every tick. Without
/// a memory it would re-solicit the very same bulky fields every interval, spamming a connected
/// bluebox with duplicate work it is already grinding. This ledger records what we solicited and
/// suppresses a re-send until `ttl` elapses — after which we DO re-solicit (a timelord may have
/// gone away, or dropped the request, so a bulky field that never became compact must be retried,
/// not abandoned forever). Capacity caps memory: past `capacity` distinct keys the oldest is
/// evicted (an evicted stale key simply becomes eligible to solicit again — safe, never a panic).
pub struct SolicitLedger {
    /// Last-solicited instant per distinct request key.
    seen: HashMap<SolicitKey, Instant>,
    /// Insertion order of distinct keys, for capacity eviction (one entry per live key).
    order: VecDeque<SolicitKey>,
    capacity: usize,
    ttl: Duration,
}

impl SolicitLedger {
    /// `capacity` distinct keys retained (>=1 enforced); a key is re-solicitable after `ttl`.
    #[must_use]
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            seen: HashMap::new(),
            order: VecDeque::new(),
            capacity: capacity.max(1),
            ttl,
        }
    }

    /// Should `req` be solicited at `now`? Returns `true` and records the send when the field has
    /// not been solicited within `ttl` (first sight, or the ttl window has expired); `false` when a
    /// recent solicitation is still outstanding. A `true` result mutates the ledger (records the
    /// send + evicts if over capacity); a `false` result leaves it untouched.
    pub fn admit(&mut self, req: &RequestCompactProofOfTime, now: Instant) -> bool {
        let key = solicit_key(req);
        if let Some(&last) = self.seen.get(&key)
            && now.duration_since(last) < self.ttl
        {
            return false;
        }
        let is_new = self.seen.insert(key, now).is_none();
        if is_new {
            self.order.push_back(key);
        }
        while self.seen.len() > self.capacity {
            let Some(evict) = self.order.pop_front() else {
                break;
            };
            self.seen.remove(&evict);
        }
        true
    }

    /// Distinct keys currently retained (bounded by `capacity`). For tests/observability.
    #[must_use]
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// Whether the ledger holds no solicited keys.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

/// Build the deduped solicitation list for one stored `block`: every still-bulky VDF field of the
/// block (see [`uncompact_fields`]) turned into a [`RequestCompactProofOfTime`], minus any the
/// `ledger` already solicited within its ttl. Mirrors the per-block half of chia
/// `broadcast_uncompact_blocks`' `broadcast_list` construction — the message a bluebox timelord
/// needs to compute + return the compact proof — with our added re-solicit suppression on top.
#[must_use]
pub fn plan_block_solicitations(
    block: &FullBlock,
    header_hash: Bytes32,
    height: u32,
    ledger: &mut SolicitLedger,
    now: Instant,
) -> Vec<RequestCompactProofOfTime> {
    uncompact_fields(block)
        .into_iter()
        .map(|(field_vdf, vdf_info)| RequestCompactProofOfTime {
            new_proof_of_time: vdf_info,
            header_hash,
            height,
            field_vdf,
        })
        .filter(|req| ledger.admit(req, now))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_from_u8_covers_the_four_compressible_fields_and_rejects_others() {
        assert_eq!(
            CompressibleVdfField::from_u8(1),
            Some(CompressibleVdfField::CcEosVdf)
        );
        assert_eq!(
            CompressibleVdfField::from_u8(2),
            Some(CompressibleVdfField::IccEosVdf)
        );
        assert_eq!(
            CompressibleVdfField::from_u8(3),
            Some(CompressibleVdfField::CcSpVdf)
        );
        assert_eq!(
            CompressibleVdfField::from_u8(4),
            Some(CompressibleVdfField::CcIpVdf)
        );
        assert_eq!(CompressibleVdfField::from_u8(0), None);
        assert_eq!(CompressibleVdfField::from_u8(5), None);
    }

    #[test]
    fn compactness_predicate_matches_chia() {
        let compact = VdfProof {
            witness_type: 0,
            witness: Default::default(),
            normalized_to_identity: true,
        };
        assert!(is_compact(&compact));
        let bulky_witness = VdfProof {
            witness_type: 3,
            ..compact.clone()
        };
        assert!(!is_compact(&bulky_witness));
        let not_normalized = VdfProof {
            normalized_to_identity: false,
            ..compact
        };
        assert!(!is_compact(&not_normalized));
    }
}
