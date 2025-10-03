#[cfg(test)]
mod tests {
    use crate::clvm::program::{Program, SerializedProgram};
    use crate::clvm::sexp::SExp;
    use crate::clvm::utils::INFINITE_COST;
    use dg_parser_macro::parse_program_hex;

    const TEST_PROGRAM: Program<'static> = parse_program_hex!("ff02ffff01ff02ff02ffff04ff02ffff04ff05ff80808080ffff04ffff01ff12ffff0118ffff12ffff0117ffff12ffff0116ff05808080ff018080");
    #[test]
    fn test_minimal_const_program() {
        let test_program = TEST_PROGRAM;
        let results = test_program
            .run(INFINITE_COST, 0, &Program::to(&[SExp::from(11)]))
            .unwrap();
        println!(
            "Constant Results: Cost({}) Value({})",
            results.0,
            results.1.as_int().unwrap()
        );
        assert_eq!(Program::to(133584), results.1);
    }
    pub const TEST_PROGRAM_HEX: &str = "ff02ffff01ff02ff02ffff04ff02ffff04ff05ff80808080ffff04ffff01ff12ffff0118ffff12ffff0117ffff12ffff0116ff05808080ff018080";
    lazy_static::lazy_static! {
        pub static ref TEST_PROGRAM_STATIC: Program<'static> = Program::from_serial(
            &SerializedProgram::from_hex(TEST_PROGRAM_HEX).unwrap()
        ).unwrap();
    }
    #[test]
    fn test_minimal_static_program() {
        let results = TEST_PROGRAM_STATIC
            .run(INFINITE_COST, 0, &Program::to(&[SExp::from(11)]))
            .unwrap();
        println!(
            "Constant Results: Cost({}) Value({})",
            results.0,
            results.1.as_int().unwrap()
        );
        assert_eq!(Program::to(133584), results.1);
    }
    #[test]
    fn test_minimal_program_equality() {
        assert_eq!(*TEST_PROGRAM_STATIC, TEST_PROGRAM);
    }
}
