// A program that parses must re-serialize to the exact bytes it came from.
//
// This is a consensus property, not a tidiness one: a program's identity is the hash of its
// serialization, so a decoder that accepts input and reconstructs it differently has silently
// changed which program is being run. No panic-freedom test would notice.
#![no_main]

use dg_xch_core::clvm::parser::sexp_to_bytes;
use dg_xch_core::clvm::program::SerializedProgram;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let serialized = SerializedProgram::from(data.to_vec());
    let Ok(program) = serialized.to_program() else {
        return;
    };
    let Ok(reserialized) = sexp_to_bytes(program.sexp()) else {
        // Failing to re-serialize something that parsed is itself a defect.
        panic!("a program that parsed failed to re-serialize");
    };
    assert_eq!(
        reserialized.as_ref(),
        data,
        "parse -> serialize changed the bytes, so the program's identity changed"
    );
});
