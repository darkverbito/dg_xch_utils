mod common;

use dg_xch_core::blockchain::full_block::FullBlock;
use dg_xch_core::blockchain::weight_proof::WeightProof;
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_core::protocols::full_node::RespondBlocks;
use dg_xch_node::sync::{BlockRangeSource, SyncError};
use dg_xch_node::{Chaser, Engine, NativePrimitives, SyncConfig};
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// Offline repro of the live tip-follow wall at mainnet 9,143,837 (`INVALID_ICC_VDF (invalid icc proof)`):
// real weight-proof checkpoint at 9,143,835 + the captured block range, driven through the exact
// headers-first -> in-order-confirm seam the live node runs. Env-gated on the uncommitted corpus
// (/path/to/corpus):
//   DGXCH_CORPUS=<dir> cargo test --release -p dg_xch_node --test reorg_wall_9143837 -- --ignored --nocapture

struct FileRangeSource {
    blocks: HashMap<u32, FullBlock>,
}

#[async_trait::async_trait]
impl BlockRangeSource for FileRangeSource {
    fn peer_id(&self) -> u64 {
        1
    }
    fn is_closed(&self) -> bool {
        false
    }
    async fn fetch_range(&self, start: u32, end: u32) -> Result<Vec<FullBlock>, SyncError> {
        let mut out = Vec::new();
        for h in start..=end {
            if let Some(b) = self.blocks.get(&h) {
                out.push(b.clone());
            }
        }
        Ok(out)
    }
}

fn corpus_dir() -> PathBuf {
    PathBuf::from(std::env::var("DGXCH_CORPUS").expect("set DGXCH_CORPUS to the corpus dir"))
}

fn load_wp(dir: &Path) -> WeightProof {
    let bytes = std::fs::read(dir.join("weight_proof_9144385.bin")).expect("weight proof present");
    WeightProof::from_bytes(&mut Cursor::new(&bytes[..]), ChiaProtocolVersion::default())
        .expect("weight proof deserializes")
}

fn load_range(dir: &Path, name: &str) -> Vec<FullBlock> {
    let bytes = std::fs::read(dir.join(name)).expect("range file present");
    RespondBlocks::from_bytes(&mut Cursor::new(&bytes[..]), ChiaProtocolVersion::default())
        .expect("RespondBlocks deserializes")
        .blocks
}

// Every captured `blocks_<start>_<end>.bin` in the corpus dir, deduped by height, ascending — the
// replay consumes whatever the live node captured, so extending the corpus extends the replay.
fn load_all_ranges(dir: &Path) -> Vec<FullBlock> {
    let mut by_height = std::collections::BTreeMap::new();
    for entry in std::fs::read_dir(dir).expect("corpus dir") {
        let entry = entry.expect("dir entry");
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("blocks_") && name.ends_with(".bin") {
            for b in load_range(dir, &name) {
                by_height.insert(b.height(), b);
            }
        }
    }
    by_height.into_values().collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the uncommitted corpus (DGXCH_CORPUS)"]
async fn checkpoint_confirm_walks_through_the_icc_slot_block() {
    let dir = corpus_dir();
    let wp = load_wp(&dir);
    let summaries =
        dg_xch_weight_proof::sub_epoch_summaries_of(&wp, &MAINNET).expect("summary chain");
    let store = common::new_store().await;
    let engine = Engine::new(store, NativePrimitives, MAINNET);
    let mut chaser = Chaser::new(engine, SyncConfig::default());
    let schedule = chaser.epoch_schedule(&summaries);
    let stored = chaser
        .sync_headers(&wp.recent_chain_data, &schedule, &summaries)
        .await
        .expect("headers-first candidate chain");
    eprintln!("headers stored: {stored}");

    let blocks = load_all_ranges(&dir);
    eprintln!(
        "range blocks: {} ({}..={})",
        blocks.len(),
        blocks.first().map(FullBlock::height).unwrap_or(0),
        blocks.last().map(FullBlock::height).unwrap_or(0)
    );
    for b in &blocks {
        if !b.finished_sub_slots.is_empty() {
            eprintln!(
                "height {} has {} finished sub-slot(s), icc present: {:?}",
                b.height(),
                b.finished_sub_slots.len(),
                b.finished_sub_slots
                    .iter()
                    .map(|s| s.infused_challenge_chain.is_some())
                    .collect::<Vec<_>>()
            );
        }
    }

    let source: Arc<dyn BlockRangeSource> = Arc::new(FileRangeSource {
        blocks: blocks.iter().map(|b| (b.height(), b.clone())).collect(),
    });
    // The live failing step, byte-for-byte: follow 9,143,835..=9,143,866 through in-order confirm,
    // one sub-range per block so the first rejecting HEIGHT is explicit in the output.
    let last = blocks.last().map(FullBlock::height).unwrap_or(9_143_866);
    let mut wall: Option<(u32, String)> = None;
    for h in 9_143_835..=last {
        match chaser.follow_to(&source, h, h).await {
            Ok(_) => {}
            Err(e) => {
                wall = Some((h, e.to_string()));
                break;
            }
        }
    }
    if let Some((h, e)) = &wall {
        let b = blocks.iter().find(|b| b.height() == *h).unwrap();
        eprintln!(
            "WALL REPRODUCED at {h}: {e} (tx_block={} gen={} refs={:?})",
            b.is_transaction_block(),
            b.transactions_generator.is_some(),
            b.transactions_generator_ref_list
        );
    }
    assert!(
        wall.is_none(),
        "the checkpoint follow confirms the whole range: {wall:?}"
    );
}

// The RESTART seam: the live pod resumed mid-window with a fresh (empty) engine cache over the same
// store — records confirmed by the previous process are on disk but NOT in cache, so any strict walk
// that assumes cache continuity falls off the cache edge mid-window ("block record not found" at
// 9,143,851). Confirm part of the range with one chaser, then hand the SAME store to a brand-new
// chaser (fresh cache) and confirm the rest — every ungated walk on the confirm path re-breaks this.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the uncommitted corpus (DGXCH_CORPUS)"]
async fn restart_mid_window_confirms_the_rest_with_a_cold_cache() {
    let dir = corpus_dir();
    let wp = load_wp(&dir);
    let summaries =
        dg_xch_weight_proof::sub_epoch_summaries_of(&wp, &MAINNET).expect("summary chain");
    let blocks = load_range(&dir, "blocks_9143835_9143866.bin");
    let source: Arc<dyn BlockRangeSource> = Arc::new(FileRangeSource {
        blocks: blocks.iter().map(|b| (b.height(), b.clone())).collect(),
    });

    let store = Arc::new(common::new_store().await);

    // Process one: headers-first + confirm the first half of the range.
    let engine = Engine::new(store.clone(), NativePrimitives, MAINNET);
    let mut chaser = Chaser::new(engine, SyncConfig::default());
    let schedule = chaser.epoch_schedule(&summaries);
    chaser
        .sync_headers(&wp.recent_chain_data, &schedule, &summaries)
        .await
        .expect("headers-first candidate chain");
    chaser
        .follow_to(&source, 9_143_835, 9_143_850)
        .await
        .expect("first process confirms the first half");
    drop(chaser);

    // Process two: same store, brand-new engine — the cache knows nothing the first process confirmed.
    let engine = Engine::new(store, NativePrimitives, MAINNET);
    let mut chaser = Chaser::new(engine, SyncConfig::default());
    let mut wall: Option<(u32, String)> = None;
    for h in 9_143_851..=9_143_866u32 {
        if let Err(e) = chaser.follow_to(&source, h, h).await {
            wall = Some((h, e.to_string()));
            break;
        }
    }
    assert!(
        wall.is_none(),
        "the restarted process must confirm the rest of the range with a cold cache: {wall:?}"
    );
}
