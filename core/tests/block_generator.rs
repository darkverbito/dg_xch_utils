use blst::min_pk::SecretKey;
use dg_xch_core::blockchain::coin::Coin;
use dg_xch_core::blockchain::condition_with_args::{ConditionWithArgs, Message};
use dg_xch_core::blockchain::foliage_transaction_block::FoliageTransactionBlock;
use dg_xch_core::blockchain::sized_bytes::{Bytes32, Bytes48};
use dg_xch_core::blockchain::spend_bundle::SpendBundle;
use dg_xch_core::blockchain::spend_bundle_conditions::SpendBundleConditions;
use dg_xch_core::blockchain::transactions_info::TransactionsInfo;
use dg_xch_core::clvm::parser::{sexp_from_bytes, sexp_from_bytes_backrefs};
use dg_xch_core::clvm::program::{Program, SerializedProgram};
use dg_xch_core::clvm::sexp::SExp;
use dg_xch_core::consensus::block_generator::{
    BlockGeneratorFlags, BlockGeneratorInput, GeneratorReference, TransactionBlockValidationInput,
    additions_root, block_fee_amount, execute_block_generator, execute_block_generator_result,
    fee_summary, removals_root, transactions_generator_refs_root, transactions_generator_root,
    transactions_info_hash, validate_block_aggregate_signature, validate_block_conditions,
    validate_transaction_block,
};
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_core::errors::ChiaError;
use dg_xch_core::traits::SizedBytes;
use dg_xch_core::utils::hash_256;
use std::io::Cursor;

const HISTORICAL_BLOCK_834752: &str =
    include_str!("fixtures/chia_generator_tests/block-834752.txt");
const HISTORICAL_BLOCK_834752_COMPRESSED: &str =
    include_str!("fixtures/chia_generator_tests/block-834752-compressed.txt");
const HISTORICAL_BLOCK_4671894: &str =
    include_str!("fixtures/chia_generator_tests/block-4671894.txt");
const HISTORICAL_BLOCK_4671894_REF: &str =
    include_str!("fixtures/chia_generator_tests/block-4671894.env");

#[derive(Debug)]
struct ExpectedGeneratorOutput {
    generator: SerializedProgram,
    reserve_fee: u64,
    cost: u64,
    removal_amount: u128,
    addition_amount: u128,
    spends: Vec<ExpectedSpend>,
}

#[derive(Clone, Debug)]
struct ExpectedSpend {
    coin_id: Bytes32,
    puzzle_hash: Bytes32,
    height_relative: Option<u32>,
    create_coins: Vec<(Bytes32, u64)>,
    agg_sig_me: Vec<(Bytes48, Vec<u8>)>,
}

fn parse_bytes32(hex: &str) -> Bytes32 {
    Bytes32::parse(&hex::decode(hex).unwrap()).unwrap()
}

fn parse_chia_generator_fixture(fixture: &str) -> ExpectedGeneratorOutput {
    let mut lines = fixture.lines();
    let generator = SerializedProgram::from_hex(lines.next().unwrap()).unwrap();
    let mut expected = ExpectedGeneratorOutput {
        generator,
        reserve_fee: 0,
        cost: 0,
        removal_amount: 0,
        addition_amount: 0,
        spends: Vec::new(),
    };
    let mut current_spend = None::<ExpectedSpend>;
    for line in lines {
        if !line.starts_with(' ') && !line.starts_with("- coin id:") {
            if let Some(spend) = current_spend.take() {
                expected.spends.push(spend);
            }
        }
        if line.starts_with("RESERVE_FEE:") {
            expected.reserve_fee = line
                .strip_prefix("RESERVE_FEE:")
                .unwrap()
                .trim()
                .parse()
                .unwrap();
        } else if line.starts_with("- coin id:") {
            if let Some(spend) = current_spend.take() {
                expected.spends.push(spend);
            }
            let fields = line.split_whitespace().collect::<Vec<_>>();
            current_spend = Some(ExpectedSpend {
                coin_id: parse_bytes32(fields[3]),
                puzzle_hash: parse_bytes32(fields[5]),
                height_relative: None,
                create_coins: Vec::new(),
                agg_sig_me: Vec::new(),
            });
        } else if line.starts_with("  ASSERT_HEIGHT_RELATIVE") {
            current_spend.as_mut().unwrap().height_relative =
                Some(line.split_whitespace().last().unwrap().parse().unwrap());
        } else if line.starts_with("  CREATE_COIN:") {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            current_spend
                .as_mut()
                .unwrap()
                .create_coins
                .push((parse_bytes32(fields[2]), fields[4].parse().unwrap()));
        } else if line.starts_with("  AGG_SIG_ME") {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            current_spend.as_mut().unwrap().agg_sig_me.push((
                Bytes48::parse(&hex::decode(fields[2]).unwrap()).unwrap(),
                hex::decode(fields[4]).unwrap(),
            ));
        } else if line.starts_with("cost:") {
            expected.cost = line.strip_prefix("cost:").unwrap().trim().parse().unwrap();
        } else if line.starts_with("removal_amount:") {
            expected.removal_amount = line
                .strip_prefix("removal_amount:")
                .unwrap()
                .trim()
                .parse()
                .unwrap();
        } else if line.starts_with("addition_amount:") {
            expected.addition_amount = line
                .strip_prefix("addition_amount:")
                .unwrap()
                .trim()
                .parse()
                .unwrap();
        }
    }
    if let Some(spend) = current_spend.take() {
        expected.spends.push(spend);
    }
    expected
}

fn historical_input(
    fixture: &ExpectedGeneratorOutput,
    height: u32,
    generator_refs: Vec<GeneratorReference>,
) -> BlockGeneratorInput {
    BlockGeneratorInput {
        transactions_generator: fixture.generator.clone(),
        generator_refs,
        constants: MAINNET,
        height,
        flags: BlockGeneratorFlags::default(),
    }
}

fn assert_matches_chia_fixture(conds: &SpendBundleConditions, expected: &ExpectedGeneratorOutput) {
    assert_eq!(conds.reserve_fee, expected.reserve_fee);
    assert!(
        conds.cost >= expected.cost,
        "legacy ROM cost {} must be at least Chia generator2 cost {}",
        conds.cost,
        expected.cost
    );
    assert_eq!(conds.removal_amount, expected.removal_amount);
    assert_eq!(conds.addition_amount, expected.addition_amount);
    assert_eq!(conds.spends.len(), expected.spends.len());
    let mut expected_spends = expected.spends.clone();
    expected_spends.sort_by_key(|spend| spend.coin_id.bytes());
    let mut actual_spends = conds
        .spends
        .iter()
        .map(|spend| {
            let mut create_coins = spend
                .create_coin
                .iter()
                .map(|coin| (coin.puzzle_hash, coin.amount))
                .collect::<Vec<_>>();
            create_coins.sort_by(|a, b| a.0.bytes().cmp(&b.0.bytes()).then(a.1.cmp(&b.1)));
            let mut agg_sig_me = spend
                .agg_sig_me
                .iter()
                .map(|(pk, msg)| {
                    (
                        Bytes48::parse(pk.as_slice()).unwrap(),
                        msg.as_slice().to_vec(),
                    )
                })
                .collect::<Vec<_>>();
            agg_sig_me.sort_by(|a, b| a.0.bytes().cmp(&b.0.bytes()).then(a.1.cmp(&b.1)));
            (
                spend.coin_id,
                spend.puzzle_hash,
                spend.height_relative,
                create_coins,
                agg_sig_me,
            )
        })
        .collect::<Vec<_>>();
    actual_spends.sort_by_key(|(coin_id, _, _, _, _)| coin_id.bytes());
    for (expected_spend, actual_spend) in expected_spends.iter().zip(actual_spends.iter()) {
        let (actual_coin_id, actual_puzzle_hash, height_relative, create_coins, agg_sig_me) =
            actual_spend;
        assert_eq!(actual_coin_id, &expected_spend.coin_id);
        assert_eq!(actual_puzzle_hash, &expected_spend.puzzle_hash);
        assert_eq!(height_relative, &expected_spend.height_relative);
        let mut expected_create_coins = expected_spend.create_coins.clone();
        expected_create_coins.sort_by(|a, b| a.0.bytes().cmp(&b.0.bytes()).then(a.1.cmp(&b.1)));
        assert_eq!(create_coins, &expected_create_coins);
        let mut expected_agg_sig_me = expected_spend.agg_sig_me.clone();
        expected_agg_sig_me.sort_by(|a, b| a.0.bytes().cmp(&b.0.bytes()).then(a.1.cmp(&b.1)));
        assert_eq!(agg_sig_me, &expected_agg_sig_me);
    }
}

fn quoted_generator(output: SExp<'static>) -> SerializedProgram {
    Program::to((1_u8, output)).serialized().unwrap()
}

fn puzzle_for_conditions(conditions: Vec<ConditionWithArgs>) -> Program<'static> {
    let condition_sexps = conditions
        .iter()
        .map(|condition| SExp::from(condition).to_owned())
        .collect::<Vec<_>>();
    Program::to((1_u8, SExp::from(condition_sexps)))
}

fn spend_output(parent: Bytes32, amount: u64, puzzle: &Program<'static>) -> SExp<'static> {
    SExp::from(vec![
        SExp::from(parent),
        puzzle.sexp().to_owned(),
        SExp::from(amount),
        SExp::default(),
    ])
}

fn input(generator: SerializedProgram) -> BlockGeneratorInput {
    BlockGeneratorInput {
        transactions_generator: generator,
        generator_refs: Vec::new(),
        constants: MAINNET,
        height: 10,
        flags: BlockGeneratorFlags {
            simple_generator: true,
            ..Default::default()
        },
    }
}

fn validation_case(
    generator: SerializedProgram,
    reward_claims: Vec<Coin>,
) -> (
    BlockGeneratorInput,
    TransactionsInfo,
    FoliageTransactionBlock,
    SpendBundleConditions,
) {
    let req = input(generator);
    let conds = execute_block_generator_result(&req).unwrap();
    let summary = fee_summary(&conds).unwrap();
    let tx_info = TransactionsInfo {
        generator_root: transactions_generator_root(
            &req.transactions_generator,
            &MAINNET,
            req.height,
        )
        .unwrap(),
        generator_refs_root: transactions_generator_refs_root(&[]).unwrap(),
        aggregated_signature: SpendBundle::empty().aggregated_signature,
        fees: block_fee_amount(&summary).unwrap(),
        cost: conds.cost,
        reward_claims_incorporated: reward_claims,
    };
    let foliage = FoliageTransactionBlock {
        prev_transaction_block_hash: Bytes32::new([10; 32]),
        timestamp: 1,
        filter_hash: Bytes32::new([11; 32]),
        additions_root: additions_root(&conds, &tx_info.reward_claims_incorporated).unwrap(),
        removals_root: removals_root(&conds),
        transactions_info_hash: transactions_info_hash(&tx_info).unwrap(),
    };
    (req, tx_info, foliage, conds)
}

#[test]
fn executes_block_generator_and_summarizes_conditions() {
    let parent = Bytes32::new([1; 32]);
    let child_puzzle = Bytes32::new([3; 32]);
    let puzzle = puzzle_for_conditions(vec![
        ConditionWithArgs::CreateCoin(child_puzzle, 90, vec![b"hint".to_vec()]),
        ConditionWithArgs::ReserveFee(7),
        ConditionWithArgs::AssertHeightAbsolute(9),
    ]);
    let output = SExp::from(vec![SExp::from(vec![spend_output(parent, 100, &puzzle)])]);

    let conds = execute_block_generator_result(&input(quoted_generator(output))).unwrap();

    assert_eq!(conds.spends.len(), 1);
    assert_eq!(conds.reserve_fee, 7);
    assert_eq!(conds.height_absolute, 9);
    assert_eq!(conds.removal_amount, 100);
    assert_eq!(conds.addition_amount, 90);
    assert_eq!(fee_summary(&conds).unwrap().reserve_fee, 7);
}

#[test]
fn malformed_reference_is_reported_without_storage_fetch() {
    let output = SExp::from(vec![SExp::from(Vec::<SExp>::new())]);
    let mut req = input(quoted_generator(output));
    req.generator_refs.push(GeneratorReference {
        height: 5,
        index: 0,
        generator: SerializedProgram::from_bytes(&[0xff]),
    });

    let result = execute_block_generator(&req);

    assert_eq!(
        result.error,
        Some(ChiaError::GeneratorRefHasNoGenerator as u16)
    );
    assert!(result.conds.is_none());
}

#[test]
fn bad_aggregate_signature_fails() {
    let sk = SecretKey::key_gen_v3(&[42; 32], &[]).unwrap();
    let pk = Bytes48::from(&sk.sk_to_pk());
    let parent = Bytes32::new([1; 32]);
    let puzzle = puzzle_for_conditions(vec![ConditionWithArgs::AggSigMe(
        pk,
        Message::new(b"message".to_vec()).unwrap(),
    )]);
    let output = SExp::from(vec![SExp::from(vec![spend_output(parent, 100, &puzzle)])]);
    let conds = execute_block_generator_result(&input(quoted_generator(output))).unwrap();
    let bad_signature = SpendBundle::empty().aggregated_signature;

    assert_eq!(
        validate_block_aggregate_signature(&conds, &bad_signature, &MAINNET),
        Err(ChiaError::BadAggregateSignature)
    );
}

#[test]
fn additions_and_removals_roots_change_with_generated_coins() {
    let parent = Bytes32::new([1; 32]);
    let child_puzzle = Bytes32::new([3; 32]);
    let puzzle = puzzle_for_conditions(vec![ConditionWithArgs::CreateCoin(
        child_puzzle,
        90,
        vec![],
    )]);
    let output = SExp::from(vec![SExp::from(vec![spend_output(parent, 100, &puzzle)])]);
    let conds = execute_block_generator_result(&input(quoted_generator(output))).unwrap();
    let reward = Coin {
        parent_coin_info: Bytes32::new([8; 32]),
        puzzle_hash: Bytes32::new([9; 32]),
        amount: 2,
    };

    assert_ne!(additions_root(&conds, &[]).unwrap(), Bytes32::default());
    assert_ne!(
        additions_root(&conds, &[]).unwrap(),
        additions_root(&conds, &[reward]).unwrap()
    );
    assert_eq!(removals_root(&conds), removals_root(&conds));
}

#[test]
fn generator_refs_root_hashes_concatenated_ref_heights() {
    assert_eq!(
        transactions_generator_refs_root(&[]).unwrap(),
        Bytes32::new([1; 32])
    );

    let mut raw_refs = Vec::new();
    raw_refs.extend_from_slice(&5_u32.to_be_bytes());
    raw_refs.extend_from_slice(&12_u32.to_be_bytes());
    assert_eq!(
        transactions_generator_refs_root(&[5, 12]).unwrap(),
        Bytes32::new(hash_256(raw_refs))
    );
}

#[test]
fn transaction_block_validation_checks_metadata_and_roots() {
    let parent = Bytes32::new([1; 32]);
    let child_puzzle = Bytes32::new([3; 32]);
    let puzzle = puzzle_for_conditions(vec![ConditionWithArgs::CreateCoin(
        child_puzzle,
        90,
        vec![],
    )]);
    let output = SExp::from(vec![SExp::from(vec![spend_output(parent, 100, &puzzle)])]);
    let reward = Coin {
        parent_coin_info: Bytes32::new([8; 32]),
        puzzle_hash: Bytes32::new([9; 32]),
        amount: 2,
    };
    let (req, tx_info, foliage, conds) = validation_case(quoted_generator(output), vec![reward]);

    let result = validate_transaction_block(&TransactionBlockValidationInput {
        generator_input: req,
        transactions_info: &tx_info,
        foliage_transaction_block: Some(&foliage),
        condition_context: None,
    })
    .unwrap();

    assert_eq!(result.conditions, conds);
    assert_eq!(result.additions_root, foliage.additions_root);
    assert_eq!(result.removals_root, foliage.removals_root);
    assert_eq!(result.fee_summary.removals_total, 100);
    assert_eq!(result.fee_summary.additions_total, 90);
}

#[test]
fn transaction_block_validation_rejects_bad_generator_root() {
    let output = SExp::from(vec![SExp::from(Vec::<SExp>::new())]);
    let (req, mut tx_info, foliage, _) = validation_case(quoted_generator(output), vec![]);
    tx_info.generator_root = Bytes32::new([99; 32]);

    assert_eq!(
        validate_transaction_block(&TransactionBlockValidationInput {
            generator_input: req,
            transactions_info: &tx_info,
            foliage_transaction_block: Some(&foliage),
            condition_context: None,
        }),
        Err(ChiaError::InvalidTransactionsGeneratorHash)
    );
}

#[test]
fn transaction_block_validation_rejects_bad_fee_cost_and_roots() {
    let parent = Bytes32::new([1; 32]);
    let puzzle = puzzle_for_conditions(vec![ConditionWithArgs::CreateCoin(
        Bytes32::new([3; 32]),
        90,
        vec![],
    )]);
    let output = SExp::from(vec![SExp::from(vec![spend_output(parent, 100, &puzzle)])]);
    let (req, mut tx_info, mut foliage, _) = validation_case(quoted_generator(output), vec![]);

    tx_info.fees += 1;
    assert_eq!(
        validate_transaction_block(&TransactionBlockValidationInput {
            generator_input: req.clone(),
            transactions_info: &tx_info,
            foliage_transaction_block: Some(&foliage),
            condition_context: None,
        }),
        Err(ChiaError::InvalidBlockFeeAmount)
    );

    tx_info.fees -= 1;
    tx_info.cost += 1;
    assert_eq!(
        validate_transaction_block(&TransactionBlockValidationInput {
            generator_input: req.clone(),
            transactions_info: &tx_info,
            foliage_transaction_block: Some(&foliage),
            condition_context: None,
        }),
        Err(ChiaError::InvalidBlockCost)
    );

    tx_info.cost -= 1;
    foliage.additions_root = Bytes32::new([77; 32]);
    assert_eq!(
        validate_transaction_block(&TransactionBlockValidationInput {
            generator_input: req,
            transactions_info: &tx_info,
            foliage_transaction_block: Some(&foliage),
            condition_context: None,
        }),
        Err(ChiaError::BadAdditionRoot)
    );
}

#[test]
fn future_generator_reference_is_invalid() {
    let output = SExp::from(vec![SExp::from(Vec::<SExp>::new())]);
    let mut req = input(quoted_generator(output.clone()));
    req.generator_refs.push(GeneratorReference {
        height: 10,
        index: 0,
        generator: quoted_generator(output),
    });

    assert_eq!(
        execute_block_generator_result(&req),
        Err(ChiaError::FutureGeneratorRefs)
    );
}

#[test]
fn cost_limit_failure_is_reported() {
    let mut req = input(quoted_generator(SExp::from(vec![SExp::from(
        Vec::<SExp>::new(),
    )])));
    req.constants.max_block_cost_clvm = 1;

    assert_eq!(
        execute_block_generator_result(&req),
        Err(ChiaError::BlockCostExceedsMax)
    );
}

#[test]
fn legacy_generator_mode_executes_bootstrap_rom() {
    let output = SExp::from(vec![SExp::from(Vec::<SExp>::new())]);
    let mut req = input(quoted_generator(output));
    req.flags.simple_generator = false;

    let conds = execute_block_generator_result(&req).unwrap();

    assert!(conds.spends.is_empty());
    assert!(conds.cost > 0);
}

#[test]
fn legacy_generator_mode_accepts_caller_supplied_reference_generators() {
    let output = SExp::from(vec![SExp::from(Vec::<SExp>::new())]);
    let mut req = input(quoted_generator(output.clone()));
    req.flags.simple_generator = false;
    req.generator_refs.push(GeneratorReference {
        height: 5,
        index: 0,
        generator: quoted_generator(output),
    });

    let conds = execute_block_generator_result(&req).unwrap();

    assert!(conds.spends.is_empty());
}

#[test]
fn simple_generator_mode_rejects_complex_generator_shape() {
    let complex = Program::to((2_u8, SExp::default())).serialized().unwrap();
    let req = input(complex);

    assert_eq!(
        execute_block_generator_result(&req),
        Err(ChiaError::ComplexGeneratorReceived)
    );
}

#[test]
fn height_flags_select_legacy_before_hardfork() {
    let before = BlockGeneratorFlags::for_height(&MAINNET, MAINNET.hard_fork_height - 1);
    let after = BlockGeneratorFlags::for_height(&MAINNET, MAINNET.hard_fork_height);

    assert!(!before.simple_generator);
    assert!(after.simple_generator);
}

#[test]
fn matching_coin_announcement_assertion_validates() {
    let announcing_parent = Bytes32::new([1; 32]);
    let asserting_parent = Bytes32::new([2; 32]);
    let message = b"ann";
    let announcing_puzzle = puzzle_for_conditions(vec![ConditionWithArgs::CreateCoinAnnouncement(
        Message::new(message.to_vec()).unwrap(),
    )]);
    let coin = Coin {
        parent_coin_info: announcing_parent,
        puzzle_hash: announcing_puzzle.tree_hash(),
        amount: 100,
    };
    let mut announcement_buf = Vec::new();
    announcement_buf.extend_from_slice(coin.name().as_ref());
    announcement_buf.extend_from_slice(message);
    let announcement_id = Bytes32::new(hash_256(announcement_buf));
    let asserting_puzzle = puzzle_for_conditions(vec![ConditionWithArgs::AssertCoinAnnouncement(
        announcement_id,
    )]);
    let output = SExp::from(vec![SExp::from(vec![
        spend_output(announcing_parent, 100, &announcing_puzzle),
        spend_output(asserting_parent, 100, &asserting_puzzle),
    ])]);
    let conds = execute_block_generator_result(&input(quoted_generator(output))).unwrap();

    assert!(validate_block_conditions(&conds, &Default::default()).is_ok());
}

#[test]
fn clvm_backreference_deserialization_matches_expanded_form() {
    let expanded = hex::decode("ff86666f6f626172ff86666f6f62617280").unwrap();
    let compressed = hex::decode("ff86666f6f626172fe01").unwrap();
    let expanded_sexp = sexp_from_bytes(&mut Cursor::new(expanded.as_slice())).unwrap();
    let compressed_sexp =
        sexp_from_bytes_backrefs(&mut Cursor::new(compressed.as_slice())).unwrap();

    assert_eq!(expanded_sexp.tree_hash(), compressed_sexp.tree_hash());
    assert_eq!(expanded_sexp, compressed_sexp);
}

#[test]
fn historical_block_834752_matches_chia_generator_fixture() {
    let expected = parse_chia_generator_fixture(HISTORICAL_BLOCK_834752);
    let conds =
        execute_block_generator_result(&historical_input(&expected, 834_752, vec![])).unwrap();

    assert_matches_chia_fixture(&conds, &expected);
}

#[test]
fn historical_compressed_block_834752_matches_chia_generator_fixture() {
    let expected = parse_chia_generator_fixture(HISTORICAL_BLOCK_834752_COMPRESSED);
    let conds =
        execute_block_generator_result(&historical_input(&expected, 834_752, vec![])).unwrap();

    assert_matches_chia_fixture(&conds, &expected);
}

#[test]
fn historical_generator_ref_block_4671894_matches_chia_generator_fixture() {
    let expected = parse_chia_generator_fixture(HISTORICAL_BLOCK_4671894);
    let refs = vec![GeneratorReference {
        height: 4_671_893,
        index: 0,
        generator: SerializedProgram::from_hex(HISTORICAL_BLOCK_4671894_REF).unwrap(),
    }];
    let conds =
        execute_block_generator_result(&historical_input(&expected, 4_671_894, refs)).unwrap();

    assert_matches_chia_fixture(&conds, &expected);
}
