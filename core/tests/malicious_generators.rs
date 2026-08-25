// Port of chia `chia/_tests/core/mempool/test_mempool.py::TestMaliciousGenerators` (@ a95a2c6d,
// 2.7.1). These are adversarial generator programs: a small CLVM source that, at RUN time,
// synthesizes a huge integer (via `concat`/`substr` ladders) or a very large number of conditions,
// probing the validator's cost/dedup bounds. chia builds each program with `binutils.assemble` and
// runs it through `get_name_puzzle_conditions` in CONSENSUS mode (`mempool_mode=False`) at a
// soft-fork height; we assemble the byte-identical CLVM source, wrap it in the same simple-generator
// envelope, and drive it through `execute_block_generator_result` (parse + aggregate) followed by
// `validate_block_conditions` / `validate_spend_context` (the consensus condition checkers) — the
// same split the existing boundary suite in `block_generator.rs` uses.
//
// chia parametrizes over every fork height via the `softfork_height` fixture; we pin the CURRENT-era
// regime (post soft-fork 9, the height the live node actually runs) — the DoS bounds and condition
// errors under test do not change across forks. chia's `benchmark_runner.assert_runtime` ceilings are
// reproduced as plain wall-clock upper bounds — but as HW-INDEPENDENT safety valves (generous
// multiples of chia's reference-HW seconds), catching a superlinear blowup, not this builder's raw
// speed vs chia's CI.
//
// DIVERGENCE-51 (FIXED): every vector below now runs. Before the fix the high-`num` vectors CRASHED
// this node — `execute_block_generator_result` `export`ed the WHOLE puzzle output before bounding it,
// so an adversarial generator that emits a huge integer (`concat`/`substr` ladder, ~268 MB per arg)
// `num` times deep-copied that shared atom once per condition (a single 280,000-arg vector OOM-killed
// a 24 GiB builder), and one emitting a 600,000-deep CREATE_COIN list built an owned `SExp` whose
// `Drop` recursed and overflowed the native stack. The fix (`ClvmRuntime::run_in_arena` +
// `parse_and_apply_spend_from_arena` in `core/src/consensus/block_generator.rs`) walks the run arena
// iteratively, classifies every integer argument with a `sanitize_uint` port WITHOUT copying it,
// charges condition cost incrementally and bails at `MAX_BLOCK_COST_CLVM`, and fails at the first
// duplicate CREATE_COIN ("we'll just end up looking at two of them, and fail at the first duplicate"
// — test_mempool.py:2867-2869) — mirroring chia_rs `conditions.rs::parse_conditions` /
// `sanitize_int.rs::sanitize_uint` (chia-consensus 0.42.1). The `timed` ceilings below are the
// HW-independent safety valves that catch a superlinear regression.

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

// chia SINGLE_ARG_INT_LADDER_COND (test_mempool.py:2663).
const SINGLE_ARG_INT_LADDER_COND: &str = "(a (q 2 4 (c 2 (c (a 6 (c 2 (c (q . {filler}) (c 5 ())))) (c 11 ())))) (c (q (a (i 11 (q 4 (c (q . {opcode}) (c (concat 5 11) ())) (a 4 (c 2 (c 5 (c (- 11 (q . 1)) ()))))) ()) 1) 2 (i 11 (q 2 6 (c 2 (c (concat 5 5) (c (- 11 (q . 1)) ())))) (q . 5)) 1) (q 24 {num})))";

// chia SINGLE_ARG_INT_COND (test_mempool.py:2625).
const SINGLE_ARG_INT_COND: &str = "(a (q 2 4 (c 2 (c (c (q . {opcode}) (c (concat (a 6 (c 2 (c (q . {filler}) (c 5 ())))) (q . {val})) ())) (c 11 ())))) (c (q (a (i 11 (q 4 5 (a 4 (c 2 (c 5 (c (- 11 (q . 1)) ()))))) ()) 1) 2 (i 11 (q 2 6 (c 2 (c (concat 5 5) (c (- 11 (q . 1)) ())))) (q . 5)) 1) (q 28 {num})))";

// chia SINGLE_ARG_INT_SUBSTR_COND (test_mempool.py:2640).
const SINGLE_ARG_INT_SUBSTR_COND: &str = "(a (q 2 4 (c 2 (c (concat (a 6 (c 2 (c (q . {filler}) (c 5 ())))) (q . {val})) (c 11 ())))) (c (q (a (i 11 (q 4 (c (q . {opcode}) (c 5 ())) (a 4 (c 2 (c (substr 5 (q . 1)) (c (- 11 (q . 1)) ()))))) ()) 1) 2 (i 11 (q 2 6 (c 2 (c (concat 5 5) (c (- 11 (q . 1)) ())))) (q . 5)) 1) (q 28 {num})))";

// chia SINGLE_ARG_INT_SUBSTR_TAIL_COND (test_mempool.py:2652).
const SINGLE_ARG_INT_SUBSTR_TAIL_COND: &str = "(a (q 2 4 (c 2 (c (concat (a 6 (c 2 (c (q . {filler}) (c 5 ())))) (q . {val})) (c 11 ())))) (c (q (a (i 11 (q 4 (c (q . {opcode}) (c 5 ())) (a 4 (c 2 (c (substr 5 () (- (strlen 5) (q . 1))) (c (- 11 (q . 1)) ()))))) ()) 1) 2 (i 11 (q 2 6 (c 2 (c (concat 5 5) (c (- 11 (q . 1)) ())))) (q . 5)) 1) (q 25 {num})))";

// chia CREATE_ANNOUNCE_COND (test_mempool.py:2677).
const CREATE_ANNOUNCE_COND: &str = "(a (q 2 4 (c 2 (c (c (q . {opcode}) (c (a 6 (c 2 (c 5 ()))) ())) (c 11 ())))) (c (q (a (i 11 (q 4 5 (a 4 (c 2 (c 5 (c (- 11 (q . 1)) ()))))) ()) 1) 23 (q . 97) 5) (q 8184 {num})))";

// chia CREATE_COIN (test_mempool.py:2686): emits `num` identical CREATE_COIN conditions.
const CREATE_COIN: &str = "(a (q 2 2 (c 2 (c (q 51 \"abababababababababababababababab\" 1) (c 5 ())))) (c (q 2 (i 11 (q 4 5 (a 2 (c 2 (c 5 (c (- 11 (q . 1)) ()))))) ()) 1) (q {num})))";

// chia CREATE_UNIQUE_COINS (test_mempool.py:2702): emits `num` CREATE_COIN, each a distinct amount.
const CREATE_UNIQUE_COINS: &str = "(a (q 2 6 (c 2 (c (q 51 \"abababababababababababababababab\") (c 5 ())))) (c (q (a (i 5 (q 4 9 (a 4 (c 2 (c 13 (c 11 ()))))) (q 4 11 ())) 1) 2 (i 11 (q 4 (a 4 (c 2 (c 5 (c 11 ())))) (a 6 (c 2 (c 5 (c (- 11 (q . 1)) ()))))) ()) 1) (q {num})))";

// The chia envelope (test_mempool.py:2244, quote=False): a simple generator returning ONE spend
// tuple (parent, puzzle_reveal, amount, solution) where the puzzle_reveal is the malicious program
// (evaluated with the empty solution `(() (q . ()))`). We assemble the byte-identical source, exactly
// as chia's `binutils.assemble(prg)`.
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

// A generous multiple of chia's reference-HW seconds. The `timed` ceilings are chia's
// `benchmark_runner.assert_runtime` values (reference HW, optimized); the pre-fix DoS blew those to
// OOM / stack-overflow / minutes, so a HW- and BUILD-MODE-independent tripwire only has to separate
// "seconds" from "OOM / minutes", not measure this builder's speed. This slack keeps an unoptimized
// `cargo test` build — which runs the 280k/600k-condition CLVM programs several times slower than
// release — comfortably green while still catching a superlinear regression.
const DOS_CEILING_SLACK: u32 = 10;

// Run the generator under a wall-clock ceiling (chia's benchmark_runner.assert_runtime analog),
// widened by `DOS_CEILING_SLACK`.
fn timed<T>(reference: Duration, f: impl FnOnce() -> T) -> T {
    let ceiling = reference * DOS_CEILING_SLACK;
    let start = Instant::now();
    let out = f();
    let elapsed = start.elapsed();
    assert!(
        elapsed <= ceiling,
        "malicious generator exceeded {ceiling:?} (took {elapsed:?}, chia reference {reference:?}) — DoS bound regressed"
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

// The four time/height-lock opcodes chia ladders over.
const LADDER_OPCODES: &[ConditionOpcode] = &[
    ConditionOpcode::AssertHeightAbsolute,
    ConditionOpcode::AssertHeightRelative,
    ConditionOpcode::AssertSecondsAbsolute,
    ConditionOpcode::AssertSecondsRelative,
];

// chia error_for_condition (test_mempool.py:2707).
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
// asserted value exceeds the bound, so chia's `error_for_condition` is expected.
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

// ---- test_duplicate_large_integer_ladder (test_mempool.py:2736) ------------------------------
// DIVERGENCE-51 (sanitization sub-point, RESOLVED): num=28 does NOT OOM (only 28 conditions), but it
// probes condition-arg byte-length sanitization. Each asserted value is ~16 MB of leading zeros then
// a small counter. Pre-fix this node stripped the leading zeros to the small counter (height 28) and
// SATISFIED the lock — an under-rejection. chia's `sanitize_int.rs::sanitize_uint` (0.42.1) rejects a
// leading-zero-padded value outright (`buf.len() > 1 && buf[0] == 0 && (buf[1] & 0x80) == 0` → the
// condition's error code), so chia fails it with ASSERT_HEIGHT_ABSOLUTE_FAILED. `sanitize_uint_from_
// arena` now classifies the pad as `LeadingZero` and saturates the height to `u32::MAX`, so
// `validate_block_conditions` raises the SAME code — the expectation asserted below.
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

// ---- test_duplicate_large_integer (test_mempool.py:2755) -------------------------------------
// DIVERGENCE-51 (FIXED): num=280,000 huge-integer args OOM-killed the node pre-fix (the shared
// ~268 MB atom was deep-copied once per condition); now each is sanitize_uint-classified without a copy.
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

// ---- test_duplicate_large_integer_substr (test_mempool.py:2774) ------------------------------
// DIVERGENCE-51 (FIXED): same OOM class as `duplicate_large_integer`, substr variant (num=280,000);
// arena new_substr is a zero-copy view, so the 280,000 whittled atoms cost O(1) each to classify.
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

// ---- test_duplicate_large_integer_substr_tail (test_mempool.py:2793) -------------------------
// DIVERGENCE-51 (FIXED): num=280, each arg a large integer whittled from the tail by substr; same
// materialize-first class pre-fix, now streamed and bounded.
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

// ---- test_duplicate_large_integer_negative (test_mempool.py:2814) ----------------------------
// A negative filler (0xff) makes every asserted value negative → a no-op, so the block is ACCEPTED
// with exactly one spend (chia: error None, len(spends) == 1).
// DIVERGENCE-51 (FIXED): num=280,000 huge negative-integer args OOM-killed the node pre-fix before the
// no-op collapse; sanitize_uint now reads only the sign byte (NegativeOverflow → Skip).
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

// ---- test_duplicate_reserve_fee (test_mempool.py:2826) ---------------------------------------
// DIVERGENCE-51 (FIXED): num=280,000 huge reserve-fee args OOM-killed the node pre-fix; parse_amount(8)
// now fails the first oversized fee outright.
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
    // chia: RESERVE_FEE_CONDITION_FAILED. A reserve fee this size cannot be paid.
    assert_eq!(result, Err(ChiaError::ReserveFeeConditionFailed));
}

// ---- test_duplicate_reserve_fee_negative (test_mempool.py:2835) ------------------------------
// DIVERGENCE-51 (FIXED): num=200,000 huge negative reserve-fee args OOM-killed the node pre-fix.
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
    // chia: a negative RESERVE_FEE fails unconditionally (conds is None).
    assert_eq!(result, Err(ChiaError::ReserveFeeConditionFailed));
}

// ---- test_duplicate_coin_announces (test_mempool.py:2850) ------------------------------------
// 1024 announcements per spend: chia's 1024 cap is a MEMPOOL rule, so CONSENSUS validation ACCEPTS
// this (error None, one spend). Confirms our deliberate no-block-cap parity (block_generator.rs
// validate_block_conditions: "NO announcement-count cap here").
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

// ---- test_create_coin_duplicates (test_mempool.py:2865) --------------------------------------
// DIVERGENCE-51 (FIXED): num=600,000 identical CREATE_COINs built a 600k-deep condition list whose
// owned-SExp Drop overflowed the native stack pre-fix; the streaming parser now fails at the first
// duplicate (coin #2), never materializing the list.
#[test]
fn create_coin_duplicates() {
    let src = CREATE_COIN.replace("{num}", "600000");
    let req = build_generator(&src, 123);
    let result = timed(Duration::from_millis(1500), || {
        execute_block_generator_result(&req)
    });
    // chia: DUPLICATE_OUTPUT, failing at the first duplicate (conds None).
    assert_eq!(result, Err(ChiaError::DuplicateOutput));
}

// ---- test_many_create_coin (test_mempool.py:2878) --------------------------------------------
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
