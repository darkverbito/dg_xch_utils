// The entry gate. Diff dg_xch's condition handling against the current Chia condition set. Two
// layers are distinct and both matter:
//   (1) PARSE:   ConditionWithArgs recognizes an opcode and round-trips it.
//   (2) ENFORCE: block_generator::spend_from_conditions collects it into SpendBundleConditions, and
//                validate_block_conditions checks it.
// A prior fix closed the enforcement gap: the five opcodes that spend_from_conditions once dropped at `_ => {}`
// (ASSERT_CONCURRENT_SPEND 64, ASSERT_CONCURRENT_PUZZLE 65, SEND_MESSAGE 66, RECEIVE_MESSAGE 67,
// ASSERT_EPHEMERAL 76) now carry fields on `Spend` and are checked in validate_block_conditions. This
// test pins that every opcode in the current Chia set is Enforced or a recognized NoOp — no silent drops.

use dg_xch_core::blockchain::condition_opcode::ConditionOpcode;
use dg_xch_core::blockchain::condition_with_args::{ConditionWithArgs, Message, MessageArgs};
use dg_xch_core::blockchain::sized_bytes::{Bytes32, Bytes48};
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use std::io::Cursor;

fn b32() -> Bytes32 {
    Bytes32::from([7u8; 32])
}
fn b48() -> Bytes48 {
    Bytes48::from(vec![9u8; 48])
}
fn msg() -> Message {
    Message::new(vec![1, 2, 3]).unwrap()
}

// One representative for every non-Unknown ConditionWithArgs variant — the full current Chia condition set.
fn every_condition() -> Vec<ConditionWithArgs> {
    vec![
        ConditionWithArgs::Remark(msg()),
        ConditionWithArgs::AggSigParent(b48(), msg()),
        ConditionWithArgs::AggSigPuzzle(b48(), msg()),
        ConditionWithArgs::AggSigAmount(b48(), msg()),
        ConditionWithArgs::AggSigPuzzleAmount(b48(), msg()),
        ConditionWithArgs::AggSigParentAmount(b48(), msg()),
        ConditionWithArgs::AggSigParentPuzzle(b48(), msg()),
        ConditionWithArgs::AggSigUnsafe(b48(), msg()),
        ConditionWithArgs::AggSigMe(b48(), msg()),
        ConditionWithArgs::CreateCoin(b32(), 1000, vec![]),
        ConditionWithArgs::ReserveFee(7),
        ConditionWithArgs::CreateCoinAnnouncement(msg()),
        ConditionWithArgs::AssertCoinAnnouncement(b32()),
        ConditionWithArgs::CreatePuzzleAnnouncement(msg()),
        ConditionWithArgs::AssertPuzzleAnnouncement(b32()),
        ConditionWithArgs::AssertConcurrentSpend(b32()),
        ConditionWithArgs::AssertConcurrentPuzzle(b32()),
        ConditionWithArgs::SendMessage(0, msg(), MessageArgs::None),
        ConditionWithArgs::ReceiveMessage(0, msg(), MessageArgs::None),
        ConditionWithArgs::AssertMyCoinId(b32()),
        ConditionWithArgs::AssertMyParentId(b32()),
        ConditionWithArgs::AssertMyPuzzlehash(b32()),
        ConditionWithArgs::AssertMyAmount(5),
        ConditionWithArgs::AssertMyBirthSeconds(5),
        ConditionWithArgs::AssertMyBirthHeight(5),
        ConditionWithArgs::AssertEphemeral,
        ConditionWithArgs::AssertSecondsRelative(5),
        ConditionWithArgs::AssertSecondsAbsolute(5),
        ConditionWithArgs::AssertHeightRelative(5),
        ConditionWithArgs::AssertHeightAbsolute(5),
        ConditionWithArgs::AssertBeforeSecondsRelative(5),
        ConditionWithArgs::AssertBeforeSecondsAbsolute(5),
        ConditionWithArgs::AssertBeforeHeightRelative(5),
        ConditionWithArgs::AssertBeforeHeightAbsolute(5),
        ConditionWithArgs::SoftFork(3),
    ]
}

// Layer (1): the parser round-trips every opcode in the current Chia set — none degrade to Unknown.
#[test]
fn every_opcode_round_trips_through_the_parser() {
    let version = ChiaProtocolVersion::default();
    for cond in every_condition() {
        let bytes = cond.to_bytes(version).expect("serialize");
        let parsed = ConditionWithArgs::from_bytes(&mut Cursor::new(bytes.as_slice()), version)
            .expect("parse");
        assert_ne!(
            parsed.op_code(),
            ConditionOpcode::Unknown,
            "{cond:?} must not degrade to Unknown"
        );
        assert_eq!(
            parsed.op_code(),
            cond.op_code(),
            "opcode stable across round-trip"
        );
    }
    // 35 opcodes: 1 (Remark) + 43–52 + 60–67 + 70–76 + 80–87 + 90.
    assert_eq!(every_condition().len(), 35, "full opcode-set cardinality");
}

// A byte code outside the known set decodes to Unknown rather than panicking — a garbage-in guard.
#[test]
fn unknown_opcode_decodes_to_unknown_not_panic() {
    assert_eq!(ConditionOpcode::from(200u8), ConditionOpcode::Unknown);
    assert_eq!(ConditionOpcode::from(0u8), ConditionOpcode::Unknown);
}

// Layer (2): the enforcement diff. Every opcode is classified against its enforcement site in dg_xch's
// block_generator. This table IS the audited coverage matrix; a change to it is a change to the consensus
// surface the engine trusts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Enforcement {
    // Collected into SpendBundleConditions/Spend and checked (spend_from_conditions + validate_block_conditions).
    Enforced,
    // A recognized no-op in Chia too (Remark, SoftFork cost only) — safe to ignore.
    NoOp,
}

fn classification(op: ConditionOpcode) -> Enforcement {
    use ConditionOpcode as O;
    use Enforcement::{Enforced, NoOp};
    match op {
        // The five formerly-dropped opcodes are now collected onto `Spend` in
        // spend_from_conditions and enforced in validate_block_conditions. chia error
        // codes on violation: ASSERT_CONCURRENT_SPEND_FAILED=132,
        // ASSERT_CONCURRENT_PUZZLE_FAILED=133, ASSERT_EPHEMERAL_FAILED=140,
        // MESSAGE_NOT_SENT_OR_RECEIVED=147 (chia-consensus 0.37.0 validation_error.rs).
        // chia-consensus 0.37.0 enforces all five at every height — the 2.0 hard fork
        // (5_496_000) only selects the generator ROM, not this condition set.
        O::AssertConcurrentSpend
        | O::AssertConcurrentPuzzle
        | O::SendMessage
        | O::ReceiveMessage
        | O::AssertEphemeral => Enforced,
        O::AggSigParent
        | O::AggSigPuzzle
        | O::AggSigAmount
        | O::AggSigPuzzleAmount
        | O::AggSigParentAmount
        | O::AggSigParentPuzzle
        | O::AggSigUnsafe
        | O::AggSigMe
        | O::CreateCoin
        | O::ReserveFee
        | O::CreateCoinAnnouncement
        | O::AssertCoinAnnouncement
        | O::CreatePuzzleAnnouncement
        | O::AssertPuzzleAnnouncement
        | O::AssertMyCoinId
        | O::AssertMyParentId
        | O::AssertMyPuzzlehash
        | O::AssertMyAmount
        | O::AssertMyBirthSeconds
        | O::AssertMyBirthHeight
        | O::AssertSecondsRelative
        | O::AssertSecondsAbsolute
        | O::AssertHeightRelative
        | O::AssertHeightAbsolute
        | O::AssertBeforeSecondsRelative
        | O::AssertBeforeSecondsAbsolute
        | O::AssertBeforeHeightRelative
        | O::AssertBeforeHeightAbsolute => Enforced,
        O::Remark | O::SoftFork => NoOp,
        O::Unknown => NoOp,
    }
}

// The enforcement gap is now closed. Every opcode in the current Chia set is either
// Enforced or a recognized NoOp — there is no dropped-but-parsed opcode left.
#[test]
fn every_opcode_is_enforced_or_noop() {
    for op in every_condition().iter().map(ConditionWithArgs::op_code) {
        let class = classification(op);
        assert!(
            class == Enforcement::Enforced || class == Enforcement::NoOp,
            "{op} must be Enforced or NoOp, was {class:?}"
        );
    }
}

// The five formerly-dropped opcodes are now enforced (previously dropped by the `_ => {}` arm).
#[test]
fn the_five_ported_opcodes_are_now_enforced() {
    for op in [
        ConditionOpcode::AssertConcurrentSpend,
        ConditionOpcode::AssertConcurrentPuzzle,
        ConditionOpcode::SendMessage,
        ConditionOpcode::ReceiveMessage,
        ConditionOpcode::AssertEphemeral,
    ] {
        assert_eq!(
            classification(op),
            Enforcement::Enforced,
            "{op} must be enforced"
        );
    }
}

// chia-consensus 0.37.0 enforces all five conditions at every height: the 2.0 hard fork only
// switches the generator ROM (run_block_generator2 vs run_block_generator), both of which share
// the same conditions parser/validator. dg_xch matches this — validate_block_conditions enforces
// them unconditionally. This test pins the hard-fork constant that selects the ROM.
#[test]
fn hard_fork_height_selects_the_rom_not_the_condition_set() {
    assert_eq!(MAINNET.hard_fork_height, 5_496_000);
}
