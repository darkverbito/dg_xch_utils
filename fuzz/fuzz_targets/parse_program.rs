// The decoder must survive any byte string a peer can send.
//
// Every serialized program the node parses is attacker-controlled in full, so the requirement is
// absolute: return Ok or Err, never panic, never abort, and never allocate on the strength of a
// length the input merely claims. A libfuzzer run explores the encodings nobody thought to write
// down — which is the half `core/tests/clvm_parser_robustness.rs` cannot reach, since its corpus is
// only ever what was enumerated by hand.
#![no_main]

use dg_xch_core::clvm::program::SerializedProgram;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let serialized = SerializedProgram::from(data.to_vec());
    // Both decoders: the plain one and the back-reference-aware one used for real generators.
    let _ = serialized.to_program();
    let _ = serialized.to_program_backrefs();
});
