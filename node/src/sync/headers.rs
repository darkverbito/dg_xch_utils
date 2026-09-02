use crate::cache::BlockRecordCache;
use crate::engine::Engine;
use crate::primitives::ConsensusPrimitives;
use crate::sync::{EpochSchedule, SyncError};
use dg_xch_core::blockchain::header_block::HeaderBlock;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::blockchain::sub_epoch_summary::SubEpochSummary;
use dg_xch_core::consensus::deficit::calculate_deficit;
use dg_xch_core::consensus::full_block_to_block_record::header_block_to_sub_block_record;
use dg_xch_core::consensus::pot_iterations::is_overflow_block;
use dg_xch_stores::{BlockStore, CoinStore};

// NotFound from the validator's ancestry walk means the block's context predates the sync start;
// a genuine PoW/VDF rejection surfaces as InvalidData.
fn out_of_window(e: &crate::error::NodeError) -> bool {
    matches!(e, crate::error::NodeError::Io(io) if io.kind() == std::io::ErrorKind::NotFound)
}

// Cross-block state for the recent-chain pospace walk: the running challenge pair, deficit, and
// transaction-block warmup count.
struct WalkState {
    challenge: Option<Bytes32>,
    prev_challenge: Option<Bytes32>,
    deficit: u8,
    tx_blocks: u32,
}

/// Headers-first candidate chain. Walks a header range in order, seeding each candidate record's
/// `required_iters` via the pospace light path, storing it (no body), and inserting it into the tip
/// cache so the engine's full PoW/VDF validator runs against sync-populated ancestry. Returns the
/// count of candidate records stored. The cache is a bounded height window.
///
/// # Errors
/// Returns [`SyncError`] if a header's proof of space is invalid or the store rejects a record.
pub(crate) async fn sync_header_chain<S, P>(
    engine: &Engine<S, P>,
    cache: &mut BlockRecordCache,
    headers: &[HeaderBlock],
    schedule: &EpochSchedule,
    summaries: &[SubEpochSummary],
) -> Result<usize, SyncError>
where
    S: CoinStore + BlockStore + Sync,
    P: ConsensusPrimitives + Sync,
{
    let c = engine.constants();
    let mut st = WalkState {
        challenge: headers
            .first()
            .map(|b| b.reward_chain_block.pos_ss_cc_challenge_hash),
        prev_challenge: None,
        deficit: 0,
        tx_blocks: 0,
    };
    let mut stored = 0usize;

    for header in headers {
        let rcb = &header.reward_chain_block;
        let h = header.height();
        for ss in &header.finished_sub_slots {
            st.prev_challenge = Some(ss.challenge_chain.challenge_chain_end_of_slot_vdf.challenge);
            st.challenge = Some(ss.challenge_chain.hash()?);
            st.deficit = ss.reward_chain.deficit;
        }

        // (ssi, difficulty) come from the weight proof's attested summary schedule, resolved per
        // height because a synced span can cross an epoch boundary where they change.
        let (ssi, difficulty) = schedule.at(h);
        let prev = cache.get(&header.prev_header_hash()).cloned();
        let mut overflow = false;
        let mut required_iters = 0u64;
        if let (Some(ch), Some(pc)) = (st.challenge, st.prev_challenge)
            && st.tx_blocks > 2
        {
            overflow = is_overflow_block(c, rcb.signage_point_index).map_err(SyncError::Io)?;
            st.deficit = calculate_deficit(
                c,
                h,
                prev.as_ref(),
                overflow,
                header.finished_sub_slots.len(),
            );
            match engine.light_required_iters(cache.records(), header, ch, pc, overflow, difficulty)
            {
                Ok(ri) => required_iters = ri,
                // Cold start: the pospace pre-sp walk reaches below the sync start. Store the record with a
                // 0 seed; full validation stays gated off until the window is deep enough.
                Err(e) if out_of_window(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }

        // A boundary header declares its sub-epoch summary's hash; the summary object must ride on
        // the record because the epoch machinery reads records, not headers. The attested summary
        // comes from the weight proof at sub-epoch index `height / SUB_EPOCH_BLOCKS - 1` and must
        // hash-match the header's declaration. Fail-closed on any mismatch.
        let declared_ses_hash = header
            .finished_sub_slots
            .iter()
            .find_map(|s| s.challenge_chain.subepoch_summary_hash);
        let ses = match declared_ses_hash {
            None => None,
            Some(expected) => {
                let idx = (h / c.sub_epoch_blocks)
                    .checked_sub(1)
                    .map(|i| i as usize)
                    .ok_or_else(|| {
                        SyncError::Io(std::io::Error::other(
                            "sub-epoch summary declared below the first sub-epoch",
                        ))
                    })?;
                let ses = summaries.get(idx).ok_or_else(|| {
                    SyncError::Io(std::io::Error::other(format!(
                        "no weight-proof summary at sub-epoch index {idx} for height {h}"
                    )))
                })?;
                if ses.hash().map_err(SyncError::Io)? != expected {
                    return Err(SyncError::Io(std::io::Error::other(format!(
                        "weight-proof summary hash does not match the header's declared \
                         sub-epoch summary at height {h}"
                    ))));
                }
                Some(*ses)
            }
        };
        let record = header_block_to_sub_block_record(
            c,
            required_iters,
            header,
            ssi,
            overflow,
            st.deficit,
            h,
            ses,
        )
        .map_err(SyncError::Io)?;
        engine
            .store()
            .add_block_records(std::slice::from_ref(&record))
            .await?;
        cache.insert(record);
        stored += 1;

        if rcb.is_transaction_block {
            st.tx_blocks += 1;
        }
    }
    Ok(stored)
}
