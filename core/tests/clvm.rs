use dg_xch_core::clvm::runtime::ClvmRuntime;
use dg_xch_core::clvm::utils::MEMPOOL_MODE;

#[test]
fn softfork_canonical_ints_rejects_noncanonical_cost() {
    use dg_xch_core::clvm::sexp::{AtomBuf, SExp};
    use dg_xch_core::clvm::utils::CANONICAL_INTS;

    // Build `(36 (1 . <cost>))` — softfork (op 36) applied to a quoted (op 1) cost atom.
    let build = |cost_bytes: Vec<u8>| -> SExp<'static> {
        let op36 = SExp::Atom(AtomBuf::new(vec![36]));
        let quote = SExp::Atom(AtomBuf::new(vec![1]));
        let cost = SExp::Atom(AtomBuf::new(cost_bytes));
        let quoted = quote.cons(cost); // (1 . cost)
        let args = quoted.cons(SExp::default()); // ((1 . cost))
        op36.cons(args) // (36 (1 . cost))
    };

    // Canonical cost 0x05: accepted with and without the flag.
    let canonical = build(vec![5]);
    assert!(
        ClvmRuntime::new(u64::MAX, 0)
            .run(&canonical, &SExp::default())
            .is_ok()
    );
    assert!(
        ClvmRuntime::new(u64::MAX, CANONICAL_INTS)
            .run(&canonical, &SExp::default())
            .is_ok()
    );

    // Non-canonical cost 0x00 0x05 (redundant leading zero): accepted pre-SF9 (flag clear),
    // rejected at/above SF9 (flag set).
    let noncanonical = build(vec![0, 5]);
    assert!(
        ClvmRuntime::new(u64::MAX, 0)
            .run(&noncanonical, &SExp::default())
            .is_ok(),
        "non-canonical softfork cost must be accepted below soft_fork9_height"
    );
    assert!(
        ClvmRuntime::new(u64::MAX, CANONICAL_INTS)
            .run(&noncanonical, &SExp::default())
            .is_err(),
        "non-canonical softfork cost must be rejected at/above soft_fork9_height"
    );
}

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

#[test]
fn multiply_running_product_limb_cost_matches_clvmr() {
    use dg_xch_core::clvm::program::{Program, SerializedProgram};
    use dg_xch_core::clvm::utils::INFINITE_COST;
    for (hex, want) in [
        // (* (q . 0x7fffff) (q . 2) (q . 3))
        ("ff12ffff01837fffffffff0102ffff010380", 2011u64),
        // (* (q . 0x7fffff) (q . 2) (q . 3) (q . 256))
        ("ff12ffff01837fffffffff0102ffff0103ffff0182010080", 2962u64),
    ] {
        let serial = SerializedProgram::from_hex(hex).unwrap();
        let prog = serial.to_program().unwrap();
        let (cost, _) = prog.run(INFINITE_COST, 0, &Program::to(0)).unwrap();
        assert_eq!(cost, want, "multiply cost diverges from clvmr for {hex}");
    }
}

#[test]
#[cfg(feature = "bls")]
fn bls_ops_cost_and_value_match_clvmr() {
    use dg_xch_core::clvm::program::{Program, SerializedProgram};
    use dg_xch_core::clvm::utils::INFINITE_COST;
    use dg_xch_core::consensus::block_generator::BlockGeneratorFlags;
    use dg_xch_core::consensus::constants::MAINNET;

    const G1_GEN: &str = "97f1d3a73197d7942695638c4fa9ac0fc3688c4f9774b905a14e3a3f171bac586c55e83ff97a1aeffb3af00adb22c6bb";
    const G2_GEN: &str = "93e02b6052719f607dacd3a088274f65596bd0d09920b61ab5da61bbdc7f5049334cf11213945d57e5ac7d055d042b7e024aa2b2f08f0a91260805272dc51051c6e47ad4fa403b02b4510b647ae3d1770bac0326a805bbefd48056c8c121bdb8";
    const NEG_G2_GEN: &str = "b3e02b6052719f607dacd3a088274f65596bd0d09920b61ab5da61bbdc7f5049334cf11213945d57e5ac7d055d042b7e024aa2b2f08f0a91260805272dc51051c6e47ad4fa403b02b4510b647ae3d1770bac0326a805bbefd48056c8c121bdb8";
    // Fixed-seed AUG-scheme vector: sk = SecretKey::from_seed([7; 32]), sig = sign(sk, "abc").
    const PK: &str = "a010d140e7c43146b5bb59695e6c444abbb62e964a535d0034351a90d1192bff0130de95f9bbc58af254c4dab4e65d3a";
    const SIG: &str = "b6263afd8aa87b1c4ad4f3da9657ed804a82a1483b6b500e6b4771e315d5f0aa6b188373b8e5b11f74184e15c0f266040bb84b6a4077e0399c8870685456eee33d89863325b07dcdb89000f609133c20fe4c8c041cf3e2242bb692cb560da59f";
    let inf_g2 = format!("c0{}", "00".repeat(95));

    // The mainnet flag regime the wedged block ran under (post SF8/SF9, pre hard fork 2).
    let flags = BlockGeneratorFlags::for_height(&MAINNET, 9_179_161).clvm_flags;

    let cases: Vec<(String, &str, u64, String)> = vec![
        (
            "g1_negate".into(),
            "51",
            1417,
            format!("b7{}", &G1_GEN[2..]),
        ),
        (
            "g2_negate".into(),
            "55",
            2185,
            NEG_G2_GEN.to_string(),
        ),
        (
            "g1_multiply".into(),
            "50",
            706_031,
            "a572cbea904d67468808c8eb50a9450c9721db309128012543902d0ac358a62ae28f75bb8f1c7c42c39a8c5529bf0f4e".into(),
        ),
        (
            "g2_multiply".into(),
            "54",
            2_101_006,
            "aa4edef9c1ed7f729f520e47730a124fd70662a904ba1074728114d1031e1572c6c886f6b57ec72a6178288c47c335771638533957d540a9d2370f17cc7ed5863bc0b995b8825e0ee1ea1e1e4d00dbae81f14b0bf3611b78c952aacab827a053".into(),
        ),
        (
            "g1_subtract".into(),
            "49",
            2_789_575,
            format!("c0{}", "00".repeat(47)),
        ),
        (
            "g2_add".into(),
            "52",
            3_981_001,
            "aa4edef9c1ed7f729f520e47730a124fd70662a904ba1074728114d1031e1572c6c886f6b57ec72a6178288c47c335771638533957d540a9d2370f17cc7ed5863bc0b995b8825e0ee1ea1e1e4d00dbae81f14b0bf3611b78c952aacab827a053".into(),
        ),
    ];

    let quote_g1 = |p: &str| format!("ffff01b0{p}");
    let quote_g2 = |p: &str| format!("ffff01c060{p}");
    let run = |hex: &str| -> Result<(u64, Vec<u8>), String> {
        let serial = SerializedProgram::from_hex(hex).unwrap();
        let prog = serial.to_program().unwrap();
        match prog.run(INFINITE_COST, flags, &Program::to(0)) {
            Ok((cost, output)) => Ok((cost, output.as_vec().expect("atom output"))),
            Err(e) => Err(format!("{e:?}")),
        }
    };

    // (g1_negate (q . G1)) — and the same one-arg shape for g2_negate.
    let one_arg: Vec<(usize, String)> = vec![(0, quote_g1(G1_GEN)), (1, quote_g2(G2_GEN))];
    for (case_idx, (name, op, want_cost, want_out)) in cases.iter().enumerate() {
        let args = match case_idx {
            0 => one_arg[0].1.clone(),
            1 => one_arg[1].1.clone(),
            // (op (q . point) (q . 2))
            2 => format!("{}ffff0102", quote_g1(G1_GEN)),
            3 => format!("{}ffff0102", quote_g2(G2_GEN)),
            // (op (q . point) (q . point))
            4 => format!("{}{}", quote_g1(G1_GEN), quote_g1(G1_GEN)),
            5 => format!("{}{}", quote_g2(G2_GEN), quote_g2(G2_GEN)),
            _ => unreachable!(),
        };
        let opcode = op.parse::<u8>().unwrap();
        let hex = format!("ff{opcode:02x}{args}80");
        let (cost, out) = run(&hex).unwrap_or_else(|e| panic!("{name} failed: {e}"));
        assert_eq!(cost, *want_cost, "{name} cost diverges from clvmr");
        assert_eq!(
            hex::encode(out),
            *want_out,
            "{name} value diverges from clvmr"
        );
    }

    // (g1_map (q . "abc")) / (g2_map (q . "abc")) — default DST.
    let (cost, out) = run("ff38ffff018361626380").unwrap();
    assert_eq!(cost, 195_685, "map_to_g1 cost diverges from clvmr");
    assert_eq!(
        hex::encode(out),
        "a4b925a7f78b97ad6a8203e9b1e319f0fcde5bea79e58fac5ec79a2867d11bd97ded3fed5e346bc0afd8e23f0069055d"
    );
    let (cost, out) = run("ff39ffff018361626380").unwrap();
    assert_eq!(cost, 816_165, "map_to_g2 cost diverges from clvmr");
    assert_eq!(
        hex::encode(out),
        "8c57634a695c6d4933239fcdefcd5d92e85c59a07b3721cf1a865981a1ba9e439839d4ee0fa6195e0fa0381bfd667ce10f57e6a4a5fa46df6cf2319b6e4396364173868d519cbab87ea0b32eb9bf9d76612f13254bb0d904ede697820c34782d"
    );

    // (bls_pairing_identity (q . G1) (q . -G2) (q . G1) (q . G2)) — e(P,-Q)·e(P,Q) = 1: nil.
    let ok_hex = format!(
        "ff3a{}{}{}{}80",
        quote_g1(G1_GEN),
        quote_g2(NEG_G2_GEN),
        quote_g1(G1_GEN),
        quote_g2(G2_GEN)
    );
    let (cost, out) = run(&ok_hex).unwrap();
    assert_eq!(cost, 5_400_081, "pairing_identity cost diverges from clvmr");
    assert!(out.is_empty(), "pairing_identity must return nil");
    let fail_hex = format!("ff3a{}{}80", quote_g1(G1_GEN), quote_g2(G2_GEN));
    assert!(run(&fail_hex).is_err(), "non-identity pairing must raise");

    // (bls_verify (q . sig) (q . pk) (q . "abc")) — AUG-scheme verify: nil on success.
    let verify_hex =
        format!("ff3b{}{}ffff0183616263 80", quote_g2(SIG), quote_g1(PK)).replace(' ', "");
    let (cost, out) = run(&verify_hex).unwrap();
    assert_eq!(cost, 4_200_245, "bls_verify cost diverges from clvmr");
    assert!(out.is_empty(), "bls_verify must return nil");
    // Empty pair set: verifies iff the signature is the G2 identity.
    let empty_hex = format!("ff3bffff01c060{inf_g2}80");
    let (cost, out) = run(&empty_hex).unwrap();
    assert_eq!(cost, 3_000_021, "bls_verify empty cost diverges from clvmr");
    assert!(out.is_empty());
    let bad_hex =
        format!("ff3b{}{}ffff0183616264 80", quote_g2(SIG), quote_g1(PK)).replace(' ', "");
    assert!(run(&bad_hex).is_err(), "bad bls_verify must raise");
}
