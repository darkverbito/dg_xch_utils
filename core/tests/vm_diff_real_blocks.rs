use dg_xch_core::clvm::program::Program;
use dg_xch_core::clvm::program::SerializedProgram;
use dg_xch_core::clvm::runtime::ClvmRuntime;
use dg_xch_core::clvm::sexp::SExp;

// VM diff: run the extracted spend-4 puzzle + solution (block 5,494,140) through OUR
// runtime and print every output condition's opcode + raw argument atoms. A reference VM's
// output for the identical inputs is the oracle; the first differing condition names the
// divergent op.
//   VM_DIFF_PUZZLE=<puzzle.bin> VM_DIFF_SOLUTION=<solution.bin> \
//   cargo test --release -p dg_xch_core --test vm_diff_real_blocks -- --ignored --nocapture
#[test]
#[ignore = "requires extracted puzzle/solution files"]
fn dump_conditions_for_extracted_spend() {
    let puzzle_bytes =
        std::fs::read(std::env::var("VM_DIFF_PUZZLE").expect("VM_DIFF_PUZZLE")).unwrap();
    let solution_bytes =
        std::fs::read(std::env::var("VM_DIFF_SOLUTION").expect("VM_DIFF_SOLUTION")).unwrap();
    let puzzle_sp = SerializedProgram::from_bytes(&puzzle_bytes);
    let solution_sp = SerializedProgram::from_bytes(&solution_bytes);
    let puzzle = Program::from_serial(&puzzle_sp).expect("puzzle parses");
    let solution = Program::from_serial(&solution_sp).expect("solution parses");
    let mut runtime = ClvmRuntime::new(u64::MAX, 0);
    let (cost, output) = runtime.run(puzzle.sexp(), solution.sexp()).expect("runs");
    eprintln!("cost {cost}");
    let mut cond = &output;
    let mut ci = 0usize;
    while let SExp::Pair(p) = cond {
        let (c, rest) = (p.first(), p.rest());
        // c = (opcode arg...) — print opcode + each atom arg in hex; nested lists print as <list>.
        let mut parts = Vec::new();
        let mut n = c;
        while let SExp::Pair(q) = n {
            match q.first() {
                SExp::Atom(a) => parts.push(hex::encode(a.as_ref())),
                SExp::Pair(_) => parts.push("<list>".to_string()),
            }
            n = q.rest();
        }
        eprintln!("ours-cond[{ci}] {}", parts.join(" "));
        cond = rest;
        ci += 1;
    }
}
