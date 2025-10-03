use dg_xch_core::blockchain::condition_opcode::ConditionOpcode;
use dg_xch_core::blockchain::sized_bytes::{Bytes32, Bytes48};
use dg_xch_core::clvm::sexp::{AtomBuf, SExp};

#[must_use]
pub fn make_create_coin_condition(
    puzzle_hash: Bytes32,
    amount: u64,
    memos: &[Vec<u8>],
) -> Vec<SExp> {
    if memos.is_empty() {
        vec![
            ConditionOpcode::CreateCoin.into(),
            puzzle_hash.into(),
            amount.into(),
        ]
    } else {
        vec![
            ConditionOpcode::CreateCoin.into(),
            puzzle_hash.into(),
            amount.into(),
            memos
                .iter()
                .map(Vec::as_slice)
                .map(Into::into)
                .collect::<Vec<SExp>>()
                .into(),
        ]
    }
}

#[must_use]
pub fn make_assert_aggsig_condition(public_key: &Bytes48) -> Vec<SExp> {
    vec![ConditionOpcode::AggSigUnsafe.into(), public_key.into()]
}

#[must_use]
pub fn make_assert_my_coin_id_condition(coin_name: &Bytes32) -> Vec<SExp> {
    vec![ConditionOpcode::AssertMyCoinId.into(), coin_name.into()]
}

#[must_use]
pub fn make_assert_absolute_height_exceeds_condition(block_index: u32) -> Vec<SExp> {
    vec![
        ConditionOpcode::AssertHeightAbsolute.into(),
        block_index.into(),
    ]
}

#[must_use]
pub fn make_assert_relative_height_exceeds_condition(block_index: u32) -> Vec<SExp> {
    vec![
        ConditionOpcode::AssertHeightRelative.into(),
        block_index.into(),
    ]
}

#[must_use]
pub fn make_assert_absolute_seconds_exceeds_condition(time: u64) -> Vec<SExp> {
    vec![ConditionOpcode::AssertSecondsAbsolute.into(), time.into()]
}

#[must_use]
pub fn make_assert_relative_seconds_exceeds_condition(time: u64) -> Vec<SExp> {
    vec![ConditionOpcode::AssertSecondsRelative.into(), time.into()]
}

#[must_use]
pub fn make_reserve_fee_condition(fee: u64) -> Vec<SExp> {
    vec![ConditionOpcode::ReserveFee.into(), fee.into()]
}

#[must_use]
pub fn make_assert_coin_announcement(announcement_hash: &Bytes32) -> Vec<SExp> {
    vec![
        ConditionOpcode::AssertCoinAnnouncement.into(),
        announcement_hash.into(),
    ]
}

#[must_use]
pub fn make_assert_puzzle_announcement(announcement_hash: &Bytes32) -> Vec<SExp> {
    vec![
        ConditionOpcode::AssertPuzzleAnnouncement.into(),
        announcement_hash.into(),
    ]
}

#[must_use]
pub fn make_create_coin_announcement(message: &[u8]) -> Vec<SExp> {
    vec![
        ConditionOpcode::CreateCoinAnnouncement.into(),
        SExp::Atom(AtomBuf::new(message.to_vec())),
    ]
}

#[must_use]
pub fn make_create_puzzle_announcement(message: &[u8]) -> Vec<SExp> {
    vec![
        ConditionOpcode::CreatePuzzleAnnouncement.into(),
        SExp::Atom(AtomBuf::new(message.to_vec())),
    ]
}

#[must_use]
pub fn make_assert_my_parent_id(parent_id: Bytes32) -> Vec<SExp> {
    vec![ConditionOpcode::AssertMyParentId.into(), parent_id.into()]
}

#[must_use]
pub fn make_assert_my_puzzlehash(puzzlehash: Bytes32) -> Vec<SExp> {
    vec![
        ConditionOpcode::AssertMyPuzzlehash.into(),
        puzzlehash.into(),
    ]
}

#[must_use]
pub fn make_assert_my_amount(amount: u64) -> Vec<SExp> {
    vec![ConditionOpcode::AssertMyAmount.into(), amount.into()]
}
