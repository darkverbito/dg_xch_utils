//! Port of `chia/_tests/blockchain/test_get_block_generator.py`.
//!
//! Chia's `get_block_generator(lookup, block_record)` assembles a block-level
//! generator from a block that references prior blocks' generators: it requires
//! the block to carry a generator, looks the referenced generators up by height
//! through a storage callback, and returns a `BlockGenerator(program, refs)`.
//!
//! dg_xch does NOT wire this resolution. `node/src/engine.rs` fails closed on any
//! non-empty `transactions_generator_ref_list` ("generator ref list resolution
//! not wired in this phase") and hardcodes empty refs. The `core` executor
//! (`execute_block_generator_result`) already consumes *caller-supplied* resolved
//! refs (see `historical_generator_ref_block_4671894...` in block_generator.rs),
//! but nothing fetches them from storage. These tests encode chia's contract; the
//! ref-resolving one is `#[ignore]`d until storage-side ref resolution lands.
//!
//! No Chia crate is used; fixtures are vendored bytes.

use dg_xch_core::clvm::program::{Program, SerializedProgram};
use dg_xch_core::clvm::sexp::SExp;
use dg_xch_core::consensus::block_generator::{
    BlockGeneratorFlags, BlockGeneratorInput, GeneratorReference, execute_block_generator_result,
    transactions_generator_refs_root,
};
use dg_xch_core::consensus::constants::MAINNET;

const HISTORICAL_BLOCK_4671894: &str =
    include_str!("fixtures/chia_generator_tests/block-4671894.txt");
const HISTORICAL_BLOCK_4671894_REF: &str =
    include_str!("fixtures/chia_generator_tests/block-4671894.env");

fn generator_line(fixture: &str) -> SerializedProgram {
    SerializedProgram::from_hex(fixture.lines().next().unwrap()).unwrap()
}

// chia test_no_refs: with an empty ref list, generator assembly succeeds without
// ever invoking the lookup callback. dg_xch's empty-ref path is fully supported,
// so this is locked in as live behavior.
#[test]
fn no_refs_generator_assembles_without_lookup() {
    let output = SExp::from(vec![SExp::from(Vec::<SExp>::new())]);
    let generator = Program::to((1_u8, output)).serialized().unwrap();
    let input = BlockGeneratorInput {
        transactions_generator: generator,
        generator_refs: Vec::new(),
        constants: MAINNET,
        height: 10,
        flags: BlockGeneratorFlags {
            simple_generator: true,
            ..Default::default()
        },
    };

    let conds = execute_block_generator_result(&input).unwrap();
    assert!(conds.spends.is_empty());
}

// chia test_get_block_generator: a block that references a prior block's generator
// validates once that generator is resolved (by height) from storage and supplied in
// ref-list order. Block 4,671,894 references the height-4,671,893 generator. With the
// reference resolved the block executes and yields conditions; the former unwired
// empty-ref path (engine.rs fail-closed hardcode) cannot reproduce them.
//
// Ref resolution now wired: node/src/engine.rs::resolve_generator_refs fetches each
// referenced generator from the confirmed chain (dg_xch_stores get_generator_at_height)
// and validate_body supplies them. The storage-side resolution is exercised end to end
// in node/tests/generator_ref_resolution.rs; this asserts the executor-level contract.
#[test]
fn ref_list_block_resolves_prior_generator_from_storage() {
    let generator = generator_line(HISTORICAL_BLOCK_4671894);
    let reference = SerializedProgram::from_hex(HISTORICAL_BLOCK_4671894_REF).unwrap();

    // The resolved path: the referenced generator is fetched by height and supplied.
    let resolved = BlockGeneratorInput {
        transactions_generator: generator,
        generator_refs: vec![GeneratorReference {
            height: 4_671_893,
            index: 0,
            generator: reference,
        }],
        constants: MAINNET,
        height: 4_671_894,
        flags: BlockGeneratorFlags::default(),
    };
    let conds =
        execute_block_generator_result(&resolved).expect("resolved ref-list block validates");
    assert!(
        conds.cost > 0,
        "a validated transaction block has a positive execution cost"
    );

    // The former unwired path hardcoded empty refs; it cannot reproduce the resolved
    // conditions (the generator consumes the reference argument).
    let unresolved = BlockGeneratorInput {
        generator_refs: Vec::new(),
        ..resolved
    };
    assert_ne!(execute_block_generator_result(&unresolved), Ok(conds));
}

// chia validate_block_body checks a block's generator_refs_root against the hash of its
// ref-list heights (transactions_generator_refs_root). engine.rs::validate_body now computes
// this from the ACTUAL referenced heights (formerly hardcoded `&[]`) and rejects a block whose
// ti.generator_refs_root disagrees. These lock the rejection predicate that branch relies on.
#[test]
fn generator_refs_root_is_order_sensitive() {
    let ab = transactions_generator_refs_root(&[4_671_893, 4_671_894]).unwrap();
    let ba = transactions_generator_refs_root(&[4_671_894, 4_671_893]).unwrap();
    assert_ne!(
        ab, ba,
        "ref-list order changes the refs_root, so a mis-ordered ti root is rejected"
    );
}

#[test]
fn generator_refs_root_of_real_heights_differs_from_empty() {
    let real = transactions_generator_refs_root(&[4_671_893]).unwrap();
    let empty = transactions_generator_refs_root(&[]).unwrap();
    assert_ne!(
        real, empty,
        "a real ref-list block's refs_root differs from the empty-list root the node formerly compared against"
    );
}
