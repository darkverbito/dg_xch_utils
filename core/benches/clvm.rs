use criterion::{Criterion, criterion_group, criterion_main};
use dg_parser_macro::parse_program_hex;
use dg_xch_core::clvm::program::Program;
use dg_xch_core::clvm::runtime::ClvmRuntime;
use dg_xch_core::clvm::sexp::SExp;
use dg_xch_core::clvm::utils::{MEMPOOL_MODE, NO_UNKNOWN_OPS};
use std::hint::black_box;

parse_program_hex!(
    SIMPLE_MATH_TEST,
    "ff02ffff01ff02ff06ffff04ff02ffff04ff03ffff04ff8205bbffff04ff8202bbffff04ff82013bffff04ff8200bbffff04ff5bffff04ff2bffff04ff13ff8080808080808080808080ffff04ffff01ffff12ffff10ff05ff0b80ff0b80ff22ffff02ff04ffff04ff02ffff04ff8202ffffff04ff82017fff8080808080ffff02ff04ffff04ff02ffff04ff2fffff04ff17ff8080808080ffff0bff8200bf80ffff0bff0b8080ff018080"
);

fn bench_simple_math(c: &mut Criterion) {
    let mut g = c.benchmark_group("bench_simple_math");
    g.bench_function(format!("Curried Simple Math"), |b| {
        let mut runtime = ClvmRuntime::new(u64::MAX, MEMPOOL_MODE | NO_UNKNOWN_OPS);
        let args = Program::to(&[SExp::from(&[
            SExp::from(2),
            SExp::from(3),
            SExp::from(b"Some Binary Data".to_vec()),
            SExp::from(&[
                SExp::from(4),
                SExp::from(5),
                SExp::from(b"Some Binary Data".to_vec()),
            ]),
        ])]);
        let args_hash = args.tree_hash();
        let args_program = vec![Program::to(args_hash)];
        let program = SIMPLE_MATH_TEST_PROGRAM.curry(&args_program);
        b.iter(|| {
            let (cost, out) = runtime.run(program.sexp(), args.sexp()).unwrap();
            black_box(cost);
            assert_eq!(out, SExp::from(1));
            black_box(out);
        })
    });
    g.finish();
}

criterion_group!(benches, bench_simple_math);
criterion_main!(benches);
