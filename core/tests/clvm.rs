use dg_xch_core::clvm::runtime::ClvmRuntime;
use dg_xch_core::clvm::utils::MEMPOOL_MODE;

#[test]
fn test_mod() {
    use dg_xch_core::clvm::compile::Compiler;
    use std::borrow::Cow;
    const EXAMPLE_CLSP: &str = "
    (mod (num)
        (* num 25)
    )";
    let compiler = Compiler::new(Cow::Borrowed(EXAMPLE_CLSP.as_bytes()), 0, 0, &[]);
    let prog = compiler.compile().unwrap();
    assert_eq!("(* 2 (q . 25))", format!("{prog}"))
}

#[test]
fn test_defun() {
    use dg_xch_core::clvm::compile::Compiler;
    use std::borrow::Cow;
    const EXAMPLE_CLSP: &str = "
    (mod (num)
        (defconstant NUL_NUM 2)
        (defun square (number)
            ;; Returns the number squared.
            (* number number)
        )
        (square num)
    )";
    let compiler = Compiler::new(Cow::Borrowed(EXAMPLE_CLSP.as_bytes()), 0, 0, &[]);
    let prog = compiler.compile().unwrap();
    assert_eq!(
        "(a (q 2 6 (c 2 (c 5 ()))) (c (q 2 18 5 5) 1))",
        format!("{prog}")
    )
}

#[test]
fn test_nested_defun() {
    use dg_xch_core::clvm::compile::Compiler;
    use std::borrow::Cow;
    const EXAMPLE_CLSP: &str = "
    (mod (num)
        (defconstant NUL_NUM 2)
        (defun square (number)
            ;; Returns the number squared.
            (* number number)
        )
        (defun double (number)
            (* NUL_NUM number)
        )
        (square (double num))
    )";
    let compiler = Compiler::new(Cow::Borrowed(EXAMPLE_CLSP.as_bytes()), 0, 0, &[]);
    let prog = compiler.compile().unwrap();
    assert_eq!(
        "(a (q 2 14 (c 2 (c (a 10 (c 2 (c 5 ()))) ()))) (c (q 2 (* 4 5) 18 5 5) 1))",
        format!("{prog}")
    )
}

#[test]
fn test_defun_inline() {
    use dg_xch_core::clvm::compile::Compiler;
    use std::borrow::Cow;
    const EXAMPLE_CLSP: &str = "
    (mod (num)
        (defun-inline double (number)
            ;; Returns twice the number.
            (* number 2)
        )
        (double num)
    )";
    let compiler = Compiler::new(Cow::Borrowed(EXAMPLE_CLSP.as_bytes()), 0, 0, &[]);
    let prog = compiler.compile().unwrap();
    assert_eq!("(* 2 (q . 2))", format!("{prog}"))
}

#[test]
fn test_multi_constant() {
    use dg_xch_core::clvm::assemble::assemble_text;
    use dg_xch_core::clvm::compile::Compiler;
    use dg_xch_core::clvm::program::Program;
    use dg_xch_core::clvm::sexp::SExp;
    use dg_xch_core::clvm::utils::INFINITE_COST;
    use std::borrow::Cow;
    const EXAMPLE_CLSP: &str = "
    (mod (num)
        (defconstant NUL_NUM 22)
        (defconstant NUL_NUM2 23)
        (defconstant NUL_NUM3 24)
        (defun mul (number)
            (* NUL_NUM3 (* NUL_NUM2 (* NUL_NUM number)))
        )
        (mul num)
    )";
    let compiler = Compiler::new(Cow::Borrowed(EXAMPLE_CLSP.as_bytes()), 0, 0, &[]);
    let prog = compiler.compile().unwrap();
    let chia_prog =
        assemble_text("(a (q 2 14 (c 2 (c 5 ()))) (c (q (ash . 23) 24 18 10 (* 12 (* 8 5))) 1))")
            .unwrap();
    let results = prog
        .run(INFINITE_COST, 0, &Program::to(&[SExp::from(11)]))
        .unwrap();
    println!(
        "DG Results: Cost({}) Value({})",
        results.0,
        results.1.as_int().unwrap()
    );
    let results = chia_prog
        .run(INFINITE_COST, 0, &Program::to(&[SExp::from(11)]))
        .unwrap();
    println!(
        "Chia Results: Cost({}) Value({})",
        results.0,
        results.1.as_int().unwrap()
    );
}

#[test]
fn test_2_constants() {
    use dg_xch_core::clvm::compile::Compiler;
    use std::borrow::Cow;
    const EXAMPLE_CLSP: &str = "
    (mod (num)
      (defconstant NUL_NUM 22)
      (defconstant NUL_NUM2 23)
      (defun mul (number)
          (* NUL_NUM2 (* NUL_NUM number))
      )
      (mul num)
    )";
    let compiler = Compiler::new(Cow::Borrowed(EXAMPLE_CLSP.as_bytes()), 0, 0, &[]);
    let prog = compiler.compile().unwrap();
    assert_eq!(
        "(a (q 2 14 (c 2 (c 5 ()))) (c (q 22 23 18 10 (* 4 5)) 1))",
        format!("{}", prog)
    );
}

#[test]
fn test_constant_inline() {
    use dg_xch_core::clvm::compile::{Compiler, INLINE_CONSTS};
    use dg_xch_core::clvm::program::Program;
    use dg_xch_core::clvm::sexp::SExp;
    use dg_xch_core::clvm::utils::INFINITE_COST;
    use std::borrow::Cow;
    const EXAMPLE_CLSP: &str = "
    (mod (num)
        (defconstant NUL_NUM 25)
        (defun mul (number)
            (* NUL_NUM number)
        )
        (mul num)
    )";
    let compiler = Compiler::new(
        Cow::Borrowed(EXAMPLE_CLSP.as_bytes()),
        INLINE_CONSTS,
        0,
        &[],
    );
    let prog = compiler.compile().unwrap();
    let results = prog
        .run(INFINITE_COST, 0, &Program::to(&[SExp::from(11)]))
        .unwrap();
    assert_eq!(Program::to(275), results.1)
}

#[test]
fn test_re_assembly() {
    use dg_xch_core::clvm::assemble::assemble_text;
    use dg_xch_core::clvm::compile::{Compiler, INLINE_CONSTS};
    use dg_xch_core::clvm::program::Program;
    use dg_xch_core::clvm::sexp::SExp;
    use dg_xch_core::clvm::utils::INFINITE_COST;
    use std::borrow::Cow;
    const EXAMPLE_CLSP: &str = "
    (mod (num)
        (defconstant NUL_NUM 22)
        (defconstant NUL_NUM2 23)
        (defconstant NUL_NUM3 24)
        (defun mul (number)
            (* NUL_NUM3 (* NUL_NUM2 (* NUL_NUM number)))
        )
        (mul num)
    )";
    println!("Compiling Program: {EXAMPLE_CLSP}");
    let inline_compiler = Compiler::new(
        Cow::Borrowed(EXAMPLE_CLSP.as_bytes()),
        INLINE_CONSTS,
        0,
        &[],
    );
    let prog = inline_compiler.compile().unwrap();
    let inlined_str = format!("{prog}");
    println!("Inlined Constants  CLVM: {inlined_str}");
    let serial = assemble_text(&inlined_str).unwrap();
    assert_eq!(prog, serial);
    let results = serial
        .run(INFINITE_COST, 0, &Program::to(&[SExp::from(11)]))
        .unwrap();
    println!(
        "Inlined Constants Results: Cost({}) Value({})",
        results.0,
        results.1.as_int().unwrap()
    );
    assert_eq!(Program::to(133584), results.1);
    let compiler = Compiler::new(Cow::Borrowed(EXAMPLE_CLSP.as_bytes()), 0, 0, &[]);
    let prog = compiler.compile().unwrap();
    let inlined_str = format!("{prog}");
    println!("Argument Constants CLVM: {inlined_str}");
    let serial = assemble_text(&inlined_str).unwrap();
    assert_eq!(prog, serial);
    let results = serial
        .run(INFINITE_COST, 0, &Program::to(&[SExp::from(11)]))
        .unwrap();
    println!(
        "Argument Constants Results: Cost({}) Value({})",
        results.0,
        results.1.as_int().unwrap()
    );
    assert_eq!(Program::to(133584), results.1);
}

#[test]
fn test_runtime_add() {
    use dg_xch_core::clvm::compile::Compiler;
    use dg_xch_core::clvm::program::Program;
    use dg_xch_core::clvm::sexp::SExp;
    use std::borrow::Cow;
    const EXAMPLE_CLSP: &str = "
    (mod (num num2)
        (+ num num2)
    )";
    let compiler = Compiler::new(Cow::Borrowed(EXAMPLE_CLSP.as_bytes()), 0, 0, &[]);
    let prog = compiler.compile().unwrap();
    assert_eq!("(+ 2 5)", format!("{prog}"));
    let args = Program::to(&[SExp::from(10), SExp::from(13)]);
    let mut runtime = ClvmRuntime::new(u64::MAX, MEMPOOL_MODE);
    let (_, output) = runtime.run(prog.sexp(), args.sexp()).unwrap();
    assert_eq!("23", format!("{output}"))
}

#[test]
fn test_runtime_sub() {
    use dg_xch_core::clvm::compile::Compiler;
    use dg_xch_core::clvm::program::Program;
    use dg_xch_core::clvm::sexp::SExp;
    use std::borrow::Cow;
    const EXAMPLE_CLSP: &str = "
    (mod (num num2)
        (- num num2)
    )";
    let compiler = Compiler::new(Cow::Borrowed(EXAMPLE_CLSP.as_bytes()), 0, 0, &[]);
    let prog = compiler.compile().unwrap();
    assert_eq!("(- 2 5)", format!("{prog}"));
    let args = Program::to(&[SExp::from(10), SExp::from(13)]);
    let mut runtime = ClvmRuntime::new(u64::MAX, MEMPOOL_MODE);
    let (_, output) = runtime.run(prog.sexp(), args.sexp()).unwrap();
    assert_eq!("-3", format!("{output}"))
}

#[test]
fn test_runtime_mul() {
    use dg_xch_core::clvm::compile::Compiler;
    use dg_xch_core::clvm::program::Program;
    use dg_xch_core::clvm::sexp::SExp;
    use std::borrow::Cow;
    const EXAMPLE_CLSP: &str = "
    (mod (num num2)
        (* num num2)
    )";
    let compiler = Compiler::new(Cow::Borrowed(EXAMPLE_CLSP.as_bytes()), 0, 0, &[]);
    let prog = compiler.compile().unwrap();
    assert_eq!("(* 2 5)", format!("{prog}"));
    let args = Program::to(&[SExp::from(10), SExp::from(13)]);
    let mut runtime = ClvmRuntime::new(u64::MAX, MEMPOOL_MODE);
    let (_, output) = runtime.run(prog.sexp(), args.sexp()).unwrap();
    assert_eq!("130", format!("{output}"))
}

#[test]
fn test_runtime_div() {
    use dg_xch_core::clvm::compile::Compiler;
    use dg_xch_core::clvm::program::Program;
    use dg_xch_core::clvm::sexp::SExp;
    use std::borrow::Cow;
    const EXAMPLE_CLSP: &str = "
    (mod (num num2)
        (/ num num2)
    )";
    let compiler = Compiler::new(Cow::Borrowed(EXAMPLE_CLSP.as_bytes()), 0, 0, &[]);
    let prog = compiler.compile().unwrap();
    assert_eq!("(/ 2 5)", format!("{prog}"));
    let args = Program::to(&[SExp::from(260), SExp::from(13)]);
    let mut runtime = ClvmRuntime::new(u64::MAX, MEMPOOL_MODE);
    let (_, output) = runtime.run(prog.sexp(), args.sexp()).unwrap();
    assert_eq!("20", format!("{output}"))
}
