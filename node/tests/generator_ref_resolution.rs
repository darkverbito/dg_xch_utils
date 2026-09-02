//! Storage-side coverage: block-level generator back-reference resolution.
//!
//! Each `transactions_generator_ref_list` height resolves to a prior block's generator in
//! ref-list order, and a missing referenced generator is `GENERATOR_REF_HAS_NO_GENERATOR`.
//! Wired in `BlockStore::get_generator_at_height` + `Engine::resolve_generator_refs`. These
//! tests confirm two generator-bearing mainnet bodies onto the confirmed chain and exercise
//! resolution directly; fixtures are the vendored mainnet blocks.

mod common;

use dg_xch_core::consensus::block_generator::transactions_generator_refs_root;
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_core::errors::ChiaError;
use dg_xch_node::{Engine, NativePrimitives, NodeError};
use dg_xch_stores::{BlockStore, SqliteStore};

// Confirm the fixture chain (heights 5,000,000..5,000,029) and append the two bodies that carry generators
// (5,000,000 and 5,000,004), so both resolve by height on the confirmed main chain.
async fn store_with_two_confirmed_generators() -> SqliteStore {
    let records = common::load_records();
    let store = common::new_store().await;
    store.add_block_records(&records).await.unwrap();

    // Flip in_main_chain=1 down the whole fixture chain via its tip.
    let top = records
        .iter()
        .max_by_key(|r| r.height)
        .expect("records")
        .header_hash;
    store.set_peak(&top).await.unwrap();

    let mut batch = store.begin().await.unwrap();
    for h in [5_000_000u32, 5_000_004] {
        let fb = common::load_full_block(h);
        store
            .append_many(&mut batch, std::slice::from_ref(&fb))
            .await
            .unwrap();
    }
    store.commit(batch).await.unwrap();
    store
}

fn engine(store: SqliteStore) -> Engine<SqliteStore, NativePrimitives> {
    Engine::new(store, NativePrimitives, MAINNET)
}

// The store lookup resolves a confirmed prior generator by height.
#[tokio::test]
async fn get_generator_at_height_returns_the_confirmed_generator() {
    let store = store_with_two_confirmed_generators().await;
    let expected = common::load_full_block(5_000_000)
        .transactions_generator
        .expect("fixture generator");
    let got = store
        .get_generator_at_height(5_000_000)
        .await
        .unwrap()
        .expect("generator resolved by height");
    assert_eq!(
        got.as_ref(),
        expected.as_ref(),
        "resolved generator bytes match the stored block"
    );
}

// An absent height and a confirmed-but-bodiless height both miss — the store surfaces it as None; the engine
// turns it into the canonical failure (next test).
#[tokio::test]
async fn get_generator_at_height_missing_is_none() {
    let store = store_with_two_confirmed_generators().await;
    assert!(
        store
            .get_generator_at_height(9_999_999)
            .await
            .unwrap()
            .is_none(),
        "absent height has no generator"
    );
    assert!(
        store
            .get_generator_at_height(5_000_029)
            .await
            .unwrap()
            .is_none(),
        "confirmed record with no body has no generator"
    );
}

// (a) Order preserved: the resolved refs follow the ref-list, not the store's natural order; and (c) the
// refs_root computed over the actual heights is order-sensitive and differs from the empty-list root — the
// exact predicate validate_body uses to reject a mismatched ti.generator_refs_root.
#[tokio::test]
async fn resolve_generator_refs_preserves_ref_list_order() {
    let g0 = common::load_full_block(5_000_000)
        .transactions_generator
        .unwrap();
    let g4 = common::load_full_block(5_000_004)
        .transactions_generator
        .unwrap();
    let eng = engine(store_with_two_confirmed_generators().await);

    let refs = eng
        .resolve_generator_refs(&[5_000_004, 5_000_000])
        .await
        .expect("both refs resolve");
    assert_eq!(
        refs.iter().map(|r| r.height).collect::<Vec<_>>(),
        vec![5_000_004, 5_000_000],
        "heights follow the ref-list order"
    );
    assert_eq!(
        refs[0].generator.as_ref(),
        g4.as_ref(),
        "first ref is height 5,000,004's generator"
    );
    assert_eq!(
        refs[1].generator.as_ref(),
        g0.as_ref(),
        "second ref is height 5,000,000's generator"
    );

    // Reversing the ref-list reverses the resolved refs (order is not incidental).
    let rev = eng
        .resolve_generator_refs(&[5_000_000, 5_000_004])
        .await
        .expect("both refs resolve");
    assert_eq!(
        rev.iter().map(|r| r.height).collect::<Vec<_>>(),
        vec![5_000_000, 5_000_004],
    );

    // (c) refs_root rejection predicate: order-sensitive, and different from the empty-list root the node
    // formerly compared against.
    let root_fwd = transactions_generator_refs_root(&[5_000_004, 5_000_000]).unwrap();
    let root_rev = transactions_generator_refs_root(&[5_000_000, 5_000_004]).unwrap();
    let root_empty = transactions_generator_refs_root(&[]).unwrap();
    assert_ne!(
        root_fwd, root_rev,
        "a mis-ordered ti.generator_refs_root is rejected"
    );
    assert_ne!(
        root_fwd, root_empty,
        "a real ref-list root differs from the empty-list root"
    );
}

// (b) A referenced height with no confirmed generator is a validation failure
// (GENERATOR_REF_HAS_NO_GENERATOR), never a silent pass — including a partial resolution.
#[tokio::test]
async fn resolve_generator_refs_missing_height_is_validation_failure() {
    let eng = engine(store_with_two_confirmed_generators().await);

    let absent = eng.resolve_generator_refs(&[9_999_999]).await;
    assert!(
        matches!(
            absent,
            Err(NodeError::Consensus(ChiaError::GeneratorRefHasNoGenerator))
        ),
        "absent referenced height fails closed, got {absent:?}"
    );

    let partial = eng.resolve_generator_refs(&[5_000_000, 9_999_999]).await;
    assert!(
        matches!(
            partial,
            Err(NodeError::Consensus(ChiaError::GeneratorRefHasNoGenerator))
        ),
        "a missing ref among present ones fails closed, got {partial:?}"
    );
}
