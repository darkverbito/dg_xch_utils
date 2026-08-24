// The weight-proof fork point against the LOCAL chain, chia
// `WeightProofHandler.get_fork_point` (chia/full_node/weight_proof.py:644-664): iterate the
// chains' sub-epoch summaries positionally, find the last agreement, and back the long-sync
// start off two sub-epochs below it (clamping the first three to genesis). The mid-chain
// long-sync must start AT/NEAR that boundary — never from zero, and never blindly from the
// local peak when the offline gap saw a deeper reorg. Summaries hash-chain
// (`prev_subepoch_summary_hash`), so the top-down positional hash match used here is equivalent
// to chia's bottom-up full-map walk.

mod common;

use dg_xch_core::blockchain::block_record::BlockRecord;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::blockchain::sub_epoch_summary::SubEpochSummary;
use dg_xch_core::blockchain::unsized_bytes::UnsizedBytes;
use dg_xch_core::blockchain::vdf_output::VdfOutput;
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_node::sync::{WpForkPoint, wp_fork_point};
use dg_xch_stores::BlockStore;
use std::sync::Arc;

// Deterministic hash-chained summaries: entry i links entry i-1 by consensus hash, exactly as
// the chain commits them. `tag` seeds distinct chains (the reorged-branch fixtures).
fn ses_chain(n: usize, tag: u8) -> Vec<SubEpochSummary> {
    let mut out: Vec<SubEpochSummary> = Vec::with_capacity(n);
    let mut prev = Bytes32::from([tag; 32]);
    for i in 0..n {
        let mut rc = [tag; 32];
        rc[31] = u8::try_from(i).expect("small fixture");
        rc[30] = 0x5e;
        let s = SubEpochSummary {
            prev_subepoch_summary_hash: prev,
            reward_chain_hash: Bytes32::from(rc),
            num_blocks_overflow: 0,
            new_difficulty: None,
            new_sub_slot_iters: None,
        };
        prev = s.hash().expect("ses hash");
        out.push(s);
    }
    out
}

fn plain_hash(n: u32) -> Bytes32 {
    let mut b = [0u8; 32];
    b[..4].copy_from_slice(&n.to_be_bytes());
    Bytes32::from(b)
}

// A linked record; `ses` = the summary this block includes (the fork-point walk's only inputs
// are the prev-hash links and the included summaries).
fn rec(h: u32, ses: Option<SubEpochSummary>) -> BlockRecord {
    BlockRecord {
        header_hash: plain_hash(h),
        prev_hash: plain_hash(h.wrapping_sub(1)),
        height: h,
        weight: 1_000_000 + u128::from(h),
        total_iters: 10_000_000 * u128::from(h),
        signage_point_index: 0,
        challenge_vdf_output: VdfOutput {
            data: UnsizedBytes::new(vec![]),
        },
        infused_challenge_vdf_output: None,
        reward_infusion_new_challenge: Bytes32::default(),
        challenge_block_info_hash: Bytes32::default(),
        sub_slot_iters: MAINNET.sub_slot_iters_starting,
        pool_puzzle_hash: Bytes32::default(),
        farmer_puzzle_hash: Bytes32::default(),
        required_iters: 1,
        deficit: 0,
        overflow: false,
        prev_transaction_block_height: h.wrapping_sub(1),
        timestamp: Some(1_000 + u64::from(h)),
        prev_transaction_block_hash: None,
        fees: None,
        reward_claims_incorporated: None,
        finished_challenge_slot_hashes: None,
        finished_infused_challenge_slot_hashes: None,
        finished_reward_slot_hashes: None,
        sub_epoch_summary_included: ses,
    }
}

// SES-bearing heights every 5 blocks: local summary #i sits at height 5*(i+1).
const SES_STEP: u32 = 5;

fn ses_height(i: usize) -> u32 {
    SES_STEP * (u32::try_from(i).expect("small fixture") + 1)
}

// A store holding heights 0..=top whose included summaries are `local` in order.
async fn store_with_local_summaries(
    local: &[SubEpochSummary],
    top: u32,
) -> Arc<dg_xch_stores::SqliteStore> {
    let store = Arc::new(common::new_store().await);
    let chain: Vec<BlockRecord> = (0..=top)
        .map(|h| {
            let ses = (h % SES_STEP == 0 && h > 0)
                .then(|| local.get((h / SES_STEP - 1) as usize).cloned())
                .flatten();
            rec(h, ses)
        })
        .collect();
    store.add_block_records(&chain).await.expect("seed chain");
    store.set_peak(&plain_hash(top)).await.expect("set peak");
    store
}

// Sub-epoch granularity for the walk's step cap only; the fixture spacing is SES_STEP.
const SUB_EPOCH_BLOCKS: u32 = 32;

// Agreement through local sub-epoch K=5 (received summaries extend beyond us — the plain
// offline-gap shape): NO fork detected, and chia's conservative start is the height of the
// summary two below the last credited agreement — near K's boundary, NOT zero.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agreement_up_to_sub_epoch_k_starts_the_long_sync_near_k_not_zero() {
    let received = ses_chain(8, 0xaa);
    // Local chain carries received[0..=5]; peak a few blocks past the last summary.
    let store = store_with_local_summaries(&received[..6], ses_height(5) + 3).await;
    let fork = wp_fork_point(store.as_ref(), &received, SUB_EPOCH_BLOCKS)
        .await
        .expect("fork point");
    // Last agreement = local summary #5 (chia fork_point_index = 5) → two below = summary #3.
    assert_eq!(
        fork,
        WpForkPoint::NoForkDetected {
            conservative: ses_height(3)
        },
        "the long sync must start at the two-below sub-epoch boundary (weight_proof.py:659-663)"
    );
    match fork {
        WpForkPoint::NoForkDetected { conservative } => {
            assert_ne!(
                conservative, 0,
                "a deep mid-chain agreement never restarts from zero"
            );
        }
        other => panic!("expected NoForkDetected, got {other:?}"),
    }
}

// The reorg-across-the-gap shape: our top summary is NOT on the proof's chain (the offline
// period reorged deeper than a sub-epoch). The fork point is the divergence — two below the
// last agreement — and it is reported as a DIVERGENCE so the caller rewinds through the engine
// reorg instead of extending the stale branch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn divergent_top_summary_reports_the_fork_below_the_peak() {
    let received = ses_chain(8, 0xaa);
    // Local: received[0..=4] then a summary from a DIFFERENT chain at position 5.
    let mut local: Vec<SubEpochSummary> = received[..5].to_vec();
    local.push(ses_chain(6, 0xbb).remove(5));
    let store = store_with_local_summaries(&local, ses_height(5) + 3).await;
    let fork = wp_fork_point(store.as_ref(), &received, SUB_EPOCH_BLOCKS)
        .await
        .expect("fork point");
    // Last agreement = local summary #4 (chia fork_point_index = 4) → two below = summary #2.
    assert_eq!(
        fork,
        WpForkPoint::Diverged {
            fork_point: ses_height(2)
        },
        "a mismatched summary is a detected divergence at chia's conservative back-off"
    );
}

// chia weight_proof.py:659-661: "Two summaries can have different blocks and still be
// identical / This gets resolved after one full sub epoch" — an agreement index ≤ 2 clamps the
// fork point to genesis.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shallow_agreement_clamps_to_genesis() {
    let received = ses_chain(8, 0xaa);
    let store = store_with_local_summaries(&received[..3], ses_height(2) + 2).await;
    let fork = wp_fork_point(store.as_ref(), &received, SUB_EPOCH_BLOCKS)
        .await
        .expect("fork point");
    assert_eq!(
        fork,
        WpForkPoint::NoForkDetected { conservative: 0 },
        "fork_point_index <= 2 returns 0 (weight_proof.py:659-661)"
    );
}

// No positional agreement at all (every local summary belongs to a foreign chain): the fork
// point cannot be established — the caller must fail closed, never guess a start height.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_agreement_within_the_walk_window_is_unknown() {
    let received = ses_chain(8, 0xaa);
    let foreign = ses_chain(6, 0xcc);
    let store = store_with_local_summaries(&foreign, ses_height(5) + 3).await;
    let fork = wp_fork_point(store.as_ref(), &received, SUB_EPOCH_BLOCKS)
        .await
        .expect("fork point");
    assert_eq!(fork, WpForkPoint::Unknown);
}

// An empty store has no peak and therefore no fork point (the from-zero landing owns that
// band); a too-short summary list can never be positionally credited.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_store_or_trivial_summaries_are_unknown() {
    let received = ses_chain(8, 0xaa);
    let empty = Arc::new(common::new_store().await);
    assert_eq!(
        wp_fork_point(empty.as_ref(), &received, SUB_EPOCH_BLOCKS)
            .await
            .expect("fork point"),
        WpForkPoint::Unknown
    );
    let store = store_with_local_summaries(&received[..6], ses_height(5) + 3).await;
    assert_eq!(
        wp_fork_point(store.as_ref(), &received[..1], SUB_EPOCH_BLOCKS)
            .await
            .expect("fork point"),
        WpForkPoint::Unknown
    );
}
