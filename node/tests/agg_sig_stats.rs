// Aggregate-signature workload census over an era corpus (perf instrumentation, not a gate).
//
// The post-roll pg-b flamegraph put 26.3% of sync CPU in blst pairing under
// `validate_block_aggregate_signature`. Whether the cross-block batch trick (one final
// exponentiation per window instead of per block) or per-unique-key dedup (uncompress +
// subgroup-check once per distinct pk) can recover any of it depends entirely on the workload
// shape: pairs per block and the unique-pk ratio. This census measures both on a real corpus.
//
//   DGXCH_ERA_CORPUS=<dir> cargo test --release -p dg_xch_node --test agg_sig_stats -- --ignored --nocapture

use dg_xch_core::blockchain::full_block::FullBlock;
use dg_xch_core::consensus::block_generator::{
    BlockGeneratorFlags, BlockGeneratorInput, GeneratorReference, execute_block_generator_result,
};
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_core::protocols::full_node::RespondBlocks;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Cursor;
use std::path::PathBuf;

fn corpus_dir() -> PathBuf {
    PathBuf::from(
        std::env::var("DGXCH_ERA_CORPUS").expect("set DGXCH_ERA_CORPUS to an era corpus dir"),
    )
}

// Window blocks (blocks_<a>_<b>.bin) plus out-of-window generator-ref blocks
// (ref_block_<h>.bin), exactly the corpus layout era_replay consumes.
fn load_blocks(dir: &std::path::Path) -> BTreeMap<u32, FullBlock> {
    let mut by_height = BTreeMap::new();
    for entry in std::fs::read_dir(dir).expect("corpus dir") {
        let name = entry
            .expect("dir entry")
            .file_name()
            .to_string_lossy()
            .to_string();
        let ranged = name.starts_with("blocks_") && name.ends_with(".bin");
        let reffed = name.starts_with("ref_block_") && name.ends_with(".bin");
        if !(ranged || reffed) {
            continue;
        }
        let bytes = std::fs::read(dir.join(&name)).expect("block file");
        let msg =
            RespondBlocks::from_bytes(&mut Cursor::new(&bytes[..]), ChiaProtocolVersion::default())
                .expect("RespondBlocks deserializes");
        for b in msg.blocks {
            by_height.insert(b.height(), b);
        }
    }
    by_height
}

#[test]
#[ignore = "requires an uncommitted era corpus (DGXCH_ERA_CORPUS)"]
fn census_agg_sig_pairs_per_block() {
    let blocks = load_blocks(&corpus_dir());
    let generators: HashMap<u32, _> = blocks
        .iter()
        .filter_map(|(h, b)| b.transactions_generator.clone().map(|g| (*h, g)))
        .collect();

    let mut tx_blocks = 0_u64;
    let mut skipped_refs = 0_u64;
    let mut failed = 0_u64;
    let mut total_pairs = 0_u64;
    let mut total_unique = 0_u64;
    let mut blocks_with_pairs = 0_u64;
    let mut min_pairs = u64::MAX;
    let mut max_pairs = 0_u64;
    let mut pair_hist = BTreeMap::<u64, u64>::new(); // bucketed by powers of two
    let mut cross_window_unique = HashSet::<[u8; 48]>::new();
    let mut cross_window_pairs = 0_u64;

    for (h, block) in &blocks {
        let Some(generator) = block.transactions_generator.clone() else {
            continue;
        };
        let mut refs = Vec::new();
        let mut missing = false;
        for (i, ref_height) in block.transactions_generator_ref_list.iter().enumerate() {
            match generators.get(ref_height) {
                Some(g) => refs.push(GeneratorReference {
                    height: *ref_height,
                    index: u32::try_from(i).expect("ref index fits u32"),
                    generator: g.clone(),
                }),
                None => missing = true,
            }
        }
        if missing {
            skipped_refs += 1;
            continue;
        }
        tx_blocks += 1;
        let input = BlockGeneratorInput {
            transactions_generator: generator,
            generator_refs: refs,
            constants: MAINNET,
            height: *h,
            flags: BlockGeneratorFlags::for_height(&MAINNET, *h),
        };
        let conds = match execute_block_generator_result(&input) {
            Ok(c) => c,
            Err(_) => {
                failed += 1;
                continue;
            }
        };

        let mut pairs = 0_u64;
        let mut unique = HashSet::<[u8; 48]>::new();
        let mut count = |pk: &[u8]| {
            pairs += 1;
            if pk.len() == 48 {
                let mut key = [0_u8; 48];
                key.copy_from_slice(pk);
                unique.insert(key);
                cross_window_unique.insert(key);
            }
        };
        for (pk, _) in &conds.agg_sig_unsafe {
            count(pk.as_slice());
        }
        for spend in &conds.spends {
            for (pk, _) in spend
                .agg_sig_me
                .iter()
                .chain(&spend.agg_sig_parent)
                .chain(&spend.agg_sig_puzzle)
                .chain(&spend.agg_sig_amount)
                .chain(&spend.agg_sig_puzzle_amount)
                .chain(&spend.agg_sig_parent_amount)
                .chain(&spend.agg_sig_parent_puzzle)
            {
                count(pk.as_slice());
            }
        }
        cross_window_pairs += pairs;
        if pairs > 0 {
            blocks_with_pairs += 1;
            total_pairs += pairs;
            total_unique += unique.len() as u64;
            min_pairs = min_pairs.min(pairs);
            max_pairs = max_pairs.max(pairs);
            *pair_hist
                .entry(pairs.next_power_of_two().trailing_zeros() as u64)
                .or_insert(0) += 1;
        }
    }

    eprintln!("=== agg-sig census ===");
    eprintln!(
        "tx blocks run: {tx_blocks} (skipped {skipped_refs} unresolved-ref, {failed} generator-failed)"
    );
    eprintln!("blocks with >=1 pair: {blocks_with_pairs}");
    eprintln!(
        "pairs: total {total_pairs}, per-block mean {:.1}, min {}, max {max_pairs}",
        total_pairs as f64 / blocks_with_pairs.max(1) as f64,
        if min_pairs == u64::MAX { 0 } else { min_pairs },
    );
    eprintln!(
        "unique pks per block: mean {:.1} -> within-block dedup ratio {:.3} (pk checks avoided: {:.1}%)",
        total_unique as f64 / blocks_with_pairs.max(1) as f64,
        total_unique as f64 / total_pairs.max(1) as f64,
        100.0 * (1.0 - total_unique as f64 / total_pairs.max(1) as f64),
    );
    eprintln!(
        "cross-window: {} unique pks over {cross_window_pairs} pairs (LRU-across-blocks ceiling {:.1}%)",
        cross_window_unique.len(),
        100.0 * (1.0 - cross_window_unique.len() as f64 / cross_window_pairs.max(1) as f64),
    );
    eprintln!("pairs-per-block histogram (bucket = next power of two exponent):");
    for (bucket, n) in &pair_hist {
        eprintln!("  <=2^{bucket:<2} {n}");
    }
    assert!(tx_blocks > 0, "corpus contained runnable tx blocks");
}
