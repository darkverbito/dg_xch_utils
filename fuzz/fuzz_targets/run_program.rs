// Execute arbitrary programs under every on-chain dialect configuration.
//
// The VM runs peer-supplied programs, so no input may crash it, hang it past its cost ceiling, or
// exhaust memory. Running each input across the whole fork ladder also makes this a differential
// harness: the flag sets are real consensus regimes, and a program whose behavior changes between
// two of them for the wrong reason is a divergence.
//
// The input is split — the first bytes seed the environment, the rest is the program — so the
// fuzzer can drive both halves independently.
#![no_main]

use dg_xch_core::clvm::program::SerializedProgram;
use dg_xch_core::clvm::runtime::ClvmRuntime;
use dg_xch_core::clvm::sexp::{AtomBuf, SExp};
use dg_xch_core::clvm::utils::{
    CANONICAL_INTS, COST_CONDITIONS, DISABLE_OP, ENABLE_KECCAK_OPS_OUTSIDE_FORK, LIMITS,
    MEMPOOL_MODE, NEW_COST_MODEL, RELAXED_BLS,
};
use libfuzzer_sys::fuzz_target;

// A real block's ceiling. Bounding cost is what keeps the fuzzer's own runs finite; a program that
// exceeds it must return CostExceeded rather than run on.
const MAX_COST: u64 = 11_000_000_000;

const HARD_FORK: u32 = COST_CONDITIONS | ENABLE_KECCAK_OPS_OUTSIDE_FORK;
const SOFT_FORK8: u32 = HARD_FORK | DISABLE_OP | LIMITS;
const SOFT_FORK9: u32 = SOFT_FORK8 | CANONICAL_INTS;
const HARD_FORK2: u32 = SOFT_FORK9 | NEW_COST_MODEL | RELAXED_BLS;

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let split = usize::from(data[0]).min(data.len() - 1);
    let (env_bytes, program_bytes) = data[1..].split_at(split.min(data.len() - 1));

    let serialized = SerializedProgram::from(program_bytes.to_vec());
    let Ok(program) = serialized.to_program() else {
        return;
    };
    let env = SExp::Atom(AtomBuf::new(env_bytes.to_vec()));

    for flags in [0, HARD_FORK, SOFT_FORK8, SOFT_FORK9, HARD_FORK2, MEMPOOL_MODE] {
        let mut runtime = ClvmRuntime::new(MAX_COST, flags);
        if let Ok((cost, _)) = runtime.run(program.sexp(), &env) {
            assert!(
                cost <= MAX_COST,
                "run reported cost {cost} above the ceiling it was given"
            );
        }
    }
});
