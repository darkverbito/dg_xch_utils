mod common;

use dg_xch_core::blockchain::full_block::FullBlock;
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_core::protocols::full_node::RespondBlocks;
use dg_xch_node::sync::{BlockRangeSource, SyncError, WindowReadahead};
use dg_xch_node::{Chaser, Engine, NativePrimitives, SyncConfig};
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use dg_xch_stores::BlockStore;
use std::collections::HashMap;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

// The readahead A/B judge, offline: the fetch-starved follow. Real mainnet blocks 0..=1023
// (the genesis corpus) served through sources with an injected per-range latency modeling public
// peers serving tx-dense bodies slower than the validator burns them (measured: ~14s fetch
// wait of an 18s window wall). Mode A is the one-window-overlap driver shape — ONE window of prefetch
// overlap from ONE peer. Mode B is the K-deep multi-peer readahead. Same corpus, same
// validation, fresh store each; the wall-clock ratio and the measured validator idle fraction
// are the acceptance readout.
//   DGXCH_CORPUS=<genesis dir> cargo test --release -p dg_xch_node --test readahead_latency -- --ignored --nocapture

const WINDOW: u32 = 32;
const FETCH_LATENCY: Duration = Duration::from_secs(6);

// Mode A single-prefetch bookkeeping: (from, to, in-flight fetch task).
type PrefetchTask = (
    u32,
    u32,
    tokio::task::JoinHandle<Result<Vec<FullBlock>, SyncError>>,
);

struct LatentSource {
    id: u64,
    blocks: Arc<HashMap<u32, FullBlock>>,
    latency: Duration,
}

#[async_trait::async_trait]
impl BlockRangeSource for LatentSource {
    fn peer_id(&self) -> u64 {
        self.id
    }
    fn is_closed(&self) -> bool {
        false
    }
    async fn fetch_range(&self, start: u32, end: u32) -> Result<Vec<FullBlock>, SyncError> {
        tokio::time::sleep(self.latency).await;
        Ok((start..=end)
            .filter_map(|h| self.blocks.get(&h).cloned())
            .collect())
    }
}

fn load_corpus() -> Arc<HashMap<u32, FullBlock>> {
    let dir = PathBuf::from(
        std::env::var("DGXCH_CORPUS").expect("set DGXCH_CORPUS to the genesis corpus dir"),
    );
    let mut blocks = HashMap::new();
    for entry in std::fs::read_dir(&dir).expect("corpus dir readable") {
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
    Arc::new(blocks)
}

async fn fresh_chaser() -> Chaser<Arc<dg_xch_stores::SqliteStore>, NativePrimitives> {
    let store = Arc::new(common::new_store().await);
    Chaser::new(
        Engine::new(store, NativePrimitives, MAINNET),
        SyncConfig::default(),
    )
}

// Mode A — the one-window-overlap driver: one prefetch window in flight from one peer, awaited at the
// handoff, next prefetch spawned before validation.
async fn run_k1(blocks: Arc<HashMap<u32, FullBlock>>, last: u32) -> (Duration, Duration) {
    let mut chaser = fresh_chaser().await;
    let source: Arc<dyn BlockRangeSource> = Arc::new(LatentSource {
        id: 1,
        blocks,
        latency: FETCH_LATENCY,
    });
    let started = Instant::now();
    let mut fetch_wait = Duration::ZERO;
    let mut prefetch: Option<PrefetchTask> = None;
    let mut from = 0u32;
    while from <= last {
        let to = last.min(from + WINDOW - 1);
        let waited = Instant::now();
        let fetched = match prefetch.take() {
            Some((pf, pt, handle)) if pf == from && pt == to => handle
                .await
                .expect("prefetch task")
                .expect("prefetch fetch"),
            other => {
                if let Some((_, _, handle)) = other {
                    handle.abort();
                }
                source.fetch_range(from, to).await.expect("direct fetch")
            }
        };
        fetch_wait += waited.elapsed();
        if to < last {
            let nf = to + 1;
            let nt = last.min(nf + WINDOW - 1);
            let src = source.clone();
            prefetch = Some((
                nf,
                nt,
                tokio::spawn(async move { src.fetch_range(nf, nt).await }),
            ));
        }
        let mut blocks = fetched;
        blocks.sort_by_key(FullBlock::height);
        chaser.follow_blocks(&blocks).await.expect("follow window");
        from = to + 1;
    }
    let peak = chaser.engine().store().get_peak().await.expect("peak");
    assert_eq!(
        peak.map(|(_, h)| h),
        Some(last),
        "k1 confirms the corpus tip"
    );
    (started.elapsed(), fetch_wait)
}

// Mode B — the readahead: K windows in flight across distinct peers, adaptive depth.
async fn run_readahead(
    blocks: Arc<HashMap<u32, FullBlock>>,
    last: u32,
    peers: u64,
) -> (Duration, Duration, Duration) {
    let mut chaser = fresh_chaser().await;
    let sources: Vec<Arc<dyn BlockRangeSource>> = (0..peers)
        .map(|id| {
            Arc::new(LatentSource {
                id,
                blocks: blocks.clone(),
                latency: FETCH_LATENCY,
            }) as Arc<dyn BlockRangeSource>
        })
        .collect();
    let metrics = chaser.metrics().clone();
    let mut ra = WindowReadahead::new(metrics.clone(), Duration::from_secs(30));
    let started = Instant::now();
    let mut from = 0u32;
    while from <= last {
        let to = last.min(from + WINDOW - 1);
        let step_started = Instant::now();
        let taken = ra.take(from, to).await;
        if to < last {
            ra.fill(&sources, to + 1, last, WINDOW);
        }
        let mut window = match taken {
            Some(w) => w,
            None => {
                let waited = Instant::now();
                let direct = sources[0]
                    .fetch_range(from, to)
                    .await
                    .expect("direct fetch");
                metrics
                    .follow_fetch_wait_micros
                    .fetch_add(waited.elapsed().as_micros() as u64, Ordering::Relaxed);
                direct
            }
        };
        window.sort_by_key(FullBlock::height);
        chaser.follow_blocks(&window).await.expect("follow window");
        metrics
            .follow_step_micros
            .fetch_add(step_started.elapsed().as_micros() as u64, Ordering::Relaxed);
        from = to + 1;
    }
    let peak = chaser.engine().store().get_peak().await.expect("peak");
    assert_eq!(
        peak.map(|(_, h)| h),
        Some(last),
        "readahead confirms the corpus tip"
    );
    let wait = Duration::from_micros(metrics.follow_fetch_wait_micros.load(Ordering::Relaxed));
    let step = Duration::from_micros(metrics.follow_step_micros.load(Ordering::Relaxed));
    (started.elapsed(), wait, step)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the uncommitted genesis corpus (DGXCH_CORPUS)"]
async fn readahead_beats_single_window_prefetch_on_fetch_starved_follow() {
    let blocks = load_corpus();
    let last = *blocks.keys().max().expect("corpus non-empty");
    assert_eq!(*blocks.keys().min().expect("corpus non-empty"), 0);

    let (k1_wall, k1_wait) = run_k1(blocks.clone(), last).await;
    let (ra_wall, ra_wait, ra_step) = run_readahead(blocks, last, 8).await;

    let k1_rate = f64::from(last + 1) / k1_wall.as_secs_f64() * 60.0;
    let ra_rate = f64::from(last + 1) / ra_wall.as_secs_f64() * 60.0;
    let k1_idle = k1_wait.as_secs_f64() / k1_wall.as_secs_f64();
    let ra_idle = ra_wait.as_secs_f64() / ra_step.as_secs_f64().max(f64::EPSILON);
    eprintln!(
        "A/B fetch-starved follow ({} blocks, {}s/window fetch latency):\n\
         k1 prefetch: wall {:.1}s  {:.0} blk/min  validator idle {:.0}%\n\
         readahead:   wall {:.1}s  {:.0} blk/min  validator idle {:.0}%  speedup {:.2}x",
        last + 1,
        FETCH_LATENCY.as_secs(),
        k1_wall.as_secs_f64(),
        k1_rate,
        k1_idle * 100.0,
        ra_wall.as_secs_f64(),
        ra_rate,
        ra_idle * 100.0,
        k1_wall.as_secs_f64() / ra_wall.as_secs_f64(),
    );
    assert!(
        ra_wall.as_secs_f64() < k1_wall.as_secs_f64() / 2.0,
        "readahead must at least double the fetch-starved follow rate: k1 {k1_wall:?} vs readahead {ra_wall:?}"
    );
    assert!(
        ra_idle < k1_idle / 2.0,
        "measured validator idle fraction must at least halve: {k1_idle:.2} -> {ra_idle:.2}"
    );
}
