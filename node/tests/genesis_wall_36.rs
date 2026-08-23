mod common;

use dg_xch_core::blockchain::full_block::FullBlock;
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_core::protocols::full_node::RespondBlocks;
use dg_xch_node::sync::{BlockRangeSource, SyncError};
use dg_xch_node::{Chaser, Engine, NativePrimitives, SyncConfig};
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use std::collections::HashMap;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;

// Offline repro of the genesis long-sync wall at mainnet height 36 (`io error: ip_iters`):
// real mainnet blocks 0..=1023 fetched via the chia full-node RPC oracle, driven
// through the exact from-empty follow the --genesis-sync daemon runs — no weight proof, no
// headers-first candidates, every block fully validated against its confirmed ancestry.
// Env-gated on the uncommitted corpus (/path/to/corpus):
//   DGXCH_CORPUS=<dir> cargo test --release -p dg_xch_node --test genesis_wall_36 -- --ignored --nocapture

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
    PathBuf::from(
        std::env::var("DGXCH_CORPUS").expect("set DGXCH_CORPUS to the genesis corpus dir"),
    )
}

fn load_all_blocks(dir: &PathBuf) -> HashMap<u32, FullBlock> {
    let mut blocks = HashMap::new();
    for entry in std::fs::read_dir(dir).expect("corpus dir readable") {
        let path = entry.expect("dir entry").path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.starts_with("blocks_") || !name.ends_with(".bin") {
            continue;
        }
        let bytes = std::fs::read(&path).expect("range file readable");
        let ranged =
            RespondBlocks::from_bytes(&mut Cursor::new(&bytes[..]), ChiaProtocolVersion::default())
                .expect("RespondBlocks deserializes");
        for b in ranged.blocks {
            blocks.insert(b.height(), b);
        }
    }
    blocks
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the uncommitted genesis corpus (DGXCH_CORPUS)"]
async fn genesis_follow_confirms_the_first_thousand_blocks() {
    let dir = corpus_dir();
    let blocks = load_all_blocks(&dir);
    let last = *blocks.keys().max().expect("corpus non-empty");
    assert_eq!(*blocks.keys().min().expect("corpus non-empty"), 0);
    let source: Arc<dyn BlockRangeSource> = Arc::new(FileRangeSource {
        blocks: blocks.clone(),
    });

    let store = Arc::new(common::new_store().await);
    let engine = Engine::new(store.clone(), NativePrimitives, MAINNET);
    let mut chaser = Chaser::new(engine, SyncConfig::default());

    // Drive exactly like the --genesis-sync daemon: 32-block windows from height 0, each block
    // validated against confirmed ancestry only. Report the FIRST wall precisely.
    let mut wall: Option<(u32, String)> = None;
    let mut next = 0u32;
    'outer: while next <= last {
        let to = last.min(next + 31);
        match chaser.follow_to(&source, next, to).await {
            Ok(_) => next = to + 1,
            Err(e) => {
                // Walk single blocks to name the exact failing height.
                #[allow(clippy::mut_range_bound)]
                // the mutation feeds the outer loop, not this range
                for h in next..=to {
                    if let Err(single) = chaser.follow_to(&source, h, h).await {
                        wall = Some((h, format!("{single}")));
                        let b = blocks.get(&h).expect("wall block in corpus");
                        eprintln!(
                            "GENESIS WALL at {h}: {single} (batch error: {e}) \
                             (tx_block={} slots={} sp_index={})",
                            b.is_transaction_block(),
                            b.finished_sub_slots.len(),
                            b.reward_chain_block.signage_point_index,
                        );
                        break 'outer;
                    }
                    next = h + 1;
                }
            }
        }
    }
    assert!(
        wall.is_none(),
        "genesis follow confirms blocks 0..={last}: {wall:?}"
    );
}
