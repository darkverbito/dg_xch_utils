// Full-conditions golden digest over committed mainnet blocks.
//
// The cost walls prove each block's total cost is exact, but cost is one number — two condition
// parses can disagree while costing the same. This gate pins EVERYTHING the VM emits for a block:
// every spend's coin id, parent, puzzle hash, amount, every created coin with hint, every timelock
// and announcement, every agg-sig class, every message, reserve_fee, and the addition/removal
// totals. The whole `SpendBundleConditions` is canonicalized (spends sorted by coin id, the
// create-coin set sorted) and hashed; one digest per block, frozen in
// `fixtures/clvm_conditions_digests.json` (UPDATE_GOLDEN=1 re-harvests).
//
// Any semantic drift in any operator, the condition parser, or the ROM anywhere in these blocks
// moves a digest. The corpus is every committed generator fixture: 46 contiguous real mainnet
// blocks (9,179,155..9,179,200), three cost-maxed blocks, and the standalone generator fixtures —
// no chain sync required.

use dg_xch_core::blockchain::full_block::FullBlock;
use dg_xch_core::blockchain::spend_bundle_conditions::SpendBundleConditions;
use dg_xch_core::clvm::program::SerializedProgram;
use dg_xch_core::consensus::block_generator::{
    BlockGeneratorFlags, BlockGeneratorInput, GeneratorReference, execute_block_generator_result,
};
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_core::protocols::full_node::RespondBlocks;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Cursor;

const GOLDEN_PATH: &str = "tests/fixtures/clvm_conditions_digests.json";

fn load_range(bytes: &[u8]) -> Vec<FullBlock> {
    RespondBlocks::from_bytes(&mut Cursor::new(bytes), ChiaProtocolVersion::default())
        .expect("RespondBlocks deserializes")
        .blocks
}

/// Canonical JSON of the whole conditions object: spends sorted by coin id, each spend's
/// create-coin set sorted by its own serialization. Serde emits every field, so a field added to
/// `Spend` later automatically joins the digest.
fn digest_of(conds: &SpendBundleConditions) -> String {
    let mut value = serde_json::to_value(conds).expect("conditions serialize");
    if let Some(spends) = value.get_mut("spends").and_then(|s| s.as_array_mut()) {
        for spend in spends.iter_mut() {
            if let Some(cc) = spend.get_mut("create_coin").and_then(|c| c.as_array_mut()) {
                cc.sort_by_key(|c| serde_json::to_string(c).expect("serializes"));
            }
        }
        spends.sort_by_key(|s| {
            s.get("coin_id")
                .map(|c| serde_json::to_string(c).expect("serializes"))
                .unwrap_or_default()
        });
    }
    let canonical = serde_json::to_string(&value).expect("canonical form serializes");
    hex::encode(Sha256::digest(canonical.as_bytes()))
}

/// Fixture files carry the generator on line one; later lines are auxiliary data.
fn first_line(fixture: &str) -> &str {
    fixture.lines().next().expect("fixture has content").trim()
}

fn input_for(hex_src: &str, height: u32, refs: Vec<GeneratorReference>) -> BlockGeneratorInput {
    BlockGeneratorInput {
        transactions_generator: SerializedProgram::from_hex(first_line(hex_src))
            .expect("generator fixture is valid hex"),
        generator_refs: refs,
        constants: MAINNET,
        height,
        flags: BlockGeneratorFlags::for_height(&MAINNET, height),
    }
}

fn run_digest(input: &BlockGeneratorInput) -> String {
    let conds = execute_block_generator_result(input).expect("generator runs");
    let first = digest_of(&conds);
    // Two runs, one digest — pins determinism at full-block scale, not just per-op.
    let again = execute_block_generator_result(input).expect("generator runs");
    assert_eq!(first, digest_of(&again), "same block, two runs, different conditions");
    first
}

fn collect() -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();

    let standalone: [(&str, &str, u32); 5] = [
        (
            "block-834752",
            include_str!("../../core/tests/fixtures/chia_generator_tests/block-834752.txt"),
            834_752,
        ),
        (
            "block-834752-compressed",
            include_str!("../../core/tests/fixtures/chia_generator_tests/block-834752-compressed.txt"),
            834_752,
        ),
        (
            "block-9189472",
            include_str!("../../core/tests/fixtures/heavy_generators/block-9189472.txt"),
            9_189_472,
        ),
        (
            "block-9189475",
            include_str!("../../core/tests/fixtures/heavy_generators/block-9189475.txt"),
            9_189_475,
        ),
        (
            "block-9189481",
            include_str!("../../core/tests/fixtures/heavy_generators/block-9189481.txt"),
            9_189_481,
        ),
    ];
    for (name, hex_src, height) in standalone {
        out.insert(name.to_string(), run_digest(&input_for(hex_src, height, vec![])));
    }

    let refs = vec![GeneratorReference {
        height: 4_671_893,
        index: 0,
        generator: SerializedProgram::from_hex(first_line(include_str!(
            "../../core/tests/fixtures/chia_generator_tests/block-4671894.env"
        )))
        .expect("ref generator"),
    }];
    out.insert(
        "block-4671894".to_string(),
        run_digest(&input_for(
            include_str!("../../core/tests/fixtures/chia_generator_tests/block-4671894.txt"),
            4_671_894,
            refs,
        )),
    );

    let mut blocks = load_range(include_bytes!("fixtures/blocks_9179155_9179186.bin"));
    blocks.extend(load_range(include_bytes!("fixtures/blocks_9179187_9179200.bin")));
    let mut tx_blocks = 0;
    for block in &blocks {
        let Some(generator) = &block.transactions_generator else {
            continue;
        };
        let height = block.height();
        assert!(
            block.transactions_generator_ref_list.is_empty(),
            "height {height}: unexpected generator refs in the committed window"
        );
        let input = BlockGeneratorInput {
            transactions_generator: generator.clone(),
            generator_refs: vec![],
            constants: MAINNET,
            height,
            flags: BlockGeneratorFlags::for_height(&MAINNET, height),
        };
        out.insert(format!("mainnet-{height}"), run_digest(&input));
        tx_blocks += 1;
    }
    // 10 of the 46 blocks in the committed window carry transactions.
    assert!(
        tx_blocks >= 10,
        "only {tx_blocks} transaction blocks in the committed window — fixture damage?"
    );

    out
}

#[test]
fn block_conditions_digests_match_golden() {
    let all = collect();

    if std::env::var("UPDATE_GOLDEN").is_ok() {
        std::fs::write(GOLDEN_PATH, serde_json::to_string_pretty(&all).expect("serializes"))
            .expect("golden file writes");
        eprintln!("  wrote {} digests to {GOLDEN_PATH}", all.len());
        return;
    }

    let stored = std::fs::read_to_string(GOLDEN_PATH)
        .expect("golden file missing — harvest once with UPDATE_GOLDEN=1");
    let stored: BTreeMap<String, String> = serde_json::from_str(&stored).expect("golden parses");
    let mut diverged = Vec::new();
    for (name, digest) in &all {
        match stored.get(name) {
            Some(want) if want == digest => {}
            Some(_) => diverged.push(name.clone()),
            None => panic!("{name}: no golden digest — harvest with UPDATE_GOLDEN=1"),
        }
    }
    assert!(
        diverged.is_empty(),
        "conditions diverged from pinned behavior on: {} — the VM no longer produces \
         consensus-identical results for real mainnet blocks",
        diverged.join(", ")
    );
    assert_eq!(all.len(), stored.len(), "golden file holds digests for blocks no longer tested");
    eprintln!("  {} block digests hold", all.len());
}
