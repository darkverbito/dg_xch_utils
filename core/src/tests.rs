#[cfg(test)]
mod tests {
    use dg_parser_macro::parse_program_hex;
    use crate::clvm::program::Program;
    use crate::clvm::utils::INFINITE_COST;

    const TEST_PROGRAM: Program = parse_program_hex!("ff02ffff01ff02ff02ffff04ff02ffff04ff05ff80808080ffff04ffff01ff12ffff0118ffff12ffff0117ffff12ffff0116ff05808080ff018080");

    #[test]
    fn test_minimal_program() {
        let results = TEST_PROGRAM
            .run(INFINITE_COST, 0, &Program::to(vec![11]))
            .unwrap();
        println!(
            "Constant Results: Cost({}) Value({})",
            results.0,
            results.1.as_int().unwrap()
        );
        assert_eq!(Program::to(133584), results.1);
    }
}