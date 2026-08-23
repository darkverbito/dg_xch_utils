mod common;

use dg_xch_core::blockchain::weight_proof::WeightProof;
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_node::{Chaser, Engine, NativePrimitives, SyncConfig};
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use std::io::Cursor;

// The real mainnet weight proof committed for the weight-proof crate (RequestProofOfWeight response), tip
// height 9_054_698. Referenced in place so the 14 MB fixture is not duplicated.
fn load_weight_proof() -> WeightProof {
    let bytes =
        include_bytes!("../../weight-proof/tests/fixtures/weight_proof_mainnet_9054698.bin");
    WeightProof::from_bytes(&mut Cursor::new(&bytes[..]), ChiaProtocolVersion::default())
        .expect("real mainnet weight proof deserializes")
}

// Fast sync validates the weight proof (the HAVE light-verify path — all six phases) and lands on the
// same peak a full sync reaches (the recent-chain tip the proof attests). Body fill from that peak runs the
// identical sync_range download/confirm pipeline the confirm-byte-equality test proves.
//
// The six-phase validation is real VDF/BLS/PoSpace verification over the full mainnet proof — ~10 min in
// release, far longer in debug — so this is release-gated exactly like the weight-proof crate's own
// end-to-end test (`cargo test -p dg_xch_node --release --test t053_fast_sync -- --ignored`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "full six-phase weight-proof validation ~10 min; run in release with --ignored"]
async fn fast_sync_reaches_the_same_peak_as_full_sync() {
    let wp = load_weight_proof();
    let full_sync_peak_height = wp
        .recent_chain_data
        .iter()
        .map(dg_xch_core::blockchain::header_block::HeaderBlock::height)
        .max()
        .expect("recent chain present");

    let store = common::new_store().await;
    let engine = Engine::new(store, NativePrimitives, MAINNET);
    let chaser = Chaser::new(engine, SyncConfig::default());

    let (peak_hash, peak_height) = chaser.fast_sync_peak(&wp).expect("weight proof validates");

    assert_eq!(
        peak_height, 9_054_698,
        "fast-sync lands on the attested tip"
    );
    assert_eq!(
        peak_height, full_sync_peak_height,
        "fast-sync peak equals the full-sync peak (both the recent-chain tip)"
    );
    let tip = wp.recent_chain_data.last().unwrap();
    assert_eq!(
        peak_hash,
        tip.header_hash().unwrap(),
        "peak hash is the tip header"
    );
}
