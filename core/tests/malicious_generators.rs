// Adversarial generator programs: a small CLVM source that, at RUN time, synthesizes a huge
// integer (via `concat`/`substr` ladders) or a very large number of conditions, probing the
// validator's cost/dedup bounds. Each program is assembled from its CLVM source, wrapped in a
// simple-generator envelope, and driven through `execute_block_generator_result` (parse +
// aggregate) followed by `validate_block_conditions` / `validate_spend_context` (the consensus
// condition checkers) — the same split the boundary suite in `block_generator.rs` uses.
//
// The vectors are pinned at the CURRENT-era regime (post soft-fork 9, the height the live node
// runs); the DoS bounds and condition errors under test do not change across forks. The `timed`
// ceilings below are HW-INDEPENDENT safety valves catching a superlinear blowup, not a measure of
// this builder's raw speed.
//
// The bounds these vectors exercise: `execute_block_generator_result` must NOT export the whole
// puzzle output before bounding it, or a generator emitting a huge integer (~268 MB per arg) `num`
// times deep-copies that shared atom once per condition, and one emitting a 600,000-deep
// CREATE_COIN list builds an owned `SExp` whose `Drop` recurses and overflows the native stack.
// `ClvmRuntime::run_in_arena` + `parse_and_apply_spend_from_arena` walk the run arena iteratively,
// classify every integer argument without copying it, charge condition cost incrementally, bail at
// `MAX_BLOCK_COST_CLVM`, and fail at the FIRST duplicate CREATE_COIN.

use dg_xch_core::blockchain::condition_opcode::ConditionOpcode;
use dg_xch_core::blockchain::spend_bundle_conditions::SpendBundleConditions;
use dg_xch_core::clvm::assemble::assemble_text;
use dg_xch_core::consensus::block_generator::{
    BlockGeneratorFlags, BlockGeneratorInput, CoinSpendContext, ConditionValidationContext,
    execute_block_generator_result, validate_block_conditions,
};
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_core::errors::ChiaError;
use std::collections::HashMap;
use std::time::{Duration, Instant};

// Post soft-fork 9 (mainnet 8,655,000): the regime the live node runs today.
const POST_SF9: u32 = 8_655_001;

// A concat ladder that doubles a filler atom, emitting `num` single-integer conditions.
const SINGLE_ARG_INT_LADDER_COND: &str = "(a (q 2 4 (c 2 (c (a 6 (c 2 (c (q . {filler}) (c 5 ())))) (c 11 ())))) (c (q (a (i 11 (q 4 (c (q . {opcode}) (c (concat 5 11) ())) (a 4 (c 2 (c 5 (c (- 11 (q . 1)) ()))))) ()) 1) 2 (i 11 (q 2 6 (c 2 (c (concat 5 5) (c (- 11 (q . 1)) ())))) (q . 5)) 1) (q 24 {num})))";

// A concat ladder whose synthesized integer is the condition's single argument.
const SINGLE_ARG_INT_COND: &str = "(a (q 2 4 (c 2 (c (c (q . {opcode}) (c (concat (a 6 (c 2 (c (q . {filler}) (c 5 ())))) (q . {val})) ())) (c 11 ())))) (c (q (a (i 11 (q 4 5 (a 4 (c 2 (c 5 (c (- 11 (q . 1)) ()))))) ()) 1) 2 (i 11 (q 2 6 (c 2 (c (concat 5 5) (c (- 11 (q . 1)) ())))) (q . 5)) 1) (q 28 {num})))";

// As above, shrinking the synthesized integer by one leading byte per condition.
const SINGLE_ARG_INT_SUBSTR_COND: &str = "(a (q 2 4 (c 2 (c (concat (a 6 (c 2 (c (q . {filler}) (c 5 ())))) (q . {val})) (c 11 ())))) (c (q (a (i 11 (q 4 (c (q . {opcode}) (c 5 ())) (a 4 (c 2 (c (substr 5 (q . 1)) (c (- 11 (q . 1)) ()))))) ()) 1) 2 (i 11 (q 2 6 (c 2 (c (concat 5 5) (c (- 11 (q . 1)) ())))) (q . 5)) 1) (q 28 {num})))";

// As above, shrinking the synthesized integer by one trailing byte per condition.
const SINGLE_ARG_INT_SUBSTR_TAIL_COND: &str = "(a (q 2 4 (c 2 (c (concat (a 6 (c 2 (c (q . {filler}) (c 5 ())))) (q . {val})) (c 11 ())))) (c (q (a (i 11 (q 4 (c (q . {opcode}) (c 5 ())) (a 4 (c 2 (c (substr 5 () (- (strlen 5) (q . 1))) (c (- 11 (q . 1)) ()))))) ()) 1) 2 (i 11 (q 2 6 (c 2 (c (concat 5 5) (c (- 11 (q . 1)) ())))) (q . 5)) 1) (q 25 {num})))";

// Emits `num` announcement conditions carrying a synthesized message.
const CREATE_ANNOUNCE_COND: &str = "(a (q 2 4 (c 2 (c (c (q . {opcode}) (c (a 6 (c 2 (c 5 ()))) ())) (c 11 ())))) (c (q (a (i 11 (q 4 5 (a 4 (c 2 (c 5 (c (- 11 (q . 1)) ()))))) ()) 1) 23 (q . 97) 5) (q 8184 {num})))";

// Emits `num` identical CREATE_COIN conditions.
const CREATE_COIN: &str = "(a (q 2 2 (c 2 (c (q 51 \"abababababababababababababababab\" 1) (c 5 ())))) (c (q 2 (i 11 (q 4 5 (a 2 (c 2 (c 5 (c (- 11 (q . 1)) ()))))) ()) 1) (q {num})))";

// Emits `num` CREATE_COIN conditions, each with a distinct amount.
const CREATE_UNIQUE_COINS: &str = "(a (q 2 6 (c 2 (c (q 51 \"abababababababababababababababab\") (c 5 ())))) (c (q (a (i 5 (q 4 9 (a 4 (c 2 (c 13 (c 11 ()))))) (q 4 11 ())) 1) 2 (i 11 (q 4 (a 4 (c 2 (c 5 (c 11 ())))) (a 6 (c 2 (c 5 (c (- 11 (q . 1)) ()))))) ()) 1) (q {num})))";

// The generator envelope: a simple generator returning ONE spend tuple (parent, puzzle_reveal,
// amount, solution) where the puzzle_reveal is the malicious program, evaluated with the empty
// solution `(() (q . ()))`.
fn build_generator(program_src: &str, coin_amount: u64) -> BlockGeneratorInput {
    let prg = format!(
        "(q ((0x0101010101010101010101010101010101010101010101010101010101010101 {program_src} {coin_amount} (() (q . ())))))"
    );
    let generator = assemble_text(&prg)
        .expect("malicious generator assembles")
        .serialized()
        .expect("serializes");
    BlockGeneratorInput {
        transactions_generator: generator,
        generator_refs: Vec::new(),
        constants: MAINNET,
        height: POST_SF9,
        flags: BlockGeneratorFlags {
            simple_generator: true,
            ..Default::default()
        },
    }
}

// A generous multiple of the reference-hardware seconds each `timed` ceiling carries. An
// unbounded run of these vectors ends in OOM, stack overflow or minutes, so a HW- and
// build-mode-independent tripwire only has to separate "seconds" from "OOM / minutes". The slack
// keeps an unoptimized `cargo test` build green while still catching a superlinear regression.
const DOS_CEILING_SLACK: u32 = 10;

// Run the generator under a wall-clock ceiling, widened by `DOS_CEILING_SLACK`.
fn timed<T>(reference: Duration, f: impl FnOnce() -> T) -> T {
    let ceiling = reference * DOS_CEILING_SLACK;
    let start = Instant::now();
    let out = f();
    let elapsed = start.elapsed();
    assert!(
        elapsed <= ceiling,
        "malicious generator exceeded {ceiling:?} (took {elapsed:?}, reference {reference:?}) — DoS bound regressed"
    );
    out
}

fn fmt(template: &str, opcode: u8, num: u64, val: &str, filler: &str) -> String {
    template
        .replace("{opcode}", &opcode.to_string())
        .replace("{num}", &num.to_string())
        .replace("{val}", val)
        .replace("{filler}", filler)
}

// The four time/height-lock opcodes the ladders target.
const LADDER_OPCODES: &[ConditionOpcode] = &[
    ConditionOpcode::AssertHeightAbsolute,
    ConditionOpcode::AssertHeightRelative,
    ConditionOpcode::AssertSecondsAbsolute,
    ConditionOpcode::AssertSecondsRelative,
];

// The condition error each ladder opcode is expected to fail with.
fn expected_failure(opcode: ConditionOpcode) -> ChiaError {
    match opcode {
        ConditionOpcode::AssertHeightAbsolute => ChiaError::AssertHeightAbsoluteFailed,
        ConditionOpcode::AssertHeightRelative => ChiaError::AssertHeightRelativeFailed,
        ConditionOpcode::AssertSecondsAbsolute => ChiaError::AssertSecondsAbsoluteFailed,
        ConditionOpcode::AssertSecondsRelative => ChiaError::AssertSecondsRelativeFailed,
        ConditionOpcode::ReserveFee => ChiaError::ReserveFeeConditionFailed,
        _ => unreachable!("unexpected ladder opcode {opcode:?}"),
    }
}

// Validate the strict-collapsed condition against the block context. Absolute locks compare against
// the block height / previous-tx timestamp directly; relative locks compare against the spent coin's
// height / seconds, so a coin context is attached for the single spend. In every arm the huge
// asserted value exceeds the bound, so the opcode's own failure code is expected.
fn validate_ladder(
    opcode: ConditionOpcode,
    conds: &SpendBundleConditions,
) -> Result<(), ChiaError> {
    let mut ctx = ConditionValidationContext {
        block_height: POST_SF9,
        previous_transaction_block_timestamp: Some(1),
        coin_context: HashMap::new(),
    };
    if matches!(
        opcode,
        ConditionOpcode::AssertHeightRelative | ConditionOpcode::AssertSecondsRelative
    ) {
        ctx.coin_context.insert(
            conds.spends[0].coin_id,
            CoinSpendContext {
                birth_height: None,
                birth_seconds: None,
                spent_height: Some(0),
                spent_seconds: Some(0),
            },
        );
    }
    validate_block_conditions(conds, &ctx)
}

// ---- large-integer ladder ---------------------------------------------------------------------
// num=28 does not OOM (only 28 conditions); this probes condition-argument byte-length
// sanitization. Each asserted value is ~16 MB of leading zeros then a small counter. A
// leading-zero-padded value is NOT canonical and must be rejected outright rather than stripped
// down to the small counter, which would satisfy the lock. `sanitize_uint_from_arena` classifies
// the pad as `LeadingZero` and saturates the height to `u32::MAX`, so `validate_block_conditions`
// raises the lock's own failure code.
#[test]
fn duplicate_large_integer_ladder() {
    for &opcode in LADDER_OPCODES {
        let src = fmt(SINGLE_ARG_INT_LADDER_COND, opcode as u8, 28, "", "0x00");
        let req = build_generator(&src, 123);
        let conds = timed(Duration::from_secs(1), || {
            execute_block_generator_result(&req)
        })
        .expect("ladder program parses to conditions");
        assert_eq!(
            validate_ladder(opcode, &conds),
            Err(expected_failure(opcode)),
            "opcode {opcode:?}"
        );
    }
}

// ---- large-integer args -----------------------------------------------------------------------
// num=280,000 huge-integer args: deep-copying the shared ~268 MB atom once per condition is an OOM,
// so each must be sanitize_uint-classified without a copy.
#[test]
fn duplicate_large_integer() {
    for &opcode in LADDER_OPCODES {
        let src = fmt(SINGLE_ARG_INT_COND, opcode as u8, 280_000, "100", "0x00");
        let req = build_generator(&src, 123);
        let conds = timed(Duration::from_secs(3), || {
            execute_block_generator_result(&req)
        })
        .expect("large-integer program parses");
        assert_eq!(
            validate_ladder(opcode, &conds),
            Err(expected_failure(opcode))
        );
    }
}

// ---- large-integer args, substr variant --------------------------------------------------------
// Same OOM class as `duplicate_large_integer` (num=280,000); the arena's substr is a zero-copy
// view, so the 280,000 whittled atoms cost O(1) each to classify.
#[test]
fn duplicate_large_integer_substr() {
    for &opcode in LADDER_OPCODES {
        let src = fmt(
            SINGLE_ARG_INT_SUBSTR_COND,
            opcode as u8,
            280_000,
            "100",
            "0x00",
        );
        let req = build_generator(&src, 123);
        let conds = timed(Duration::from_secs(2), || {
            execute_block_generator_result(&req)
        })
        .expect("substr program parses");
        assert_eq!(
            validate_ladder(opcode, &conds),
            Err(expected_failure(opcode))
        );
    }
}

// ---- large-integer args, substr-from-tail variant ----------------------------------------------
// num=280, each arg a large integer whittled from the tail by substr; streamed and bounded.
#[test]
fn duplicate_large_integer_substr_tail() {
    for &opcode in LADDER_OPCODES {
        let src = fmt(
            SINGLE_ARG_INT_SUBSTR_TAIL_COND,
            opcode as u8,
            280,
            "0xffffffff",
            "0x00",
        );
        let req = build_generator(&src, 123);
        let conds = timed(Duration::from_secs(1), || {
            execute_block_generator_result(&req)
        })
        .expect("substr-tail program parses");
        assert_eq!(
            validate_ladder(opcode, &conds),
            Err(expected_failure(opcode))
        );
    }
}

// ---- large negative-integer args ---------------------------------------------------------------
// A negative filler (0xff) makes every asserted value negative, hence a no-op, so the block is
// ACCEPTED with exactly one spend. With num=280,000 the no-op collapse must not require
// materializing the args: sanitize_uint reads only the sign byte and skips the condition.
#[test]
fn duplicate_large_integer_negative() {
    for &opcode in LADDER_OPCODES {
        let src = fmt(SINGLE_ARG_INT_COND, opcode as u8, 280_000, "100", "0xff");
        let req = build_generator(&src, 123);
        let conds = timed(Duration::from_millis(2750), || {
            execute_block_generator_result(&req)
        })
        .expect("negative program parses");
        assert!(
            validate_ladder(opcode, &conds).is_ok(),
            "opcode {opcode:?}: a negative time/height lock is a no-op, not a failure"
        );
        assert_eq!(conds.spends.len(), 1);
    }
}

// ---- huge reserve fees --------------------------------------------------------------------------
// num=280,000 huge reserve-fee args: the 8-byte amount parse must fail the first oversized fee
// outright rather than materialize the rest.
#[test]
fn duplicate_reserve_fee() {
    let src = fmt(
        SINGLE_ARG_INT_COND,
        ConditionOpcode::ReserveFee as u8,
        280_000,
        "100",
        "0x00",
    );
    let req = build_generator(&src, 123);
    let result = timed(Duration::from_millis(1500), || {
        execute_block_generator_result(&req)
    });
    // A reserve fee this size cannot be paid.
    assert_eq!(result, Err(ChiaError::ReserveFeeConditionFailed));
}

// ---- huge negative reserve fees -----------------------------------------------------------------
// num=200,000 huge negative reserve-fee args.
#[test]
fn duplicate_reserve_fee_negative() {
    let src = fmt(
        SINGLE_ARG_INT_COND,
        ConditionOpcode::ReserveFee as u8,
        200_000,
        "100",
        "0xff",
    );
    let req = build_generator(&src, 123);
    let result = timed(Duration::from_millis(1500), || {
        execute_block_generator_result(&req)
    });
    // A negative RESERVE_FEE fails unconditionally.
    assert_eq!(result, Err(ChiaError::ReserveFeeConditionFailed));
}

// ---- many announcements per spend ---------------------------------------------------------------
// 1024 announcements in one spend: the 1024 cap is a MEMPOOL rule, so CONSENSUS validation ACCEPTS
// this (no error, one spend). See `validate_block_conditions`, which deliberately applies no
// announcement-count cap.
#[test]
fn duplicate_coin_announces() {
    for opcode in [
        ConditionOpcode::CreateCoinAnnouncement,
        ConditionOpcode::CreatePuzzleAnnouncement,
    ] {
        let src = fmt(CREATE_ANNOUNCE_COND, opcode as u8, 1024, "", "");
        let req = build_generator(&src, 123);
        let conds = timed(Duration::from_secs(14), || {
            execute_block_generator_result(&req)
        })
        .expect("1024 announcements are accepted in consensus mode");
        let ctx = ConditionValidationContext {
            block_height: POST_SF9,
            previous_transaction_block_timestamp: None,
            coin_context: HashMap::new(),
        };
        assert!(validate_block_conditions(&conds, &ctx).is_ok());
        assert_eq!(conds.spends.len(), 1);
    }
}

// ---- duplicate CREATE_COINs ---------------------------------------------------------------------
// num=600,000 identical CREATE_COINs: materializing the list would build a 600k-deep owned SExp
// whose Drop overflows the native stack, so the streaming parser fails at the first duplicate
// (coin #2) and never materializes it.
#[test]
fn create_coin_duplicates() {
    let src = CREATE_COIN.replace("{num}", "600000");
    let req = build_generator(&src, 123);
    let result = timed(Duration::from_millis(1500), || {
        execute_block_generator_result(&req)
    });
    // DUPLICATE_OUTPUT, failing at the first duplicate.
    assert_eq!(result, Err(ChiaError::DuplicateOutput));
}

// ---- many unique CREATE_COINs -------------------------------------------------------------------
#[test]
fn many_create_coin() {
    let src = CREATE_UNIQUE_COINS.replace("{num}", "6094");
    let req = build_generator(&src, 123_000_000);
    let conds = timed(Duration::from_millis(300), || {
        execute_block_generator_result(&req)
    })
    .expect("6094 distinct create-coins are accepted");
    assert_eq!(conds.spends.len(), 1);
    assert_eq!(conds.spends[0].create_coin.len(), 6094);
}
