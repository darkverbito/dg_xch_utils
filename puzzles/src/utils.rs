use crate::nft::NFT_STATE_LAYER_TREE_HASH;
use crate::p2_parent::P2_PARENT_PROGRAM;
use crate::singleton_top_layer::SINGLETON_TOP_LAYER_TREE_HASH;
use crate::singleton_top_layer_v1_1::SINGLETON_TOP_LAYER_V1_1_TREE_HASH;
use dg_xch_core::blockchain::condition_opcode::ConditionOpcode;
use dg_xch_core::blockchain::sized_bytes::{Bytes32, Bytes48};
use dg_xch_core::clvm::program::Program;
use dg_xch_core::clvm::sexp::{AtomBuf, SExp};
use lazy_static::lazy_static;
use log::error;

pub fn is_singleton_top_layer(program: &Program) -> bool {
    if let Ok((module, _)) = program.uncurry() {
        let mod_hash = module.tree_hash();
        return mod_hash == SINGLETON_TOP_LAYER_TREE_HASH;
    }
    false
}

pub fn is_singleton_top_layer_v1_1(program: &Program) -> bool {
    if let Ok((module, _)) = program.uncurry() {
        let mod_hash = module.tree_hash();
        return mod_hash == SINGLETON_TOP_LAYER_V1_1_TREE_HASH;
    }
    false
}

lazy_static! {
    pub static ref ACS_MU_PH: Bytes32 = Program::to(11u8).tree_hash(); //returns the third argument a.k.a the full solution
    pub static ref MIRROR_PUZZLE_HASH: Bytes32 = P2_PARENT_PROGRAM.curry(&[Program::to(1u8)]).expect("Failed to Calculate Mirror Hash").tree_hash();
}

#[derive(Debug)]
pub struct DataLayerSingletonInfo<'a> {
    pub launcher_id: Program<'a>,
    pub root: Program<'a>,
    pub inner_puzzle: Program<'a>,
}

pub fn datalayer_singleton_info<'a>(
    program: &'a Program<'a>,
) -> Option<DataLayerSingletonInfo<'static>> {
    if let Ok((module, singleton_curried_args)) = program.uncurry() {
        let mod_hash = module.tree_hash();
        if mod_hash == SINGLETON_TOP_LAYER_V1_1_TREE_HASH {
            if let Ok((dl_module, dl_curried_args)) =
                singleton_curried_args.at("rf").ok()?.uncurry()
            {
                let dl_mod_hash = dl_module.tree_hash();
                if dl_mod_hash == NFT_STATE_LAYER_TREE_HASH {
                    if Bytes32::try_from(&dl_curried_args.rest().ok()?.rest().ok()?.first().ok()?)
                        .ok()?
                        == *ACS_MU_PH
                    {
                        let launcher_id = singleton_curried_args.at("frf").ok()?.to_owned();
                        let root = dl_curried_args.at("rff").ok()?.to_owned();
                        let inner_puzzle = dl_curried_args.at("rrrf").ok()?.to_owned();
                        return Some(DataLayerSingletonInfo {
                            launcher_id,
                            root,
                            inner_puzzle,
                        });
                    } else {
                        error!("ACS_MU_PH Mismatch");
                    }
                } else {
                    error!("NFT_STATE_LAYER_TREE_HASH Mismatch");
                }
            } else {
                error!("Failed to Uncurry Singleton Args");
            }
        } else {
            error!("SINGLETON_TOP_LAYER_V1_1_TREE_HASH Mismatch");
        }
    }
    None
}

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
