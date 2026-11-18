#![cfg(feature = "bls")]

use dg_xch_core::blockchain::{coin::Coin, coin_spend::CoinSpend, spend_bundle::SpendBundle};
use dg_xch_core::clvm::program::Program;
use dg_xch_core::clvm::sexp::SExp;
use dg_xch_core::consensus::constants::MAINNET;

#[test]
fn unsigned_clvm_values_keep_their_positive_sign() {
    for amount in [
        0u64,
        127,
        128,
        255,
        256,
        i64::MAX as u64,
        1u64 << 63,
        u64::MAX,
    ] {
        assert_eq!(
            Program::to(amount).as_int().unwrap().to_string(),
            amount.to_string()
        );
    }
    assert_eq!(
        Program::to(u64::MAX).as_vec().unwrap(),
        [vec![0], vec![0xff; 8]].concat()
    );
    assert_eq!(Program::to(u16::MAX).as_vec().unwrap(), vec![0, 0xff, 0xff]);
    assert_eq!(
        Program::to(u32::MAX).as_vec().unwrap(),
        [vec![0], vec![0xff; 4]].concat()
    );
    assert_eq!(
        Program::to(u128::MAX).as_vec().unwrap(),
        [vec![0], vec![0xff; 16]].concat()
    );
    assert_eq!(
        Program::to(usize::MAX).as_int().unwrap().to_string(),
        usize::MAX.to_string()
    );
    assert_eq!(Program::to(-1i64).as_vec().unwrap(), vec![0xff]);
    assert_eq!(
        Program::to(i64::MIN).as_int().unwrap().to_string(),
        i64::MIN.to_string()
    );
}

#[test]
fn several_maximum_amount_coins_do_not_overflow_offline_validation() {
    // Each output conserves its input. The aggregate value is larger than u64;
    // unused diagnostic counters must not panic or wrap on this valid shape.
    let condition = Program::to(vec![
        SExp::from(51u64),
        SExp::from(vec![7u8; 32]),
        SExp::from(u64::MAX),
    ]);
    let puzzle = Program::to((1u64, vec![condition.sexp().to_owned()]));
    let spends = [1u8, 2]
        .into_iter()
        .map(|parent| CoinSpend {
            coin: Coin {
                parent_coin_info: [parent; 32].into(),
                puzzle_hash: puzzle.tree_hash(),
                amount: u64::MAX,
            },
            puzzle_reveal: puzzle.serialized().unwrap(),
            solution: Program::to(0).serialized().unwrap(),
        })
        .collect();
    // Empty signature uses the canonical G2 identity, not an all-zero point.
    let mut identity = [0; 96];
    identity[0] = 0xc0;
    let bundle = SpendBundle {
        coin_spends: spends,
        aggregated_signature: identity.into(),
        authorization: Default::default(),
    };
    bundle
        .validate(Some(10_000_000), 0, &MAINNET, false)
        .unwrap();
}
