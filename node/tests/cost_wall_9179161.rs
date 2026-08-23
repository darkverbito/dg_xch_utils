use dg_xch_core::blockchain::full_block::FullBlock;
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_core::protocols::full_node::RespondBlocks;
use dg_xch_node::NativePrimitives;
use dg_xch_node::engine::run_body_expensive;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use std::io::Cursor;

// The live tip-follow wall at mainnet 9,179,161..9,179,192 (2026-08-21): a node wedged
// rejecting every follow step with `InvalidBlockCost`, peak frozen, liveness restart loop. The
// window is real mainnet bytes wire-captured from a synced chia 2.x node (`block_fetch` →
// RequestBlocks/RespondBlocks frames, the corpus-import format). Every transaction block in the
// window must execute to EXACTLY its declared `transactions_info.cost` — the body rule the live
// node holds each followed block to (chia `validate_block_body` rule 9, INVALID_BLOCK_COST) —
// and its aggregate signature must verify. This exact window wedged production; it stays green
// forever.

fn load_range(bytes: &[u8]) -> Vec<FullBlock> {
    RespondBlocks::from_bytes(&mut Cursor::new(bytes), ChiaProtocolVersion::default())
        .expect("RespondBlocks deserializes")
        .blocks
}

#[test]
fn mainnet_9179155_9179200_costs_are_exact() {
    let mut blocks = load_range(include_bytes!("fixtures/blocks_9179155_9179186.bin"));
    blocks.extend(load_range(include_bytes!(
        "fixtures/blocks_9179187_9179200.bin"
    )));
    assert_eq!(blocks.first().map(FullBlock::height), Some(9_179_155));
    assert_eq!(blocks.last().map(FullBlock::height), Some(9_179_200));

    let primitives = NativePrimitives;
    let mut generator_blocks = 0u32;
    let mut failures = Vec::new();
    for block in &blocks {
        if block.transactions_generator.is_none() {
            continue;
        }
        generator_blocks += 1;
        // Post-SF9 (8,655,000): generator back-reference lists are banned, so the window is
        // self-contained — no out-of-window ref resolution needed.
        assert!(
            block.transactions_generator_ref_list.is_empty(),
            "unexpected generator refs at {}",
            block.height()
        );
        let declared = block
            .transactions_info
            .as_ref()
            .expect("generator block has transactions_info")
            .cost;
        match run_body_expensive(&primitives, &MAINNET, block, &[], true) {
            Ok((conds, verified)) => {
                assert!(verified, "aggregate signature not verified");
                if conds.cost != declared {
                    failures.push(format!(
                        "height {}: computed cost {} != declared {} (delta {})",
                        block.height(),
                        conds.cost,
                        declared,
                        i128::from(conds.cost) - i128::from(declared)
                    ));
                }
            }
            Err(e) => failures.push(format!("height {}: body run failed: {e:?}", block.height())),
        }
    }
    assert_eq!(
        generator_blocks, 10,
        "expected 10 generator blocks in window"
    );
    assert!(failures.is_empty(), "cost walls:\n{}", failures.join("\n"));
}
