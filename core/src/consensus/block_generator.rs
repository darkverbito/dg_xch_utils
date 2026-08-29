use crate::blockchain::coin::Coin;
use crate::blockchain::coin_spend::CoinSpend;
use crate::blockchain::condition_opcode::ConditionOpcode;
use crate::blockchain::condition_with_args::{ConditionWithArgs, Message, MessageArgs};
use crate::blockchain::foliage_transaction_block::FoliageTransactionBlock;
use crate::blockchain::npc_result::NPCResult;
use crate::blockchain::sized_bytes::{Bytes32, Bytes48, Bytes96};
use crate::blockchain::spend::{ELIGIBLE_FOR_DEDUP, ELIGIBLE_FOR_FF, NewCoin, Spend, SpendMessage};
use crate::blockchain::spend_bundle_conditions::SpendBundleConditions;
use crate::blockchain::transactions_info::TransactionsInfo;
use crate::blockchain::unsized_bytes::UnsizedBytes;
use crate::blockchain::utils::{pkm_pairs_for_conditions, verify_agg_sig_unsafe_message};
use crate::clvm::parser::{sexp_from_bytes, sexp_to_bytes};
use crate::clvm::program::{Program, SerializedProgram};
use crate::clvm::sexp::{AtomBuf, PairBuf, SExp};
use crate::clvm::tree_hash_cache::TreeHashCache;
use crate::clvm::utils::{
    CANONICAL_INTS, COST_CONDITIONS, DISABLE_OP, ENABLE_KECCAK_OPS_OUTSIDE_FORK, LIMITS,
    NEW_COST_MODEL, RELAXED_BLS,
};
use crate::consensus::constants::ConsensusConstants;
use crate::consensus::generator_puzzles::ROM_BOOTSTRAP_GENERATOR_HEX;
use crate::consensus::{AGG_SIG_COST, CREATE_COIN_COST};
use crate::constants::AUG_SCHEME_DST;
use crate::errors::{ChiaError, ClvmError};
use crate::traits::SizedBytes;
use crate::utils::hash_256;
use blst::BLST_ERROR;
use blst::min_pk::{PublicKey, Signature};
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use std::cmp::{max, min};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Clone, Debug)]
pub struct GeneratorReference {
    pub height: u32,
    pub index: u32,
    pub generator: SerializedProgram,
}

#[derive(Clone, Debug, Default)]
pub struct BlockGeneratorFlags {
    pub clvm_flags: u32,
    pub simple_generator: bool,
}

impl BlockGeneratorFlags {
    // CLVM flag ladder, keyed on the block's OWN height. The ladder is CUMULATIVE:
    // - hard fork 1 (mainnet 5,496,000): the simple generator plus post-fork condition
    //   accounting (`COST_CONDITIONS` switches announcement limits to per-condition
    //   costing) and keccak.
    // - soft fork 8 (mainnet 8,655,000): op_modpow disabled and division-family
    //   operand caps (DISABLE_OP | LIMITS).
    // - hard fork 2 (unscheduled): the bounded NEW_COST_MODEL; the dialect neuters
    //   DISABLE_OP/LIMITS once NEW_COST_MODEL is set.
    // - soft fork 9 (mainnet 8,655,000 — SAME height as soft fork 8): of the SF9 set only
    //   CANONICAL_INTS is a VM flag, so it is the ONLY member wired into `clvm_flags` here.
    //   SIMPLE_GENERATOR (generator-ref ban + canonical-serialization + simple quote shape)
    //   and LIMIT_SPENDS (6,000-spend cap) are enforced as BODY rules in
    //   `validate_transaction_block`, keyed on prev-tx height.
    #[must_use]
    pub fn for_height(constants: &ConsensusConstants, height: u32) -> Self {
        let mut clvm_flags = 0u32;
        if height >= constants.hard_fork_height {
            clvm_flags |= COST_CONDITIONS | ENABLE_KECCAK_OPS_OUTSIDE_FORK;
        }
        if height >= constants.soft_fork8_height {
            clvm_flags |= DISABLE_OP | LIMITS;
        }
        if height >= constants.soft_fork9_height {
            clvm_flags |= CANONICAL_INTS;
        }
        if height >= constants.hard_fork2_height {
            // hard-fork-2 additions: the bounded NEW_COST_MODEL and RELAXED_BLS (BLS negate
            // operators accept invalid points).
            clvm_flags |= NEW_COST_MODEL | RELAXED_BLS;
        }
        Self {
            clvm_flags,
            simple_generator: height >= constants.hard_fork_height,
        }
    }
}

#[derive(Clone, Debug)]
pub struct BlockGeneratorInput {
    pub transactions_generator: SerializedProgram,
    pub generator_refs: Vec<GeneratorReference>,
    pub constants: ConsensusConstants,
    pub height: u32,
    pub flags: BlockGeneratorFlags,
}

#[derive(Copy, Clone, Debug)]
pub struct CoinSpendContext {
    pub birth_height: Option<u32>,
    pub birth_seconds: Option<u64>,
    pub spent_height: Option<u32>,
    pub spent_seconds: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct ConditionValidationContext {
    pub block_height: u32,
    pub previous_transaction_block_timestamp: Option<u64>,
    pub coin_context: HashMap<Bytes32, CoinSpendContext>,
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct BlockFeeSummary {
    pub removals_total: u128,
    pub additions_total: u128,
    pub reserve_fee: u64,
}

#[derive(Clone, Debug)]
pub struct TransactionBlockValidationInput<'a> {
    pub generator_input: BlockGeneratorInput,
    pub transactions_info: &'a TransactionsInfo,
    pub foliage_transaction_block: Option<&'a FoliageTransactionBlock>,
    pub condition_context: Option<&'a ConditionValidationContext>,
    // The previous TRANSACTION block's height: the SF9 body rules key on it, UNLIKE the
    // CLVM flag ladder which keys on the block's own height.
    pub prev_transaction_block_height: u32,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TransactionBlockValidationResult {
    pub conditions: SpendBundleConditions,
    pub additions_root: Bytes32,
    pub removals_root: Bytes32,
    pub generator_root: Bytes32,
    pub generator_refs_root: Bytes32,
    pub fee_summary: BlockFeeSummary,
}

pub fn execute_block_generator(input: &BlockGeneratorInput) -> NPCResult {
    match execute_block_generator_result(input) {
        Ok(conds) => NPCResult {
            error: None,
            conds: Some(conds),
        },
        Err(error) => NPCResult {
            error: Some(chia_error_code(error)),
            conds: None,
        },
    }
}

// Blocks past soft fork 9 are limited to this many spends besides the CLVM cost ceiling.
pub const MAX_SPENDS_PER_BLOCK: usize = 6_000;

pub fn validate_transaction_block(
    input: &TransactionBlockValidationInput,
) -> Result<TransactionBlockValidationResult, ChiaError> {
    let sf9 =
        input.prev_transaction_block_height >= input.generator_input.constants.soft_fork9_height;
    // SF9 rule 1: generator back-reference lists are banned.
    if sf9 && !input.generator_input.generator_refs.is_empty() {
        return Err(ChiaError::TooManyGeneratorRefs);
    }
    // SF9 rule 2: the generator must use canonical CLVM serialization.
    if sf9
        && !crate::clvm::parser::is_canonical_serialization(
            input.generator_input.transactions_generator.as_ref(),
        )
    {
        return Err(ChiaError::ComplexGeneratorReceived);
    }
    let conds = execute_block_generator_result(&input.generator_input)?;
    // SF9 rule 3: at most 6,000 spends per block.
    if sf9 && conds.spends.len() > MAX_SPENDS_PER_BLOCK {
        return Err(ChiaError::TooManySpends);
    }
    let generator_root = transactions_generator_root(&input.generator_input.transactions_generator);
    if generator_root != input.transactions_info.generator_root {
        return Err(ChiaError::InvalidTransactionsGeneratorHash);
    }

    let generator_ref_heights = input
        .generator_input
        .generator_refs
        .iter()
        .map(|reference| reference.height)
        .collect::<Vec<_>>();
    let generator_refs_root = transactions_generator_refs_root(&generator_ref_heights)?;
    if generator_refs_root != input.transactions_info.generator_refs_root {
        return Err(ChiaError::InvalidTransactionsGeneratorHash);
    }

    validate_block_aggregate_signature(
        &conds,
        &input.transactions_info.aggregated_signature,
        &input.generator_input.constants,
    )?;

    if conds.cost != input.transactions_info.cost {
        return Err(ChiaError::InvalidBlockCost);
    }

    let fee_summary = fee_summary(&conds)?;
    let computed_fee = block_fee_amount(&fee_summary)?;
    if computed_fee != input.transactions_info.fees {
        return Err(ChiaError::InvalidBlockFeeAmount);
    }
    if computed_fee < fee_summary.reserve_fee {
        return Err(ChiaError::ReserveFeeConditionFailed);
    }

    let additions_root =
        additions_root(&conds, &input.transactions_info.reward_claims_incorporated)
            .map_err(|_| ChiaError::BadAdditionRoot)?;
    let removals_root = removals_root(&conds);
    if let Some(foliage) = input.foliage_transaction_block {
        if foliage.transactions_info_hash != transactions_info_hash(input.transactions_info)? {
            return Err(ChiaError::InvalidTransactionsInfoHash);
        }
        if foliage.additions_root != additions_root {
            return Err(ChiaError::BadAdditionRoot);
        }
        if foliage.removals_root != removals_root {
            return Err(ChiaError::BadRemovalRoot);
        }
    }

    if let Some(ctx) = input.condition_context {
        validate_block_conditions(&conds, ctx)?;
    }

    Ok(TransactionBlockValidationResult {
        conditions: conds,
        additions_root,
        removals_root,
        generator_root,
        generator_refs_root,
        fee_summary,
    })
}

/// The generator identity root: `sha256` of the generator's **raw serialized bytes**.
///
/// There is **no height gate and no tree hash**: the on-wire generator bytes (which may
/// carry CLVM back-references) are hashed verbatim, never the decompressed program's
/// `sha256tree`. `SerializedProgram::as_ref()` is those raw wire bytes.
pub fn transactions_generator_root(generator: &SerializedProgram) -> Bytes32 {
    Bytes32::new(hash_256(generator.as_ref()))
}

pub fn transactions_generator_refs_root(ref_heights: &[u32]) -> Result<Bytes32, ChiaError> {
    if ref_heights.is_empty() {
        return Ok(Bytes32::new([1; 32]));
    }
    let mut serialized = Vec::with_capacity(ref_heights.len() * 4);
    for height in ref_heights {
        serialized.extend(
            height
                .to_bytes(ChiaProtocolVersion::default())
                .map_err(|_| ChiaError::InvalidTransactionsGeneratorHash)?,
        );
    }
    Ok(Bytes32::new(hash_256(serialized)))
}

pub fn transactions_info_hash(transactions_info: &TransactionsInfo) -> Result<Bytes32, ChiaError> {
    let serialized = transactions_info
        .to_bytes(ChiaProtocolVersion::default())
        .map_err(|_| ChiaError::InvalidTransactionsInfoHash)?;
    Ok(Bytes32::new(hash_256(serialized)))
}

pub fn execute_block_generator_result(
    input: &BlockGeneratorInput,
) -> Result<SpendBundleConditions, ChiaError> {
    if input.transactions_generator.as_ref().len() > input.constants.max_generator_size as usize {
        return Err(ChiaError::InvalidTransactionsGeneratorHash);
    }
    if input.generator_refs.len() > input.constants.max_generator_ref_list_size as usize {
        return Err(ChiaError::TooManyGeneratorRefs);
    }
    for generator_ref in &input.generator_refs {
        if generator_ref.height >= input.height {
            return Err(ChiaError::FutureGeneratorRefs);
        }
        generator_ref
            .generator
            .to_program_backrefs()
            .map_err(|_| ChiaError::GeneratorRefHasNoGenerator)?;
    }
    if input.flags.simple_generator && !input.generator_refs.is_empty() {
        return Err(ChiaError::TooManyGeneratorRefs);
    }

    let generator = input
        .transactions_generator
        .to_program_backrefs()
        .map_err(|error| {
            log::warn!(
                "execute_block_generator: generator back-reference decode failed at height {}: {error:?}",
                input.height
            );
            ChiaError::InvalidBlockSolution
        })?;
    if input.flags.simple_generator {
        check_simple_generator(&input.transactions_generator, &generator)?;
    }
    let max_cost = input.constants.max_block_cost_clvm;
    let byte_cost = (input.transactions_generator.as_ref().len() as u64)
        .checked_mul(input.constants.cost_per_byte)
        .ok_or(ChiaError::InvalidCostResult)?;
    let cost_left = max_cost
        .checked_sub(byte_cost)
        .ok_or(ChiaError::BlockCostExceedsMax)?;
    let (cost, output) = if input.flags.simple_generator {
        generator
            .run(cost_left, input.flags.clvm_flags, &Program::default())
            .map_err(generator_run_error)?
    } else {
        // (ROM path: reveal hashes are computed inside the CLVM bootstrap, not here.)
        let rom_serial = SerializedProgram::from_hex(ROM_BOOTSTRAP_GENERATOR_HEX)
            .map_err(|_| ChiaError::GeneratorRuntimeError)?;
        let rom = rom_serial
            .to_program()
            .map_err(|_| ChiaError::GeneratorRuntimeError)?;
        let args = Program::to(SExp::from(vec![
            generator.sexp().to_owned(),
            SExp::from(vec![generator_ref_atoms(input)]),
        ]));
        rom.run(cost_left, input.flags.clvm_flags, &args)
            .map_err(generator_run_error)?
    };
    let cost = byte_cost
        .checked_add(cost)
        .ok_or(ChiaError::InvalidCostResult)?;
    if input.flags.simple_generator {
        // Dedup puzzle-reveal tree-hashing within the block. The generator is a
        // verified `(q . REST)`, so the run's output is REST verbatim — spend i of the
        // PARSED tree is spend i of the output. The parsed tree still carries the
        // back-reference `Arc` sharing the serializer put there, so the reveal hashes
        // are computed on the parsed tree with a pointer-keyed cache.
        let reveal_hashes = simple_generator_reveal_hashes(generator.sexp());
        conditions_from_generator_output(
            output.sexp(),
            cost,
            max_cost,
            input.flags.clvm_flags,
            &input.constants,
            reveal_hashes.as_deref(),
        )
    } else {
        conditions_from_processed_generator_output(output.sexp(), cost, max_cost, &input.constants)
    }
    .map_err(|error| {
        // Never discard the underlying ClvmError: the true cause — which spend /
        // condition / cost — is logged at error level and mapped to the most faithful
        // ChiaError code below.
        log::error!(
            "execute_block_generator: condition extraction failed at height {} (simple_generator={}): {error:?}",
            input.height,
            input.flags.simple_generator
        );
        map_condition_error(error)
    })
}

fn generator_run_error(error: ClvmError) -> ChiaError {
    match error {
        ClvmError::CostExceeded(_, _) => ChiaError::BlockCostExceedsMax,
        _ => ChiaError::GeneratorRuntimeError,
    }
}

// Map a condition-extraction ClvmError to the corresponding ChiaError.
// Recognizable failures get their faithful code; anything
// structurally malformed (a spend not in `(parent puzzle amount solution)` form,
// a non-atom where an atom is required, an amount that is not a valid u64) falls
// through to InvalidBlockSolution, so a genuinely corrupt generator is still
// rejected as an invalid block solution.
fn map_condition_error(error: ClvmError) -> ChiaError {
    match error {
        // A disallowed AGG_SIG_* public key (the G1 infinity element) is an
        // invalid condition, not a malformed block solution.
        ClvmError::InvalidPublicKey(_) => ChiaError::InvalidCondition,
        ClvmError::CostExceeded(_, _) => ChiaError::BlockCostExceedsMax,
        ClvmError::DoubleSpend(_) => ChiaError::DoubleSpend,
        ClvmError::DuplicateCreate(_) => ChiaError::DuplicateOutput,
        _ => ChiaError::InvalidBlockSolution,
    }
}

fn check_simple_generator(
    serialized: &SerializedProgram,
    generator: &Program,
) -> Result<(), ChiaError> {
    if !serialized.as_ref().starts_with(&[0xff, 0x01]) {
        return Err(ChiaError::ComplexGeneratorReceived);
    }
    let Ok((operator, _)) = generator.sexp().split() else {
        return Err(ChiaError::ComplexGeneratorReceived);
    };
    if operator.as_vec().as_deref() == Some(&[1]) {
        Ok(())
    } else {
        Err(ChiaError::ComplexGeneratorReceived)
    }
}

// Per-spend puzzle-reveal tree hashes computed from the PARSED simple generator
// `(q . REST)` with a shared-subtree memo (see `crate::clvm::tree_hash_cache`).
// Returns hashes for the leading well-formed spends;
// iteration stops at the first spend that is not `(parent puzzle amount solution . extra)`
// — `conditions_from_generator_output` errors on that same spend before it would need the
// missing hash, and it falls back to a direct `tree_hash()` for any index not covered.
fn simple_generator_reveal_hashes(generator: &SExp) -> Option<Vec<Bytes32>> {
    // generator = (q . REST); the quote's output is REST; the validator reads
    // output.first() as the spends list — mirror that walk exactly.
    let (_operator, rest) = generator.split().ok()?;
    let all_spends = rest.first().ok()?;
    let spends = all_spends.ref_list();
    let mut reveals = Vec::with_capacity(spends.len());
    for spend in &spends {
        let parts = spend.ref_list();
        if parts.len() < 4 {
            break;
        }
        reveals.push(parts[1]);
    }
    let mut cache = TreeHashCache::default();
    for reveal in &reveals {
        cache.visit_tree(reveal);
    }
    Some(reveals.iter().map(|r| cache.tree_hash(r)).collect())
}

fn generator_ref_atoms(input: &BlockGeneratorInput) -> SExp<'static> {
    SExp::from(
        input
            .generator_refs
            .iter()
            .map(|reference| SExp::Atom(AtomBuf::new(reference.generator.to_bytes())))
            .collect::<Vec<_>>(),
    )
}

pub fn validate_block_aggregate_signature(
    conds: &SpendBundleConditions,
    aggregated_signature: &Bytes96,
    constants: &ConsensusConstants,
) -> Result<(), ChiaError> {
    let mut keys = Vec::<Bytes48>::new();
    let mut messages = Vec::<Message>::new();
    // Bundle-level AGG_SIG_UNSAFE pairs come from the conditions set itself, not any
    // spend, and sign their raw message with no coin or additional-data suffix.
    for (pk, msg) in &conds.agg_sig_unsafe {
        keys.push(Bytes48::parse(pk.as_slice()).map_err(|_| ChiaError::InvalidCondition)?);
        messages
            .push(Message::new(msg.as_slice().to_vec()).map_err(|_| ChiaError::InvalidCondition)?);
    }
    for spend in &conds.spends {
        let coin = Coin {
            parent_coin_info: spend.parent_id,
            puzzle_hash: spend.puzzle_hash,
            amount: spend.coin_amount,
        };
        for (_, pk, msg) in pkm_pairs_for_spend(spend, coin, constants)? {
            keys.push(pk);
            messages.push(msg);
        }
    }
    if keys.is_empty() {
        let mut infinity = [0_u8; 96];
        infinity[0] = 0xc0;
        return if aggregated_signature.as_ref() == infinity {
            Ok(())
        } else {
            Err(ChiaError::BadAggregateSignature)
        };
    }
    let signature: Signature = aggregated_signature
        .try_into()
        .map_err(|_| ChiaError::BadAggregateSignature)?;
    if aggregate_verify_deduped(&keys, &messages, &signature) {
        Ok(())
    } else {
        Err(ChiaError::BadAggregateSignature)
    }
}

// Aggregate-verify a block's AGG_SIG pair set, validating each DISTINCT public key once.
//
// Verdict-preserving: identical key bytes yield identical uncompress/infinity/subgroup
// verdicts, so checking once per distinct key cannot change the accept/reject outcome.
// `PublicKey::validate()` performs the same two checks the per-occurrence path performs
// per pair (infinity, then in-group), and the deduped pairing runs with
// `pks_validate = false` since every key it receives has already passed them. A malformed
// key is rejected at deserialize — the same verdict the per-occurrence path reaches via
// its decoy infinity point.
//
// The dedup branch validates the distinct keys on the CALLING thread, so it engages only
// where it pays: at least half the occurrences are repeats and the distinct set is small.
// Everything else takes the per-occurrence path.
const DEDUP_MAX_DISTINCT_KEYS: usize = 64;

fn aggregate_verify_deduped(keys: &[Bytes48], messages: &[Message], signature: &Signature) -> bool {
    // AUG scheme: each pair verifies pk || msg with an empty aug.
    let augmented = keys
        .iter()
        .zip(messages)
        .map(|(key, msg)| {
            let mut combined = Vec::with_capacity(48 + msg.data().len());
            combined.extend_from_slice(key.as_ref());
            combined.extend_from_slice(msg.data());
            combined
        })
        .collect::<Vec<Vec<u8>>>();
    let message_refs = augmented.iter().map(Vec::as_slice).collect::<Vec<&[u8]>>();

    let mut distinct = HashSet::<Bytes48>::new();
    for key in keys {
        distinct.insert(*key);
        if distinct.len() > DEDUP_MAX_DISTINCT_KEYS {
            break;
        }
    }
    if distinct.len() <= DEDUP_MAX_DISTINCT_KEYS && distinct.len() * 2 <= keys.len() {
        // Dedup branch: uncompress + validate each DISTINCT key once, pairing skips the
        // per-occurrence checks.
        let mut validated = HashMap::<Bytes48, PublicKey>::with_capacity(distinct.len());
        for key in &distinct {
            let Ok(pk) = PublicKey::from_bytes(key.as_ref()) else {
                return false;
            };
            if pk.validate().is_err() {
                return false;
            }
            validated.insert(*key, pk);
        }
        let key_refs = keys
            .iter()
            .map(|key| &validated[key])
            .collect::<Vec<&PublicKey>>();
        matches!(
            signature.aggregate_verify(true, &message_refs, AUG_SCHEME_DST, &key_refs, false),
            BLST_ERROR::BLST_SUCCESS
        )
    } else {
        // Per-occurrence conversion (`unwrap_or_default`, matching `From<Bytes48> for
        // PublicKey`) and per-occurrence validation inside blst's workers.
        let converted = keys
            .iter()
            .map(|key| PublicKey::from_bytes(key.as_ref()).unwrap_or_default())
            .collect::<Vec<PublicKey>>();
        let key_refs = converted.iter().collect::<Vec<&PublicKey>>();
        matches!(
            signature.aggregate_verify(true, &message_refs, AUG_SCHEME_DST, &key_refs, true),
            BLST_ERROR::BLST_SUCCESS
        )
    }
}

pub fn additions_for_conditions(
    conds: &SpendBundleConditions,
    reward_claims: &[Coin],
) -> Vec<Coin> {
    let mut additions = reward_claims.to_vec();
    for spend in &conds.spends {
        for coin in &spend.create_coin {
            additions.push(Coin {
                parent_coin_info: spend.coin_id,
                puzzle_hash: coin.puzzle_hash,
                amount: coin.amount,
            });
        }
    }
    additions
}

pub fn removals_for_conditions(conds: &SpendBundleConditions) -> Vec<Bytes32> {
    conds.spends.iter().map(|spend| spend.coin_id).collect()
}

/// The create-coin hints for one block's spends: `(hint, created_coin_id)` pairs feeding
/// the `coin_hint` index — one pair for every non-reward addition carrying a hint, the
/// hint being the first memo of its `CreateCoin` condition. Reward coins never carry a
/// hint.
///
/// The index is keyed on a fixed 32-byte hint: the entire read/subscription surface is
/// 32-byte (`get_coin_records_by_hint` takes a `bytes32`, and a puzzle-hash subscription
/// fires only for a 32-byte hint), so a hint of any other length is unreachable through
/// any query and is skipped here.
#[must_use]
pub fn hints_for_conditions(conds: &SpendBundleConditions) -> Vec<(Bytes32, Bytes32)> {
    let mut out = Vec::new();
    for spend in &conds.spends {
        for created in &spend.create_coin {
            let Some(hint) = created.hint.as_ref() else {
                continue;
            };
            let bytes = hint.as_slice();
            if bytes.len() != 32 {
                continue;
            }
            let mut arr = [0u8; 32];
            arr.copy_from_slice(bytes);
            let coin = Coin {
                parent_coin_info: spend.coin_id,
                puzzle_hash: created.puzzle_hash,
                amount: created.amount,
            };
            out.push((Bytes32::new(arr), coin.name()));
        }
    }
    out
}

/// Extract the [`CoinSpend`] (coin + raw puzzle reveal + solution) for `coin_id` from a block's
/// transactions generator, reusing the exact CLVM run the body validator drives — no second VM.
///
/// The generator is decompressed and run, and the spend whose derived coin id equals
/// `coin_id` yields its `(puzzle_reveal, solution)` programs verbatim. `Ok(None)` when the
/// generator spends no such coin.
///
/// Post-hard-fork blocks (`simple_generator`, height ≥ `hard_fork_height`) carry the reveal and
/// solution directly in the generator's output — the same `(parent puzzle_reveal amount solution)`
/// spend list [`conditions_from_generator_output`] parses — so extraction is exact. A pre-hard-fork
/// ROM generator evaluates each puzzle internally and surfaces only the puzzle HASH plus conditions
/// ([`conditions_from_processed_generator_output`]), never the reveal; extraction there returns
/// [`ChiaError::GeneratorRuntimeError`].
///
/// # Errors
/// [`ChiaError::GeneratorRuntimeError`] for a pre-hard-fork (ROM) generator; the generator's own
/// [`ChiaError`] on a decode/run/parse failure.
pub fn coin_spend_from_generator(
    input: &BlockGeneratorInput,
    coin_id: &Bytes32,
) -> Result<Option<CoinSpend>, ChiaError> {
    if !input.flags.simple_generator {
        // Pre-hard-fork ROM generators do not surface puzzle reveals; see the doc comment.
        return Err(ChiaError::GeneratorRuntimeError);
    }
    // Same size gate as execute_block_generator_result.
    if input.transactions_generator.as_ref().len() > input.constants.max_generator_size as usize {
        return Err(ChiaError::InvalidTransactionsGeneratorHash);
    }
    let generator = input
        .transactions_generator
        .to_program_backrefs()
        .map_err(|_| ChiaError::InvalidBlockSolution)?;
    check_simple_generator(&input.transactions_generator, &generator)?;
    let max_cost = input.constants.max_block_cost_clvm;
    let byte_cost = (input.transactions_generator.as_ref().len() as u64)
        .checked_mul(input.constants.cost_per_byte)
        .ok_or(ChiaError::InvalidCostResult)?;
    let cost_left = max_cost
        .checked_sub(byte_cost)
        .ok_or(ChiaError::BlockCostExceedsMax)?;
    let (_cost, output) = generator
        .run(cost_left, input.flags.clvm_flags, &Program::default())
        .map_err(generator_run_error)?;
    let out_sexp = output.sexp();
    let all_spends = out_sexp
        .first()
        .map_err(|_| ChiaError::InvalidBlockSolution)?;
    for spend_sexp in all_spends.ref_list() {
        let parts = spend_sexp.ref_list();
        if parts.len() < 4 {
            return Err(ChiaError::InvalidBlockSolution);
        }
        let parent_id =
            Bytes32::parse_atom(parts[0]).map_err(|_| ChiaError::InvalidBlockSolution)?;
        let puzzle_reveal = Program::new_ref(parts[1]);
        let amount = parts[2]
            .as_int()
            .map_err(|_| ChiaError::InvalidBlockSolution)?
            .to_u64()
            .ok_or(ChiaError::InvalidBlockSolution)?;
        let coin = Coin {
            parent_coin_info: parent_id,
            puzzle_hash: puzzle_reveal.tree_hash(),
            amount,
        };
        if coin.name() == *coin_id {
            let solution = Program::new_ref(parts[3]);
            return Ok(Some(CoinSpend {
                coin,
                puzzle_reveal: puzzle_reveal
                    .serialized()
                    .map_err(|_| ChiaError::InvalidBlockSolution)?,
                solution: solution
                    .serialized()
                    .map_err(|_| ChiaError::InvalidBlockSolution)?,
            }));
        }
    }
    Ok(None)
}

/// One parsed condition of a spend's puzzle run — the raw `(opcode, atom vars...)` shape:
/// `opcode` bytes + `vars` atom list; non-atom tail elements (e.g. a CREATE_COIN memo
/// list) are not vars.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawCondition {
    pub opcode: Vec<u8>,
    pub vars: Vec<Vec<u8>>,
}

/// Every [`CoinSpend`] a block's transactions generator produces, in generator order — the
/// storage-free core of the `get_block_spends` RPC.
/// Same simple-generator contract as [`coin_spend_from_generator`]: post-hard-fork blocks carry
/// `(parent puzzle_reveal amount solution)` verbatim; a pre-hard-fork ROM generator surfaces no
/// reveals and errors [`ChiaError::GeneratorRuntimeError`].
///
/// # Errors
/// [`ChiaError::GeneratorRuntimeError`] for a pre-hard-fork (ROM) generator; the generator's own
/// [`ChiaError`] on a decode/run/parse failure.
pub fn coin_spends_from_generator(
    input: &BlockGeneratorInput,
) -> Result<Vec<CoinSpend>, ChiaError> {
    run_simple_generator_spends(input, false).map(|spends| {
        spends
            .into_iter()
            .map(|(coin_spend, _conditions)| coin_spend)
            .collect()
    })
}

/// Every [`CoinSpend`] plus that spend's parsed condition list — the
/// `get_block_spends_with_conditions` RPC:
/// each puzzle reveal is re-run with its solution (trusted context — the block already
/// validated, so each run gets the full block cost budget) and the output list is parsed to
/// `(opcode, atom vars...)` conditions.
///
/// # Errors
/// [`ChiaError::GeneratorRuntimeError`] for a pre-hard-fork (ROM) generator; the generator's own
/// [`ChiaError`] on a decode/run/parse failure.
pub fn coin_spends_with_conditions_from_generator(
    input: &BlockGeneratorInput,
) -> Result<Vec<(CoinSpend, Vec<RawCondition>)>, ChiaError> {
    run_simple_generator_spends(input, true)
}

// Shared body of the two trusted-block spend extractors: run the simple generator exactly as
// `coin_spend_from_generator` does (same size gate, same backref decode, same cost budget), walk
// the `(parent puzzle_reveal amount solution)` spend list, and — when `with_conditions` — re-run
// each puzzle with its solution to parse the condition output.
fn run_simple_generator_spends(
    input: &BlockGeneratorInput,
    with_conditions: bool,
) -> Result<Vec<(CoinSpend, Vec<RawCondition>)>, ChiaError> {
    if !input.flags.simple_generator {
        // Pre-hard-fork ROM generators do not surface puzzle reveals; see coin_spend_from_generator.
        return Err(ChiaError::GeneratorRuntimeError);
    }
    if input.transactions_generator.as_ref().len() > input.constants.max_generator_size as usize {
        return Err(ChiaError::InvalidTransactionsGeneratorHash);
    }
    let generator = input
        .transactions_generator
        .to_program_backrefs()
        .map_err(|_| ChiaError::InvalidBlockSolution)?;
    check_simple_generator(&input.transactions_generator, &generator)?;
    let max_cost = input.constants.max_block_cost_clvm;
    let byte_cost = (input.transactions_generator.as_ref().len() as u64)
        .checked_mul(input.constants.cost_per_byte)
        .ok_or(ChiaError::InvalidCostResult)?;
    let cost_left = max_cost
        .checked_sub(byte_cost)
        .ok_or(ChiaError::BlockCostExceedsMax)?;
    let (_cost, output) = generator
        .run(cost_left, input.flags.clvm_flags, &Program::default())
        .map_err(generator_run_error)?;
    let out_sexp = output.sexp();
    let all_spends = out_sexp
        .first()
        .map_err(|_| ChiaError::InvalidBlockSolution)?;
    let mut out = Vec::new();
    for spend_sexp in all_spends.ref_list() {
        let parts = spend_sexp.ref_list();
        if parts.len() < 4 {
            return Err(ChiaError::InvalidBlockSolution);
        }
        let parent_id =
            Bytes32::parse_atom(parts[0]).map_err(|_| ChiaError::InvalidBlockSolution)?;
        let puzzle_reveal = Program::new_ref(parts[1]);
        let amount = parts[2]
            .as_int()
            .map_err(|_| ChiaError::InvalidBlockSolution)?
            .to_u64()
            .ok_or(ChiaError::InvalidBlockSolution)?;
        let coin = Coin {
            parent_coin_info: parent_id,
            puzzle_hash: puzzle_reveal.tree_hash(),
            amount,
        };
        let solution = Program::new_ref(parts[3]);
        let conditions = if with_conditions {
            // Trusted context (already-validated block): full budget.
            let (_spend_cost, cond_out) = puzzle_reveal
                .run(cost_left, input.flags.clvm_flags, &solution)
                .map_err(generator_run_error)?;
            parse_raw_conditions(cond_out.sexp())?
        } else {
            Vec::new()
        };
        out.push((
            CoinSpend {
                coin,
                puzzle_reveal: puzzle_reveal
                    .serialized()
                    .map_err(|_| ChiaError::InvalidBlockSolution)?,
                solution: solution
                    .serialized()
                    .map_err(|_| ChiaError::InvalidBlockSolution)?,
            },
            conditions,
        ));
    }
    Ok(out)
}

// Parse a puzzle run's output list into raw conditions: for each condition list, the first
// element's atom is the opcode and the vars are the FOLLOWING ATOMS, stopping at the first
// pair (a CREATE_COIN memo list is dropped).
fn parse_raw_conditions(output: &SExp) -> Result<Vec<RawCondition>, ChiaError> {
    let mut conditions = Vec::new();
    for cond_sexp in output.ref_list() {
        let parts = cond_sexp.ref_list();
        let Some(first) = parts.first() else {
            return Err(ChiaError::InvalidBlockSolution);
        };
        let opcode = first.as_vec().ok_or(ChiaError::InvalidBlockSolution)?;
        let mut vars = Vec::new();
        for part in &parts[1..] {
            match part.as_vec() {
                Some(atom) => vars.push(atom),
                None => break,
            }
        }
        conditions.push(RawCondition { opcode, vars });
    }
    Ok(conditions)
}

pub fn additions_root(
    conds: &SpendBundleConditions,
    reward_claims: &[Coin],
) -> Result<Bytes32, ClvmError> {
    canonical_additions_root(&additions_for_conditions(conds, reward_claims))
}

pub fn removals_root(conds: &SpendBundleConditions) -> Bytes32 {
    canonical_removals_root(&removals_for_conditions(conds))
}

pub fn canonical_removals_root(removals: &[Bytes32]) -> Bytes32 {
    merkle_set_root(removals.iter().map(|v| v.bytes()).collect())
}

/// Addition merkle-set leaves: for every puzzle hash, BOTH the puzzle hash and
/// `hash_coin_ids(coin_ids)` are leaves — including single-coin puzzle hashes. The merkle
/// set is order-independent, so grouping order is irrelevant.
pub fn canonical_additions_root(additions: &[Coin]) -> Result<Bytes32, ClvmError> {
    let mut by_puzzle_hash = BTreeMap::<[u8; 32], Vec<[u8; 32]>>::new();
    for coin in additions {
        by_puzzle_hash
            .entry(coin.puzzle_hash.bytes())
            .or_default()
            .push(coin.name().bytes());
    }
    let mut merkle_items = Vec::<[u8; 32]>::new();
    for (puzzle_hash, coin_ids) in by_puzzle_hash {
        merkle_items.push(puzzle_hash);
        merkle_items.push(hash_coin_ids(&coin_ids));
    }
    Ok(merkle_set_root(merkle_items))
}

/// A single coin id hashes to `std_hash(coin_id)` with **no** length/type prefix; multiple
/// coin ids are sorted **descending**, concatenated, then `std_hash`ed. The descending sort
/// is essential — an ascending or unsorted concatenation yields a different leaf and thus
/// `BadAdditionRoot`. Public because `request_additions` proof serving builds the same
/// `[puzzle_hash, hash_coin_ids(coin names)]` leaf pairs.
#[must_use]
pub fn hash_coin_ids(coin_ids: &[[u8; 32]]) -> [u8; 32] {
    if coin_ids.len() == 1 {
        return hash_256(coin_ids[0]);
    }
    let mut sorted = coin_ids.to_vec();
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    let mut buf = Vec::with_capacity(32 * sorted.len());
    for coin_id in &sorted {
        buf.extend_from_slice(coin_id);
    }
    hash_256(buf)
}

pub fn fee_summary(conds: &SpendBundleConditions) -> Result<BlockFeeSummary, ChiaError> {
    Ok(BlockFeeSummary {
        removals_total: conds.removal_amount,
        additions_total: conds.addition_amount,
        reserve_fee: conds.reserve_fee,
    })
}

pub fn block_fee_amount(summary: &BlockFeeSummary) -> Result<u64, ChiaError> {
    let fee = summary
        .removals_total
        .checked_sub(summary.additions_total)
        .ok_or(ChiaError::InvalidBlockFeeAmount)?;
    u64::try_from(fee).map_err(|_| ChiaError::InvalidBlockFeeAmount)
}

pub fn validate_block_conditions(
    conds: &SpendBundleConditions,
    ctx: &ConditionValidationContext,
) -> Result<(), ChiaError> {
    if conds.height_absolute > ctx.block_height {
        return Err(ChiaError::AssertHeightAbsoluteFailed);
    }
    if let Some(before_height) = conds.before_height_absolute
        && ctx.block_height >= before_height
    {
        return Err(ChiaError::AssertHeightAbsoluteFailed);
    }
    if let Some(timestamp) = ctx.previous_transaction_block_timestamp {
        if conds.seconds_absolute > timestamp {
            return Err(ChiaError::AssertSecondsAbsoluteFailed);
        }
        if let Some(before_seconds) = conds.before_seconds_absolute
            && timestamp >= before_seconds
        {
            return Err(ChiaError::AssertSecondsAbsoluteFailed);
        }
    }

    let mut spent = HashSet::new();
    let mut spent_puzzles = HashSet::<Bytes32>::new();
    let mut coin_index = HashMap::<Bytes32, usize>::new();
    let mut created = HashSet::new();
    let mut coin_announcements = HashSet::<Bytes32>::new();
    let mut puzzle_announcements = HashSet::<Bytes32>::new();
    let mut asserted_coin_announcements = Vec::<Bytes32>::new();
    let mut asserted_puzzle_announcements = Vec::<Bytes32>::new();

    // NO announcement-count cap here: the 1024-announcement limit is a MEMPOOL rule —
    // consensus accepts announcement-heavy blocks. The mempool-side cap lives in
    // `spend_bundle` validation.
    for (index, spend) in conds.spends.iter().enumerate() {
        if !spent.insert(spend.coin_id) {
            return Err(ChiaError::DoubleSpend);
        }
        spent_puzzles.insert(spend.puzzle_hash);
        coin_index.insert(spend.coin_id, index);
        if let Some(context) = ctx.coin_context.get(&spend.coin_id) {
            validate_spend_context(spend, *context, ctx)?;
        }
        for coin in &spend.create_coin {
            let created_coin = Coin {
                parent_coin_info: spend.coin_id,
                puzzle_hash: coin.puzzle_hash,
                amount: coin.amount,
            };
            if !created.insert(created_coin.name()) {
                return Err(ChiaError::DuplicateOutput);
            }
        }
        collect_announcements(
            spend,
            &mut coin_announcements,
            &mut puzzle_announcements,
            &mut asserted_coin_announcements,
            &mut asserted_puzzle_announcements,
        );
    }
    for asserted in asserted_coin_announcements {
        if !coin_announcements.contains(&asserted) {
            return Err(ChiaError::AssertAnnounceConsumedFailed);
        }
    }
    for asserted in asserted_puzzle_announcements {
        if !puzzle_announcements.contains(&asserted) {
            return Err(ChiaError::AssertAnnounceConsumedFailed);
        }
    }

    // ASSERT_CONCURRENT_SPEND (64) / ASSERT_CONCURRENT_PUZZLE (65) / ASSERT_EPHEMERAL (76):
    // the asserted coin id / puzzle hash must be spent in this block, and an
    // ephemeral coin must have been created by another spend in this block.
    // CHIP-25 messages (SEND_MESSAGE 66 / RECEIVE_MESSAGE 67) are keyed by
    // (source-commitment, destination-commitment, message) and must net to zero.
    let mut message_counts = HashMap::<Vec<u8>, i64>::new();
    for spend in &conds.spends {
        for coin_id in &spend.assert_concurrent_spend {
            if !spent.contains(coin_id) {
                return Err(ChiaError::AssertConcurrentSpendFailed);
            }
        }
        for puzzle_hash in &spend.assert_concurrent_puzzle {
            if !spent_puzzles.contains(puzzle_hash) {
                return Err(ChiaError::AssertConcurrentPuzzleFailed);
            }
        }
        if spend.assert_ephemeral && !is_ephemeral(spend, &coin_index, &conds.spends) {
            return Err(ChiaError::AssertEphemeralFailed);
        }
        for message in &spend.sent_messages {
            // src = this spend's own commitment (sender bits, mode >> 3);
            // dst = the destination commitment parsed from the condition.
            let mut key = Vec::new();
            own_message_id(&mut key, message.mode >> 3, spend);
            args_message_id(&mut key, &message.args);
            key.extend_from_slice(&message.message);
            *message_counts.entry(key).or_insert(0) += 1;
        }
        for message in &spend.received_messages {
            // src = the source commitment parsed from the condition;
            // dst = this spend's own commitment (receiver bits, mode & 0b111).
            let mut key = Vec::new();
            args_message_id(&mut key, &message.args);
            own_message_id(&mut key, message.mode, spend);
            key.extend_from_slice(&message.message);
            *message_counts.entry(key).or_insert(0) -= 1;
        }
    }
    if message_counts.values().any(|count| *count != 0) {
        return Err(ChiaError::MessageNotSentOrReceived);
    }
    Ok(())
}

// A coin is ephemeral when its parent coin is also spent in this block and that
// parent spend creates a coin with this coin's puzzle hash and amount. The hint
// is not part of a coin's identity, so it is ignored.
fn is_ephemeral(spend: &Spend, coin_index: &HashMap<Bytes32, usize>, spends: &[Spend]) -> bool {
    let Some(&idx) = coin_index.get(&spend.parent_id) else {
        return false;
    };
    spends[idx]
        .create_coin
        .iter()
        .any(|coin| coin.puzzle_hash == spend.puzzle_hash && coin.amount == spend.coin_amount)
}

// Serialize a spend's own commitment for the given 3-bit message mode: a tag byte
// followed by the committed fields.
fn own_message_id(out: &mut Vec<u8>, bits: u8, spend: &Spend) {
    match bits & 0b111 {
        0b000 => out.push(0),
        0b001 => {
            out.push(1);
            out.extend_from_slice(&spend.coin_amount.to_be_bytes());
        }
        0b010 => {
            out.push(2);
            out.extend_from_slice(&spend.puzzle_hash);
        }
        0b011 => {
            out.push(3);
            out.extend_from_slice(&spend.puzzle_hash);
            out.extend_from_slice(&spend.coin_amount.to_be_bytes());
        }
        0b100 => {
            out.push(4);
            out.extend_from_slice(&spend.parent_id);
        }
        0b101 => {
            out.push(5);
            out.extend_from_slice(&spend.parent_id);
            out.extend_from_slice(&spend.coin_amount.to_be_bytes());
        }
        0b110 => {
            out.push(6);
            out.extend_from_slice(&spend.parent_id);
            out.extend_from_slice(&spend.puzzle_hash);
        }
        _ => {
            out.push(7);
            out.extend_from_slice(&spend.coin_id);
        }
    }
}

// Serialize a counterparty commitment parsed from a message condition, using the
// same tag-byte layout as own_message_id so a matched send/receive pair agrees.
fn args_message_id(out: &mut Vec<u8>, args: &MessageArgs) {
    match args {
        MessageArgs::None => out.push(0),
        MessageArgs::Amount(amount) => {
            out.push(1);
            out.extend_from_slice(&amount.to_be_bytes());
        }
        MessageArgs::Puzzle(puzzle_hash) => {
            out.push(2);
            out.extend_from_slice(puzzle_hash);
        }
        MessageArgs::PuzzleAmount {
            puzzle_hash,
            amount,
        } => {
            out.push(3);
            out.extend_from_slice(puzzle_hash);
            out.extend_from_slice(&amount.to_be_bytes());
        }
        MessageArgs::Parent(parent) => {
            out.push(4);
            out.extend_from_slice(parent);
        }
        MessageArgs::ParentAmount { parent, amount } => {
            out.push(5);
            out.extend_from_slice(parent);
            out.extend_from_slice(&amount.to_be_bytes());
        }
        MessageArgs::ParentPuzzle {
            parent,
            puzzle_hash,
        } => {
            out.push(6);
            out.extend_from_slice(parent);
            out.extend_from_slice(puzzle_hash);
        }
        MessageArgs::CoinId(coin_id) => {
            out.push(7);
            out.extend_from_slice(coin_id);
        }
    }
}

/// Assemble a plain (uncompressed) block generator from a set of coin spends. The result
/// is `(q . SPENDS)` where SPENDS is the proper list `((parent-id puzzle-reveal amount solution) …)`.
/// Because `q` (quote) returns its argument verbatim, running the generator yields SPENDS directly —
/// exactly the shape [`execute_block_generator_result`] consumes, so a producer built here round-trips
/// through our own validator. Minimal-signed amount atom via `bigint_to_bytes`, canonical
/// serialization via `sexp_to_bytes`. This is a SIMPLE generator (`0xff 0x01` prefix, quote
/// operator); a plain generator is fully consensus-legal.
pub fn simple_solution_generator(spends: &[CoinSpend]) -> Result<SerializedProgram, ClvmError> {
    Ok(sexp_to_bytes(&build_generator_tree(spends)?)?)
}

/// Build the CLVM tree `(q . (SPENDS))` for `spends` in the given order — the shared shape both the
/// plain ([`simple_solution_generator`]) and back-reference-compressed
/// ([`compressed_solution_generator_from_coin_spends`]) serializers emit. Kept as one builder so the
/// two forms are guaranteed to encode the SAME program (identical run output, tree hash, and
/// conditions — only the byte encoding differs).
fn build_generator_tree(spends: &[CoinSpend]) -> Result<SExp<'static>, ClvmError> {
    let mut items: Vec<SExp<'static>> = Vec::with_capacity(spends.len());
    for s in spends {
        let parent = SExp::Atom(AtomBuf::Owned(std::sync::Arc::new(
            s.coin.parent_coin_info.bytes().to_vec(),
        )));
        let puzzle = sexp_from_bytes(&mut std::io::Cursor::new(s.puzzle_reveal.as_ref()))?;
        let amount = SExp::from(&num_bigint::BigInt::from(s.coin.amount));
        let solution = sexp_from_bytes(&mut std::io::Cursor::new(s.solution.as_ref()))?;
        // ( parent puzzle amount solution )
        items.push(SExp::from(vec![parent, puzzle, amount, solution]));
    }
    let spends_list = SExp::from(items);
    // The validator reads the generator's OUTPUT via `.first()`, so the output must be `(SPENDS . _)`
    // — the spend list wrapped one level. So we quote a one-element list `(SPENDS)`: the empty case
    // is `(q . (()))` = ff01ff8080.
    let output = SExp::from(vec![spends_list]);
    // (q . (SPENDS)): the quote operator (atom 0x01) returns `(SPENDS)` when run.
    Ok(SExp::Pair(PairBuf::Owned((
        std::sync::Arc::new(SExp::Atom(AtomBuf::Owned(std::sync::Arc::new(vec![1u8])))),
        std::sync::Arc::new(output),
    ))))
}

/// The block producer's plain (uncompressed) generator for an ORDERED spend sequence. The
/// wire format holds the spends in REVERSE input order; [`simple_solution_generator`]
/// preserves input order, so this wrapper reverses before assembling. Byte-parity is
/// pinned by `chia_rs_solution_generator_byte_parity` below.
///
/// The plain form is the uncompressed sibling of the back-reference-compressed generator:
/// identical tree, identical consensus validity (post-SF9: starts `ff01`, canonical
/// serialization, parses under the backrefs parser), higher byte cost.
///
/// # Errors
/// Propagates [`simple_solution_generator`]'s CLVM parse/serialize errors.
pub fn solution_generator_from_coin_spends(
    spends: &[CoinSpend],
) -> Result<SerializedProgram, ClvmError> {
    let reversed: Vec<CoinSpend> = spends.iter().rev().cloned().collect();
    simple_solution_generator(&reversed)
}

/// The BACK-REFERENCE-COMPRESSED block generator for `spends`. It
/// builds the IDENTICAL `(q . (SPENDS))` tree as [`solution_generator_from_coin_spends`] (spends in
/// reversed input order) and serializes it with [`sexp_to_bytes_backrefs`], deduplicating
/// repeated subtrees (identical puzzle reveals, shared puzzle hashes) via CLVM back-references
/// (`0xfe`). The result:
/// - decodes (our `sexp_from_bytes_backrefs`, the same decoder validation uses) to the SAME program
///   the plain form does — so it runs to the same conditions at the same execution/condition cost;
/// - is never larger than the plain form, and strictly smaller whenever a ≥4-byte subtree repeats —
///   which is what lets a block pack more spends under `MAX_BLOCK_COST_CLVM`.
///
/// Byte-parity is pinned by `compressed_generator_matches_chia_rs_backrefs` below.
///
/// # Errors
/// Propagates [`build_generator_tree`]'s CLVM parse errors and the serializer's `io::Error`.
pub fn compressed_solution_generator_from_coin_spends(
    spends: &[CoinSpend],
) -> Result<SerializedProgram, ClvmError> {
    let reversed: Vec<CoinSpend> = spends.iter().rev().cloned().collect();
    let tree = build_generator_tree(&reversed)?;
    crate::clvm::serialize_backrefs::sexp_to_bytes_backrefs(&tree)
        .map_err(|e| ClvmError::SerializationError(e.to_string()))
}

/// Serialized length of `spends` wrapped as a PLAIN (uncompressed) `solution_generator` program —
/// the byte cost a bundle is charged as if it were a block generator.
/// This is the plain length; the compressed generator (back-references) is never larger,
/// so this is a sound upper bound on the compressed byte cost — the block producer uses it to gate
/// cheaply and only measures the true compressed size at the cost limit.
#[must_use]
pub fn spend_bundle_generator_length(spends: &[crate::blockchain::coin_spend::CoinSpend]) -> usize {
    fn clvm_bytes_len(val: u64) -> usize {
        if val < 0x80 {
            1
        } else if val < 0x8000 {
            3
        } else if val < 0x0080_0000 {
            4
        } else if val < 0x8000_0000 {
            5
        } else if val < 0x0080_0000_0000 {
            6
        } else if val < 0x8000_0000_0000 {
            7
        } else if val < 0x0080_0000_0000_0000 {
            8
        } else if val < 0x8000_0000_0000_0000 {
            9
        } else {
            10
        }
    }
    let mut size: usize = 5; // (q . (())) => ff01ff8080
    for s in spends {
        // ( parent-id puzzle-reveal amount solution ): 33 bytes of prefixed parent id
        // + 6 bytes of list extension, amount prefix folded into clvm_bytes_len.
        size += 39
            + s.puzzle_reveal.as_ref().len()
            + clvm_bytes_len(s.coin.amount)
            + s.solution.as_ref().len();
    }
    size
}

/// Conditions for a SPEND BUNDLE run standalone — mempool admission's analog of the block
/// generator run: each spend's puzzle runs against its
/// solution under the height's flag ladder plus MEMPOOL_MODE, the puzzle reveal must hash to
/// the spent coin's puzzle hash, and the byte cost is charged as if the bundle were wrapped by
/// `solution_generator` minus the quote bytes it doesn't pay for. The aggregate signature is
/// NOT checked here — hold the returned conditions against
/// [`validate_block_aggregate_signature`] with the bundle's `aggregated_signature`.
///
/// # Errors
/// [`ChiaError::WrongPuzzleHash`] when a reveal doesn't match its coin; otherwise as the
/// generator path (cost, CLVM, condition-parse failures).
pub fn conditions_from_spend_bundle(
    bundle: &crate::blockchain::spend_bundle::SpendBundle,
    height: u32,
    constants: &ConsensusConstants,
) -> Result<SpendBundleConditions, ChiaError> {
    let flags = BlockGeneratorFlags::for_height(constants, height);
    let clvm_flags = flags.clvm_flags | crate::clvm::utils::MEMPOOL_MODE;
    let max_cost = constants.max_block_cost_clvm;
    const QUOTE_BYTES: usize = 2;
    let byte_cost = (spend_bundle_generator_length(&bundle.coin_spends) - QUOTE_BYTES) as u64
        * constants.cost_per_byte;
    let mut conds = SpendBundleConditions {
        spends: Vec::new(),
        reserve_fee: 0,
        height_absolute: 0,
        seconds_absolute: 0,
        before_height_absolute: None,
        before_seconds_absolute: None,
        agg_sig_unsafe: Vec::new(),
        cost: byte_cost,
        removal_amount: 0,
        addition_amount: 0,
    };
    let mut spent = HashSet::<Bytes32>::new();
    let mut created = HashSet::<Bytes32>::new();
    let mut conditions_cost = 0_u64;
    // Bundles repeat puzzle reveals (dust); the serialized reveal bytes are in
    // hand here, so dedup the tree-hash exact-bytes-keyed — identical bytes parse to
    // the identical tree, hence the identical sha256tree.
    let mut reveal_hash_cache: HashMap<&[u8], Bytes32> = HashMap::new();
    for coin_spend in &bundle.coin_spends {
        let puzzle_reveal =
            Program::from_serial(&coin_spend.puzzle_reveal).map_err(map_condition_error)?;
        let solution = Program::from_serial(&coin_spend.solution).map_err(map_condition_error)?;
        let reveal_hash = match reveal_hash_cache.get(coin_spend.puzzle_reveal.as_ref()) {
            Some(hash) => *hash,
            None => {
                let hash = puzzle_reveal.tree_hash();
                reveal_hash_cache.insert(coin_spend.puzzle_reveal.as_ref(), hash);
                hash
            }
        };
        if reveal_hash != coin_spend.coin.puzzle_hash {
            return Err(ChiaError::WrongPuzzleHash);
        }
        if !spent.insert(coin_spend.coin.name()) {
            return Err(map_condition_error(ClvmError::DoubleSpend(format!(
                "{}",
                coin_spend.coin.name()
            ))));
        }
        let cost_left = max_cost
            .checked_sub(conds.cost)
            .ok_or(ChiaError::BlockCostExceedsMax)?;
        let (puzzle_cost, puzzle_output) = puzzle_reveal
            .run(cost_left, clvm_flags, &solution)
            .map_err(map_condition_error)?;
        conds.cost = conds
            .cost
            .checked_add(puzzle_cost)
            .ok_or(ChiaError::BlockCostExceedsMax)?;
        let conditions = puzzle_output
            .sexp()
            .try_into()
            .map_err(map_condition_error)?;
        let mut spend = spend_from_conditions(
            coin_spend.coin,
            conditions,
            &mut conds,
            constants,
            &mut conditions_cost,
            true,
        )
        .map_err(map_condition_error)?;
        spend.execution_cost = puzzle_cost;
        for created_coin in &spend.create_coin {
            let coin = Coin {
                parent_coin_info: spend.coin_id,
                puzzle_hash: created_coin.puzzle_hash,
                amount: created_coin.amount,
            };
            if !created.insert(coin.name()) {
                return Err(map_condition_error(ClvmError::DuplicateCreate(format!(
                    "{}",
                    coin.name()
                ))));
            }
        }
        conds.spends.push(spend);
    }
    conds.cost = conds
        .cost
        .checked_add(conditions_cost)
        .ok_or(ChiaError::BlockCostExceedsMax)?;
    if conds.cost > max_cost {
        return Err(ChiaError::BlockCostExceedsMax);
    }
    // bundle-level fast-forward disqualifiers
    clear_ff_for_bundle_commitments(&mut conds);
    Ok(conds)
}

fn conditions_from_generator_output(
    output: &SExp,
    cost: u64,
    max_cost: u64,
    clvm_flags: u32,
    constants: &ConsensusConstants,
    // Spend-indexed reveal hashes precomputed on the parsed generator (identical
    // tree — the quote returns it verbatim); None on paths without a parsed simple
    // generator. Any uncovered index falls back to hashing the output tree directly.
    reveal_hashes: Option<&[Bytes32]>,
) -> Result<SpendBundleConditions, ClvmError> {
    let mut conds = SpendBundleConditions {
        spends: Vec::new(),
        reserve_fee: 0,
        height_absolute: 0,
        seconds_absolute: 0,
        before_height_absolute: None,
        before_seconds_absolute: None,
        agg_sig_unsafe: Vec::new(),
        cost,
        removal_amount: 0,
        addition_amount: 0,
    };
    let mut spent = HashSet::<Bytes32>::new();
    let mut created = HashSet::<Bytes32>::new();
    let mut conditions_cost = 0_u64;
    let all_spends = output.first()?;
    for (spend_index, spend_sexp) in all_spends.ref_list().into_iter().enumerate() {
        let spend_parts = spend_sexp.ref_list();
        if spend_parts.len() < 4 {
            return Err(ClvmError::InvalidSpendbundle(
                "generator spend must be (parent puzzle_reveal amount solution . extra)"
                    .to_string(),
            ));
        }
        let parent_id = Bytes32::parse_atom(spend_parts[0])?;
        let puzzle_reveal = Program::new_ref(spend_parts[1]).to_owned();
        let puzzle_hash = match reveal_hashes.and_then(|hashes| hashes.get(spend_index)) {
            Some(hash) => *hash,
            None => puzzle_reveal.tree_hash(),
        };
        let amount = spend_parts[2]
            .as_int()?
            .to_u64()
            .ok_or_else(|| ClvmError::AtomNotValidU64(spend_parts[2].to_string()))?;
        let solution = Program::new_ref(spend_parts[3]).to_owned();
        let coin = Coin {
            parent_coin_info: parent_id,
            puzzle_hash,
            amount,
        };
        if !spent.insert(coin.name()) {
            return Err(ClvmError::DoubleSpend(format!("{}", coin.name())));
        }
        let cost_left = max_cost
            .checked_sub(conds.cost)
            .ok_or(ClvmError::CostExceeded(max_cost, conds.cost))?;
        let (puzzle_cost, puzzle_output) = puzzle_reveal.run(cost_left, clvm_flags, &solution)?;
        conds.cost = conds
            .cost
            .checked_add(puzzle_cost)
            .ok_or_else(|| ClvmError::Overflow("puzzle execution cost overflow".to_string()))?;
        let conditions = puzzle_output.sexp().try_into()?;
        let mut spend = spend_from_conditions(
            coin,
            conditions,
            &mut conds,
            constants,
            &mut conditions_cost,
            false,
        )?;
        spend.execution_cost = puzzle_cost;
        for created_coin in &spend.create_coin {
            let coin = Coin {
                parent_coin_info: spend.coin_id,
                puzzle_hash: created_coin.puzzle_hash,
                amount: created_coin.amount,
            };
            if !created.insert(coin.name()) {
                return Err(ClvmError::DuplicateCreate(format!("{}", coin.name())));
            }
        }
        conds.spends.push(spend);
    }
    conds.cost = conds
        .cost
        .checked_add(conditions_cost)
        .ok_or_else(|| ClvmError::Overflow("block generator cost overflow".to_string()))?;
    if conds.cost > max_cost {
        return Err(ClvmError::CostExceeded(max_cost, conds.cost));
    }
    Ok(conds)
}

fn conditions_from_processed_generator_output(
    output: &SExp,
    cost: u64,
    max_cost: u64,
    constants: &ConsensusConstants,
) -> Result<SpendBundleConditions, ClvmError> {
    let mut conds = SpendBundleConditions {
        spends: Vec::new(),
        reserve_fee: 0,
        height_absolute: 0,
        seconds_absolute: 0,
        before_height_absolute: None,
        before_seconds_absolute: None,
        agg_sig_unsafe: Vec::new(),
        cost,
        removal_amount: 0,
        addition_amount: 0,
    };
    let mut spent = HashSet::<Bytes32>::new();
    let mut created = HashSet::<Bytes32>::new();
    let mut conditions_cost = 0_u64;
    let all_spends = output.first()?;
    for spend_sexp in all_spends.ref_list() {
        let spend_parts = spend_sexp.ref_list();
        if spend_parts.len() < 4 {
            return Err(ClvmError::InvalidSpendbundle(
                "generator spend must be (parent puzzle_hash amount conditions . extra)"
                    .to_string(),
            ));
        }
        let parent_id = Bytes32::parse_atom(spend_parts[0])?;
        let puzzle_hash = Bytes32::parse_atom(spend_parts[1])?;
        let amount = spend_parts[2]
            .as_int()?
            .to_u64()
            .ok_or_else(|| ClvmError::AtomNotValidU64(spend_parts[2].to_string()))?;
        let coin = Coin {
            parent_coin_info: parent_id,
            puzzle_hash,
            amount,
        };
        if !spent.insert(coin.name()) {
            return Err(ClvmError::DoubleSpend(format!("{}", coin.name())));
        }
        let conditions = spend_parts[3].try_into()?;
        let spend = spend_from_conditions(
            coin,
            conditions,
            &mut conds,
            constants,
            &mut conditions_cost,
            false,
        )?;
        for created_coin in &spend.create_coin {
            let coin = Coin {
                parent_coin_info: spend.coin_id,
                puzzle_hash: created_coin.puzzle_hash,
                amount: created_coin.amount,
            };
            if !created.insert(coin.name()) {
                return Err(ClvmError::DuplicateCreate(format!("{}", coin.name())));
            }
        }
        conds.spends.push(spend);
    }
    conds.cost = conds
        .cost
        .checked_add(conditions_cost)
        .ok_or_else(|| ClvmError::Overflow("block generator cost overflow".to_string()))?;
    if conds.cost > max_cost {
        return Err(ClvmError::CostExceeded(max_cost, conds.cost));
    }
    Ok(conds)
}

// Per-condition eligibility clearing: given a parsed condition and its position in the
// spend's condition list, clear the DEDUP/FF bits the condition forfeits. The singleton
// top layer emits ASSERT_MY_PARENT_ID as exactly the SECOND condition, so any other
// position marks an inner-puzzle commitment to the specific coin and kills fast-forward.
fn clear_eligibility_for_condition(flags: &mut u32, condition: &ConditionWithArgs, index: usize) {
    match condition {
        ConditionWithArgs::AssertMyCoinId(_)
        | ConditionWithArgs::AssertHeightRelative(_)
        | ConditionWithArgs::AssertSecondsRelative(_)
        | ConditionWithArgs::AssertBeforeHeightRelative(_)
        | ConditionWithArgs::AssertBeforeSecondsRelative(_)
        | ConditionWithArgs::AssertMyBirthHeight(_)
        | ConditionWithArgs::AssertMyBirthSeconds(_)
        | ConditionWithArgs::AssertEphemeral => {
            // A fast-forward changes the coin id, parent and birth frame; previously-passing
            // relative locks would likely fail.
            *flags &= !ELIGIBLE_FOR_FF;
        }
        ConditionWithArgs::AssertMyParentId(_) => {
            if index != 1 {
                *flags &= !ELIGIBLE_FOR_FF;
            }
        }
        ConditionWithArgs::AggSigMe(_, _)
        | ConditionWithArgs::AggSigParent(_, _)
        | ConditionWithArgs::AggSigParentAmount(_, _)
        | ConditionWithArgs::AggSigParentPuzzle(_, _) => {
            // Parent-committing signatures cannot survive a rebase, and no signed spend dedups.
            *flags &= !(ELIGIBLE_FOR_DEDUP | ELIGIBLE_FOR_FF);
        }
        ConditionWithArgs::AggSigPuzzle(_, _)
        | ConditionWithArgs::AggSigAmount(_, _)
        | ConditionWithArgs::AggSigPuzzleAmount(_, _)
        | ConditionWithArgs::AggSigUnsafe(_, _) => {
            *flags &= !ELIGIBLE_FOR_DEDUP;
        }
        ConditionWithArgs::SendMessage(mode, _, _) => {
            // Sender commitment rides in bits 3..6; a PARENT commitment pins the coin's parent.
            if ((mode >> 3) & 0b100) != 0 {
                *flags &= !ELIGIBLE_FOR_FF;
            }
            // De-duplicating a sending spend may leave a receiver without a message.
            *flags &= !ELIGIBLE_FOR_DEDUP;
        }
        ConditionWithArgs::ReceiveMessage(mode, _, _) => {
            if (mode & 0b100) != 0 {
                *flags &= !ELIGIBLE_FOR_FF;
            }
            *flags &= !ELIGIBLE_FOR_DEDUP;
        }
        ConditionWithArgs::CreateCoinAnnouncement(_) => {
            *flags &= !ELIGIBLE_FOR_FF;
        }
        _ => {}
    }
}

fn spend_from_conditions(
    coin: Coin,
    conditions: Vec<ConditionWithArgs>,
    conds: &mut SpendBundleConditions,
    constants: &ConsensusConstants,
    extra_cost: &mut u64,
    // Compute mempool eligibility flags (the mempool conditions run); consensus runs pass
    // false and leave `flags` 0.
    mempool: bool,
) -> Result<Spend, ClvmError> {
    let condition_cost_start = *extra_cost;
    let mut spend = Spend {
        parent_id: coin.parent_coin_info,
        coin_amount: coin.amount,
        puzzle_hash: coin.puzzle_hash,
        coin_id: coin.name(),
        height_relative: None,
        seconds_relative: None,
        before_height_relative: None,
        before_seconds_relative: None,
        birth_height: None,
        birth_seconds: None,
        create_coin: HashSet::new(),
        agg_sig_me: Vec::new(),
        agg_sig_parent: Vec::new(),
        agg_sig_puzzle: Vec::new(),
        agg_sig_amount: Vec::new(),
        agg_sig_puzzle_amount: Vec::new(),
        agg_sig_parent_amount: Vec::new(),
        agg_sig_parent_puzzle: Vec::new(),
        create_coin_announcements: Vec::new(),
        assert_coin_announcements: Vec::new(),
        create_puzzle_announcements: Vec::new(),
        assert_puzzle_announcements: Vec::new(),
        assert_concurrent_spend: Vec::new(),
        assert_concurrent_puzzle: Vec::new(),
        assert_ephemeral: false,
        sent_messages: Vec::new(),
        received_messages: Vec::new(),
        // Assume dedup-eligible; fast-forward candidates must be singletons, which use odd
        // amounts. Cleared per condition below; consensus runs leave flags 0.
        flags: if mempool {
            ELIGIBLE_FOR_DEDUP
                | if coin.amount & 1 == 1 {
                    ELIGIBLE_FOR_FF
                } else {
                    0
                }
        } else {
            0
        },
        condition_cost: 0,
        execution_cost: 0,
    };
    conds.removal_amount = conds
        .removal_amount
        .checked_add(u128::from(coin.amount))
        .ok_or_else(|| ClvmError::Overflow("removal amount overflow".to_string()))?;
    for (condition_index, condition) in conditions.into_iter().enumerate() {
        if mempool {
            clear_eligibility_for_condition(&mut spend.flags, &condition, condition_index);
        }
        // The BLS G1 infinity/identity element is an invalid AGG_SIG_* public key.
        // Reject it here, during condition aggregation and before signature
        // verification, so the block is refused on condition validity rather than a
        // deferred aggregate-signature failure.
        if let Some(public_key) = condition.agg_sig_infinity_pubkey() {
            return Err(ClvmError::InvalidPublicKey(public_key));
        }
        match condition {
            ConditionWithArgs::CreateCoin(puzzle_hash, amount, memos) => {
                *extra_cost = extra_cost
                    .checked_add(CREATE_COIN_COST)
                    .ok_or_else(|| ClvmError::Overflow("create coin cost overflow".to_string()))?;
                conds.addition_amount = conds
                    .addition_amount
                    .checked_add(u128::from(amount))
                    .ok_or_else(|| ClvmError::Overflow("addition amount overflow".to_string()))?;
                spend.create_coin.insert(NewCoin {
                    puzzle_hash,
                    amount,
                    hint: memos.first().map(|memo| UnsizedBytes::new(memo.clone())),
                });
            }
            ConditionWithArgs::ReserveFee(fee) => {
                conds.reserve_fee = conds
                    .reserve_fee
                    .checked_add(fee)
                    .ok_or_else(|| ClvmError::Overflow("reserve fee overflow".to_string()))?;
            }
            ConditionWithArgs::AssertHeightAbsolute(height) => {
                conds.height_absolute = max(conds.height_absolute, height);
            }
            ConditionWithArgs::AssertSecondsAbsolute(seconds) => {
                conds.seconds_absolute = max(conds.seconds_absolute, seconds);
            }
            ConditionWithArgs::AssertBeforeHeightAbsolute(height) => {
                conds.before_height_absolute = Some(match conds.before_height_absolute {
                    Some(existing) => min(existing, height),
                    None => height,
                });
            }
            ConditionWithArgs::AssertBeforeSecondsAbsolute(seconds) => {
                conds.before_seconds_absolute = Some(match conds.before_seconds_absolute {
                    Some(existing) => min(existing, seconds),
                    None => seconds,
                });
            }
            ConditionWithArgs::AssertHeightRelative(height) => {
                spend.height_relative = Some(max(spend.height_relative.unwrap_or(0), height));
            }
            ConditionWithArgs::AssertSecondsRelative(seconds) => {
                spend.seconds_relative = Some(max(spend.seconds_relative.unwrap_or(0), seconds));
            }
            ConditionWithArgs::AssertBeforeHeightRelative(height) => {
                spend.before_height_relative = Some(match spend.before_height_relative {
                    Some(existing) => min(existing, height),
                    None => height,
                });
            }
            ConditionWithArgs::AssertBeforeSecondsRelative(seconds) => {
                spend.before_seconds_relative = Some(match spend.before_seconds_relative {
                    Some(existing) => min(existing, seconds),
                    None => seconds,
                });
            }
            ConditionWithArgs::AssertMyBirthHeight(height) => spend.birth_height = Some(height),
            ConditionWithArgs::AssertMyBirthSeconds(seconds) => spend.birth_seconds = Some(seconds),
            ConditionWithArgs::AggSigMe(pk, msg) => {
                *extra_cost = extra_cost.checked_add(AGG_SIG_COST).ok_or_else(|| {
                    ClvmError::Overflow("aggregate signature cost overflow".to_string())
                })?;
                spend.agg_sig_me.push((
                    UnsizedBytes::new(pk.bytes().to_vec()),
                    UnsizedBytes::from(msg.data()),
                ));
            }
            ConditionWithArgs::AggSigParent(pk, msg) => {
                *extra_cost = extra_cost.checked_add(AGG_SIG_COST).ok_or_else(|| {
                    ClvmError::Overflow("aggregate signature cost overflow".to_string())
                })?;
                spend.agg_sig_parent.push((
                    UnsizedBytes::new(pk.bytes().to_vec()),
                    UnsizedBytes::from(msg.data()),
                ));
            }
            ConditionWithArgs::AggSigPuzzle(pk, msg) => {
                *extra_cost = extra_cost.checked_add(AGG_SIG_COST).ok_or_else(|| {
                    ClvmError::Overflow("aggregate signature cost overflow".to_string())
                })?;
                spend.agg_sig_puzzle.push((
                    UnsizedBytes::new(pk.bytes().to_vec()),
                    UnsizedBytes::from(msg.data()),
                ));
            }
            ConditionWithArgs::AggSigAmount(pk, msg) => {
                *extra_cost = extra_cost.checked_add(AGG_SIG_COST).ok_or_else(|| {
                    ClvmError::Overflow("aggregate signature cost overflow".to_string())
                })?;
                spend.agg_sig_amount.push((
                    UnsizedBytes::new(pk.bytes().to_vec()),
                    UnsizedBytes::from(msg.data()),
                ));
            }
            ConditionWithArgs::AggSigPuzzleAmount(pk, msg) => {
                *extra_cost = extra_cost.checked_add(AGG_SIG_COST).ok_or_else(|| {
                    ClvmError::Overflow("aggregate signature cost overflow".to_string())
                })?;
                spend.agg_sig_puzzle_amount.push((
                    UnsizedBytes::new(pk.bytes().to_vec()),
                    UnsizedBytes::from(msg.data()),
                ));
            }
            ConditionWithArgs::AggSigParentAmount(pk, msg) => {
                *extra_cost = extra_cost.checked_add(AGG_SIG_COST).ok_or_else(|| {
                    ClvmError::Overflow("aggregate signature cost overflow".to_string())
                })?;
                spend.agg_sig_parent_amount.push((
                    UnsizedBytes::new(pk.bytes().to_vec()),
                    UnsizedBytes::from(msg.data()),
                ));
            }
            ConditionWithArgs::AggSigParentPuzzle(pk, msg) => {
                *extra_cost = extra_cost.checked_add(AGG_SIG_COST).ok_or_else(|| {
                    ClvmError::Overflow("aggregate signature cost overflow".to_string())
                })?;
                spend.agg_sig_parent_puzzle.push((
                    UnsizedBytes::new(pk.bytes().to_vec()),
                    UnsizedBytes::from(msg.data()),
                ));
            }
            ConditionWithArgs::AggSigUnsafe(pk, msg) => {
                *extra_cost = extra_cost.checked_add(AGG_SIG_COST).ok_or_else(|| {
                    ClvmError::Overflow("aggregate signature cost overflow".to_string())
                })?;
                verify_agg_sig_unsafe_message(&msg, constants)?;
                conds.agg_sig_unsafe.push((
                    UnsizedBytes::new(pk.bytes().to_vec()),
                    UnsizedBytes::from(msg.data()),
                ));
            }
            ConditionWithArgs::CreateCoinAnnouncement(msg) => {
                spend
                    .create_coin_announcements
                    .push(UnsizedBytes::from(msg.data()));
            }
            ConditionWithArgs::AssertCoinAnnouncement(announcement) => {
                spend.assert_coin_announcements.push(announcement);
            }
            ConditionWithArgs::CreatePuzzleAnnouncement(msg) => {
                spend
                    .create_puzzle_announcements
                    .push(UnsizedBytes::from(msg.data()));
            }
            ConditionWithArgs::AssertPuzzleAnnouncement(announcement) => {
                spend.assert_puzzle_announcements.push(announcement);
            }
            ConditionWithArgs::AssertMyCoinId(coin_id) if coin_id != coin.name() => {
                return Err(ClvmError::InvalidSpendbundle(
                    "ASSERT_MY_COIN_ID".to_string(),
                ));
            }
            ConditionWithArgs::AssertMyParentId(parent_id)
                if parent_id != coin.parent_coin_info =>
            {
                return Err(ClvmError::InvalidSpendbundle(
                    "ASSERT_MY_PARENT_ID".to_string(),
                ));
            }
            ConditionWithArgs::AssertMyPuzzlehash(puzzle_hash)
                if puzzle_hash != coin.puzzle_hash =>
            {
                return Err(ClvmError::InvalidSpendbundle(
                    "ASSERT_MY_PUZZLEHASH".to_string(),
                ));
            }
            ConditionWithArgs::AssertMyAmount(amount) if amount != coin.amount => {
                return Err(ClvmError::InvalidSpendbundle(
                    "ASSERT_MY_AMOUNT".to_string(),
                ));
            }
            ConditionWithArgs::SoftFork(cost) => {
                *extra_cost = extra_cost
                    .checked_add(cost)
                    .ok_or_else(|| ClvmError::Overflow("soft fork cost overflow".to_string()))?;
            }
            ConditionWithArgs::AssertConcurrentSpend(coin_id) => {
                spend.assert_concurrent_spend.push(coin_id);
            }
            ConditionWithArgs::AssertConcurrentPuzzle(puzzle_hash) => {
                spend.assert_concurrent_puzzle.push(puzzle_hash);
            }
            ConditionWithArgs::AssertEphemeral => {
                spend.assert_ephemeral = true;
            }
            ConditionWithArgs::SendMessage(mode, message, args) => {
                spend.sent_messages.push(SpendMessage {
                    mode,
                    message: message.data().to_vec(),
                    args,
                });
            }
            ConditionWithArgs::ReceiveMessage(mode, message, args) => {
                spend.received_messages.push(SpendMessage {
                    mode,
                    message: message.data().to_vec(),
                    args,
                });
            }
            _ => {}
        }
    }
    // This spend's share of the accumulated condition cost — the delta the loop above
    // added to the bundle-wide accumulator.
    spend.condition_cost = extra_cost.saturating_sub(condition_cost_start);
    if mempool {
        // A fast-forward candidate must actually look like a singleton — an output coin
        // with the spend's own puzzle hash and amount.
        if (spend.flags & ELIGIBLE_FOR_FF) != 0
            && !spend
                .create_coin
                .iter()
                .any(|c| c.puzzle_hash == spend.puzzle_hash && c.amount == spend.coin_amount)
        {
            spend.flags &= !ELIGIBLE_FOR_FF;
        }
        // A spend with an excess amount (paying a fee or funding siblings) must not dedup — the
        // duplicate would pay twice.
        if (spend.flags & ELIGIBLE_FOR_DEDUP) != 0 {
            let spend_additions: u128 =
                spend.create_coin.iter().map(|c| u128::from(c.amount)).sum();
            if u128::from(spend.coin_amount) > spend_additions {
                spend.flags &= !ELIGIBLE_FOR_DEDUP;
            }
        }
    }
    Ok(spend)
}

// The bundle-level
// fast-forward disqualifiers that only resolve once every spend is parsed. (a) a coin referenced
// by an in-bundle ASSERT_CONCURRENT_SPEND is committed to its exact id; (b) a spend whose output
// is itself spent by this bundle (an ephemeral child) is likewise committed — rebasing the parent
// would orphan the child.
fn clear_ff_for_bundle_commitments(conds: &mut SpendBundleConditions) {
    let spent_coins: HashMap<Bytes32, usize> = conds
        .spends
        .iter()
        .enumerate()
        .map(|(index, spend)| (spend.coin_id, index))
        .collect();
    let mut committed: Vec<usize> = Vec::new();
    for spend in &conds.spends {
        for coin_id in &spend.assert_concurrent_spend {
            if let Some(&index) = spent_coins.get(coin_id) {
                committed.push(index);
            }
        }
    }
    for index in committed {
        conds.spends[index].flags &= !ELIGIBLE_FOR_FF;
    }
    for index in 0..conds.spends.len() {
        if (conds.spends[index].flags & ELIGIBLE_FOR_FF) == 0 {
            continue;
        }
        let spend = &conds.spends[index];
        let child_spent = spend.create_coin.iter().any(|c| {
            spent_coins.contains_key(
                &Coin {
                    parent_coin_info: spend.coin_id,
                    puzzle_hash: c.puzzle_hash,
                    amount: c.amount,
                }
                .name(),
            )
        });
        if child_spent {
            conds.spends[index].flags &= !ELIGIBLE_FOR_FF;
        }
    }
}

fn pkm_pairs_for_spend(
    spend: &Spend,
    coin: Coin,
    constants: &ConsensusConstants,
) -> Result<Vec<(ConditionOpcode, Bytes48, Message)>, ChiaError> {
    let mut conditions = Vec::<ConditionWithArgs>::new();
    for (pk, msg) in &spend.agg_sig_me {
        conditions.push(ConditionWithArgs::AggSigMe(
            Bytes48::parse(pk.as_slice()).map_err(|_| ChiaError::InvalidCondition)?,
            Message::new(msg.as_slice().to_vec()).map_err(|_| ChiaError::InvalidCondition)?,
        ));
    }
    for (pk, msg) in &spend.agg_sig_parent {
        conditions.push(ConditionWithArgs::AggSigParent(
            Bytes48::parse(pk.as_slice()).map_err(|_| ChiaError::InvalidCondition)?,
            Message::new(msg.as_slice().to_vec()).map_err(|_| ChiaError::InvalidCondition)?,
        ));
    }
    for (pk, msg) in &spend.agg_sig_puzzle {
        conditions.push(ConditionWithArgs::AggSigPuzzle(
            Bytes48::parse(pk.as_slice()).map_err(|_| ChiaError::InvalidCondition)?,
            Message::new(msg.as_slice().to_vec()).map_err(|_| ChiaError::InvalidCondition)?,
        ));
    }
    for (pk, msg) in &spend.agg_sig_amount {
        conditions.push(ConditionWithArgs::AggSigAmount(
            Bytes48::parse(pk.as_slice()).map_err(|_| ChiaError::InvalidCondition)?,
            Message::new(msg.as_slice().to_vec()).map_err(|_| ChiaError::InvalidCondition)?,
        ));
    }
    for (pk, msg) in &spend.agg_sig_puzzle_amount {
        conditions.push(ConditionWithArgs::AggSigPuzzleAmount(
            Bytes48::parse(pk.as_slice()).map_err(|_| ChiaError::InvalidCondition)?,
            Message::new(msg.as_slice().to_vec()).map_err(|_| ChiaError::InvalidCondition)?,
        ));
    }
    for (pk, msg) in &spend.agg_sig_parent_amount {
        conditions.push(ConditionWithArgs::AggSigParentAmount(
            Bytes48::parse(pk.as_slice()).map_err(|_| ChiaError::InvalidCondition)?,
            Message::new(msg.as_slice().to_vec()).map_err(|_| ChiaError::InvalidCondition)?,
        ));
    }
    for (pk, msg) in &spend.agg_sig_parent_puzzle {
        conditions.push(ConditionWithArgs::AggSigParentPuzzle(
            Bytes48::parse(pk.as_slice()).map_err(|_| ChiaError::InvalidCondition)?,
            Message::new(msg.as_slice().to_vec()).map_err(|_| ChiaError::InvalidCondition)?,
        ));
    }
    pkm_pairs_for_conditions(
        &conditions,
        coin,
        constants.agg_sig_me_additional_data.as_ref(),
    )
    .map_err(|_| ChiaError::InvalidCondition)
}

fn validate_spend_context(
    spend: &Spend,
    context: CoinSpendContext,
    block_context: &ConditionValidationContext,
) -> Result<(), ChiaError> {
    if let Some(expected) = spend.birth_height
        && context.birth_height != Some(expected)
    {
        return Err(ChiaError::InvalidCondition);
    }
    if let Some(expected) = spend.birth_seconds
        && context.birth_seconds != Some(expected)
    {
        return Err(ChiaError::InvalidCondition);
    }
    if let Some(relative) = spend.height_relative
        && context
            .spent_height
            .and_then(|height| height.checked_add(relative))
            .is_some_and(|required| block_context.block_height < required)
    {
        return Err(ChiaError::AssertHeightRelativeFailed);
    }
    if let Some(before_relative) = spend.before_height_relative
        && context
            .spent_height
            .and_then(|height| height.checked_add(before_relative))
            .is_some_and(|before| block_context.block_height >= before)
    {
        return Err(ChiaError::AssertHeightRelativeFailed);
    }
    if let Some(timestamp) = block_context.previous_transaction_block_timestamp {
        if let Some(relative) = spend.seconds_relative
            && context
                .spent_seconds
                .and_then(|seconds| seconds.checked_add(relative))
                .is_some_and(|required| timestamp < required)
        {
            return Err(ChiaError::AssertSecondsRelativeFailed);
        }
        if let Some(before_relative) = spend.before_seconds_relative
            && context
                .spent_seconds
                .and_then(|seconds| seconds.checked_add(before_relative))
                .is_some_and(|before| timestamp >= before)
        {
            return Err(ChiaError::AssertSecondsRelativeFailed);
        }
    }
    Ok(())
}

fn collect_announcements(
    spend: &Spend,
    coin_announcements: &mut HashSet<Bytes32>,
    puzzle_announcements: &mut HashSet<Bytes32>,
    asserted_coin_announcements: &mut Vec<Bytes32>,
    asserted_puzzle_announcements: &mut Vec<Bytes32>,
) {
    for msg in &spend.create_coin_announcements {
        let mut buf = Vec::with_capacity(32 + msg.as_slice().len());
        buf.extend_from_slice(spend.coin_id.as_ref());
        buf.extend_from_slice(msg.as_slice());
        coin_announcements.insert(hash_256(buf).into());
    }
    for msg in &spend.create_puzzle_announcements {
        let mut buf = Vec::with_capacity(32 + msg.as_slice().len());
        buf.extend_from_slice(spend.puzzle_hash.as_ref());
        buf.extend_from_slice(msg.as_slice());
        puzzle_announcements.insert(hash_256(buf).into());
    }
    asserted_coin_announcements.extend(spend.assert_coin_announcements.iter().copied());
    asserted_puzzle_announcements.extend(spend.assert_puzzle_announcements.iter().copied());
}

// Merkle-set node-type tags: hashdown(buf) = sha256(bytes([0]*30) + buf), terminal root
// sha256(b"\1" + key), empty root all-zeros.
const MERKLE_NODE_EMPTY: u8 = 0;
const MERKLE_NODE_TERMINAL: u8 = 1;
const MERKLE_NODE_MIDDLE: u8 = 2;
const MERKLE_BLANK: [u8; 32] = [0u8; 32];

/// The structural kind of a subtree, tracked so the parent knows which type byte to feed
/// `hashdown` and — critically — whether an all-on-one-side level collapses: a `Terminal`
/// or a terminal *pair* (`MiddleDbl`) forwards up through empty levels unchanged, but a
/// genuine `Middle` is wrapped in an empty-sibling node.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MerkleNodeType {
    Terminal,
    Middle,
    MiddleDbl,
}

impl MerkleNodeType {
    /// The type byte a node contributes to its parent's `hashdown`: terminals are `1`, and both
    /// `Middle` and `MiddleDbl` are `2` (the double-terminal distinction only affects collapsing,
    /// never the wire tag).
    fn tag(self) -> u8 {
        match self {
            MerkleNodeType::Terminal => MERKLE_NODE_TERMINAL,
            MerkleNodeType::Middle | MerkleNodeType::MiddleDbl => MERKLE_NODE_MIDDLE,
        }
    }
}

/// `hashdown`: `sha256([0u8; 30] ++ ltag ++ rtag ++ lhash ++ rhash)`.
fn merkle_hashdown(ltag: u8, rtag: u8, left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut buf = [0u8; 96];
    buf[30] = ltag;
    buf[31] = rtag;
    buf[32..64].copy_from_slice(left);
    buf[64..96].copy_from_slice(right);
    hash_256(buf)
}

/// Compute the merkle-set root over `items` (already-hashed 32-byte leaves).
/// Order-independent: the items are sorted and de-duplicated, then the radix tree is built
/// by big-endian bit traversal. Empty ⇒ all-zeros, single ⇒ `sha256(b"\1" + key)`,
/// interior via `hashdown`.
fn merkle_set_root(mut items: Vec<[u8; 32]>) -> Bytes32 {
    items.sort_unstable();
    items.dedup();
    if items.is_empty() {
        // Empty set ⇒ all-zero root (NOT sha256 of the empty string).
        return Bytes32::new(MERKLE_BLANK);
    }
    let (hash, node_type) = merkle_set_recurse(&items, 0);
    match node_type {
        // A lone terminal is finalized as the root by hashing it with the terminal tag:
        // `sha256(b"\1" + key)`. As an interior child it would instead contribute its raw key.
        MerkleNodeType::Terminal => {
            let mut buf = [0u8; 33];
            buf[0] = MERKLE_NODE_TERMINAL;
            buf[1..].copy_from_slice(&hash);
            Bytes32::new(hash_256(buf))
        }
        // A middle (or terminal-pair) root is already a full `hashdown`.
        MerkleNodeType::Middle | MerkleNodeType::MiddleDbl => Bytes32::new(hash),
    }
}

/// Returns `(node_hash, node_type)` for the subtree over `items` at bit depth `depth`. `items` is
/// non-empty, sorted, and distinct. A terminal's `node_hash` is its raw key (finalization to
/// `sha256(b"\1" + key)` happens only if it is the whole-tree root); an interior node's `node_hash` is
/// its `hashdown`.
fn merkle_set_recurse(items: &[[u8; 32]], depth: usize) -> ([u8; 32], MerkleNodeType) {
    debug_assert!(!items.is_empty());
    if items.len() == 1 {
        return (items[0], MerkleNodeType::Terminal);
    }
    // Sorted ascending ⇒ bit-clear leaves precede bit-set leaves; the split is the first bit-set item.
    let split = items
        .iter()
        .position(|item| bit_is_set(item, depth))
        .unwrap_or(items.len());
    let (left, right) = items.split_at(split);
    match (left.is_empty(), right.is_empty()) {
        // All leaves have this bit set: recurse right. A `Middle` child is wrapped in an empty-left
        // node; a `Terminal`/`MiddleDbl` child is forwarded up unchanged (the collapse rule).
        (true, false) => {
            let (child, child_type) = merkle_set_recurse(right, depth + 1);
            if child_type == MerkleNodeType::Middle {
                (
                    merkle_hashdown(MERKLE_NODE_EMPTY, MERKLE_NODE_MIDDLE, &MERKLE_BLANK, &child),
                    MerkleNodeType::Middle,
                )
            } else {
                (child, child_type)
            }
        }
        // All leaves have this bit clear: symmetric, empty-right.
        (false, true) => {
            let (child, child_type) = merkle_set_recurse(left, depth + 1);
            if child_type == MerkleNodeType::Middle {
                (
                    merkle_hashdown(MERKLE_NODE_MIDDLE, MERKLE_NODE_EMPTY, &child, &MERKLE_BLANK),
                    MerkleNodeType::Middle,
                )
            } else {
                (child, child_type)
            }
        }
        // Genuine split: a middle over both children. It is a `MiddleDbl` iff both children are raw
        // terminals (so it collapses through higher empty levels); otherwise a `Middle`.
        (false, false) => {
            let (lhash, ltype) = merkle_set_recurse(left, depth + 1);
            let (rhash, rtype) = merkle_set_recurse(right, depth + 1);
            let node = merkle_hashdown(ltype.tag(), rtype.tag(), &lhash, &rhash);
            let node_type =
                if ltype == MerkleNodeType::Terminal && rtype == MerkleNodeType::Terminal {
                    MerkleNodeType::MiddleDbl
                } else {
                    MerkleNodeType::Middle
                };
            (node, node_type)
        }
        // Non-empty input cannot split into two empty halves.
        (true, true) => unreachable!("merkle_set_recurse called on empty range"),
    }
}

fn bit_is_set(item: &[u8; 32], bit: usize) -> bool {
    let byte = bit / 8;
    let offset = 7 - (bit % 8);
    byte < item.len() && (item[byte] & (1 << offset)) != 0
}

fn chia_error_code(error: ChiaError) -> u16 {
    match error {
        ChiaError::InvalidBlockSolution => 2,
        ChiaError::DuplicateOutput => 4,
        ChiaError::DoubleSpend => 5,
        ChiaError::BadAggregateSignature => 7,
        ChiaError::InvalidCondition => 10,
        ChiaError::AssertAnnounceConsumedFailed => 12,
        ChiaError::AssertHeightRelativeFailed => 13,
        ChiaError::AssertHeightAbsoluteFailed => 14,
        ChiaError::AssertSecondsAbsoluteFailed => 15,
        ChiaError::CoinAmountExceedsMaximum => 16,
        ChiaError::BlockCostExceedsMax => 23,
        ChiaError::BadAdditionRoot => 24,
        ChiaError::BadRemovalRoot => 25,
        ChiaError::ReserveFeeConditionFailed => 48,
        ChiaError::TooManyGeneratorRefs => 113,
        ChiaError::GeneratorRuntimeError => 117,
        ChiaError::InvalidCostResult => 118,
        ChiaError::FutureGeneratorRefs => 120,
        ChiaError::GeneratorRefHasNoGenerator => 121,
        ChiaError::CoinAmountNegative => 124,
        ChiaError::AssertConcurrentSpendFailed => 132,
        ChiaError::AssertConcurrentPuzzleFailed => 133,
        ChiaError::AssertEphemeralFailed => 140,
        ChiaError::MessageNotSentOrReceived => 147,
        ChiaError::ComplexGeneratorReceived => 148,
        _ => 1,
    }
}

trait ParseAtomBytes32 {
    fn parse_atom(sexp: &SExp) -> Result<Self, ClvmError>
    where
        Self: Sized;
}

impl ParseAtomBytes32 for Bytes32 {
    fn parse_atom(sexp: &SExp) -> Result<Self, ClvmError> {
        Bytes32::parse(
            &sexp
                .as_vec()
                .ok_or_else(|| ClvmError::ExpectedAtomGotPair(sexp.to_string()))?,
        )
    }
}

#[cfg(test)]
mod merkle_set_tests {
    use super::{hash_coin_ids, merkle_set_root};
    use crate::traits::SizedBytes;

    // hashdown(buf) = sha256(bytes([0] * 30) + buf)
    fn hashdown(buf: &[u8]) -> [u8; 32] {
        let mut input = vec![0u8; 30];
        input.extend_from_slice(buf);
        super::hash_256(input)
    }

    fn leaf(first: u8) -> [u8; 32] {
        let mut v = [0u8; 32];
        v[0] = first;
        v
    }

    // the empty set roots to all-zero bytes32
    #[test]
    fn empty_set_root_is_zero() {
        assert_eq!(merkle_set_root(vec![]).bytes(), [0u8; 32]);
    }

    // a single leaf roots to sha256(b"\1" + key)
    #[test]
    fn single_leaf_root() {
        let a = leaf(0x80);
        let mut buf = vec![1u8];
        buf.extend_from_slice(&a);
        assert_eq!(merkle_set_root(vec![a]).bytes(), super::hash_256(buf));
    }

    // duplicates collapse to the single-leaf root
    #[test]
    fn duplicate_leaf_root() {
        let a = leaf(0x80);
        let mut buf = vec![1u8];
        buf.extend_from_slice(&a);
        assert_eq!(merkle_set_root(vec![a, a]).bytes(), super::hash_256(buf));
    }

    // two leaves -> hashdown(\1\1 + b + a) (b's bit clear -> left; order-independent)
    #[test]
    fn two_leaf_root() {
        let a = leaf(0x80);
        let b = leaf(0x70);
        let mut buf = vec![1u8, 1u8];
        buf.extend_from_slice(&b);
        buf.extend_from_slice(&a);
        let expected = hashdown(&buf);
        assert_eq!(merkle_set_root(vec![a, b]).bytes(), expected);
        assert_eq!(merkle_set_root(vec![b, a]).bytes(), expected);
    }

    // hashdown(\2\1 + hashdown(\1\1 + b + c) + a) — a MiddleDbl {b,c} collapses through
    // the shared-bit levels rather than emitting empty children.
    #[test]
    fn three_leaf_root_collapses_pair() {
        let a = leaf(0x80);
        let b = leaf(0x70);
        let c = leaf(0x71);
        let mut bc = vec![1u8, 1u8];
        bc.extend_from_slice(&b);
        bc.extend_from_slice(&c);
        let bc = hashdown(&bc);
        let mut top = vec![2u8, 1u8];
        top.extend_from_slice(&bc);
        top.extend_from_slice(&a);
        let expected = hashdown(&top);
        assert_eq!(merkle_set_root(vec![a, b, c]).bytes(), expected);
        assert_eq!(merkle_set_root(vec![c, b, a]).bytes(), expected);
    }

    // two MiddleDbl subtrees -> hashdown(\2\2 + hashdown(\1\1+b+c) + hashdown(\1\1+a+d))
    #[test]
    fn four_leaf_root() {
        let a = leaf(0x80);
        let b = leaf(0x70);
        let c = leaf(0x71);
        let d = leaf(0x81);
        let mut bc = vec![1u8, 1u8];
        bc.extend_from_slice(&b);
        bc.extend_from_slice(&c);
        let bc = hashdown(&bc);
        let mut ad = vec![1u8, 1u8];
        ad.extend_from_slice(&a);
        ad.extend_from_slice(&d);
        let ad = hashdown(&ad);
        let mut top = vec![2u8, 2u8];
        top.extend_from_slice(&bc);
        top.extend_from_slice(&ad);
        assert_eq!(merkle_set_root(vec![a, b, c, d]).bytes(), hashdown(&top));
    }

    // exercises the empty-child chain a genuine `Middle` subtree emits through
    // shared-bit levels (the case a `MiddleDbl` collapses but a `Middle` does not)
    #[test]
    fn five_leaf_root_emits_empty_children() {
        const BLANK: [u8; 32] = [0u8; 32];
        let a = leaf(0x58);
        let b = leaf(0x23);
        let c = leaf(0x21);
        let d = leaf(0xCA);
        let e = leaf(0x20);

        let cat = |tags: [u8; 2], l: &[u8; 32], r: &[u8; 32]| {
            let mut buf = vec![tags[0], tags[1]];
            buf.extend_from_slice(l);
            buf.extend_from_slice(r);
            hashdown(&buf)
        };
        let mut expected = cat([1, 1], &e, &c);
        expected = cat([2, 1], &expected, &b);
        expected = cat([2, 0], &expected, &BLANK);
        expected = cat([2, 0], &expected, &BLANK);
        expected = cat([2, 0], &expected, &BLANK);
        expected = cat([0, 2], &BLANK, &expected);
        expected = cat([2, 1], &expected, &a);
        expected = cat([2, 1], &expected, &d);

        assert_eq!(merkle_set_root(vec![a, b, c, d, e]).bytes(), expected);
        assert_eq!(merkle_set_root(vec![e, d, c, b, a]).bytes(), expected);
    }

    // hash_coin_ids: single -> sha256(id); multiple -> sort descending, concat, sha256.
    #[test]
    fn hash_coin_ids_matches_chia() {
        let x = leaf(0x11);
        let y = leaf(0x22);
        // Single: no prefix, just sha256 of the id.
        assert_eq!(hash_coin_ids(&[x]), super::hash_256(x));
        // Multiple: descending sort (y before x), concat, sha256 — regardless of input order.
        let mut buf = Vec::new();
        buf.extend_from_slice(&y);
        buf.extend_from_slice(&x);
        let expected = super::hash_256(buf);
        assert_eq!(hash_coin_ids(&[x, y]), expected);
        assert_eq!(hash_coin_ids(&[y, x]), expected);
    }
}

#[cfg(test)]
mod agg_sig_tests {
    use super::validate_block_aggregate_signature;
    use crate::blockchain::sized_bytes::Bytes96;
    use crate::blockchain::spend_bundle_conditions::SpendBundleConditions;
    use crate::blockchain::unsized_bytes::UnsizedBytes;
    use crate::clvm::bls_bindings::sign;
    use crate::consensus::constants::MAINNET;
    use crate::traits::SizedBytes;
    use blst::min_pk::SecretKey;

    // AGG_SIG_UNSAFE pairs live on the bundle conditions, not on any spend. A bundle
    // whose only signature is a real AGG_SIG_UNSAFE must verify.
    #[test]
    fn bundle_level_agg_sig_unsafe_pairs_join_the_aggregate() {
        let sk = SecretKey::key_gen_v3(&[7u8; 32], &[]).expect("sk");
        let msg = b"agg sig unsafe message".to_vec();
        let sig = sign(&sk, &msg);
        let mut conds = SpendBundleConditions::default();
        conds.agg_sig_unsafe.push((
            UnsizedBytes::new(sk.sk_to_pk().to_bytes().to_vec()),
            UnsizedBytes::new(msg),
        ));
        let aggregate = Bytes96::parse(&sig.to_bytes()).expect("sig bytes");
        validate_block_aggregate_signature(&conds, &aggregate, &MAINNET)
            .expect("an AGG_SIG_UNSAFE-only bundle verifies");
    }
}

#[cfg(test)]
mod producer_tests {
    use super::{
        BlockGeneratorFlags, BlockGeneratorInput, execute_block_generator_result,
        simple_solution_generator,
    };
    use crate::blockchain::coin::Coin;
    use crate::blockchain::coin_spend::CoinSpend;
    use crate::blockchain::sized_bytes::Bytes32;
    use crate::clvm::parser::sexp_to_bytes;
    use crate::clvm::sexp::{AtomBuf, PairBuf, SExp};
    use crate::consensus::constants::MAINNET;
    use crate::traits::SizedBytes;
    use num_bigint::BigInt;
    use std::sync::Arc;

    fn atom(bytes: Vec<u8>) -> SExp<'static> {
        SExp::Atom(AtomBuf::Owned(Arc::new(bytes)))
    }

    // The producer's output MUST round-trip through our own validator: assemble a plain generator
    // from a coin spend whose puzzle creates a coin, run it through execute_block_generator_result,
    // and get exactly that spend + created coin back.
    #[test]
    fn simple_generator_round_trips_through_our_validator() {
        let created_ph = Bytes32::from([0x11u8; 32]);
        let created_amount = 500u64;

        // puzzle = (q . ((51 created_ph created_amount)))  — returns one CREATE_COIN, any solution.
        let create_coin = SExp::from(vec![
            SExp::from(&BigInt::from(51u8)), // CREATE_COIN opcode
            atom(created_ph.bytes().to_vec()),
            SExp::from(&BigInt::from(created_amount)),
        ]);
        let conditions = SExp::from(vec![create_coin]);
        let puzzle = SExp::Pair(PairBuf::Owned((
            Arc::new(atom(vec![1u8])),
            Arc::new(conditions),
        )));
        let puzzle_reveal = sexp_to_bytes(&puzzle).expect("serialize puzzle");
        let solution = sexp_to_bytes(&atom(vec![])).expect("serialize nil solution");

        let coin = Coin {
            parent_coin_info: Bytes32::from([0x22u8; 32]),
            puzzle_hash: Bytes32::from([0u8; 32]), // unused by the generator (derived from reveal)
            amount: 1000,
        };
        let spend = CoinSpend {
            coin,
            puzzle_reveal,
            solution,
        };

        let generator = simple_solution_generator(&[spend]).expect("assemble generator");
        // It is a SIMPLE generator: (q . …) => 0xff 0x01 …
        assert!(
            generator.as_ref().starts_with(&[0xff, 0x01]),
            "expected a simple (quoted) generator"
        );

        // Run it through OUR validator on the simple path (height >= hard fork => simple_generator).
        let height = MAINNET.hard_fork_height + 4000;
        let input = BlockGeneratorInput {
            transactions_generator: generator,
            generator_refs: Vec::new(),
            constants: MAINNET,
            height,
            flags: BlockGeneratorFlags::for_height(&MAINNET, height),
        };
        assert!(
            input.flags.simple_generator,
            "test must exercise the simple path"
        );

        let conds =
            execute_block_generator_result(&input).expect("generator runs in our validator");
        assert_eq!(conds.spends.len(), 1, "exactly the one spend we assembled");
        let created: Vec<_> = conds.spends[0]
            .create_coin
            .iter()
            .filter(|c| c.puzzle_hash == created_ph && c.amount == created_amount)
            .collect();
        assert_eq!(
            created.len(),
            1,
            "the assembled spend created the expected coin ({created_ph}, {created_amount})"
        );
    }

    // Byte-parity vector for the plain generator: the reference bytes carry the spends in
    // REVERSE input order; `solution_generator_from_coin_spends` must reproduce them for
    // the same ordered input.
    #[test]
    fn chia_rs_solution_generator_byte_parity() {
        use crate::consensus::block_generator::solution_generator_from_coin_spends;

        let puzzle1 = hex::decode(concat!(
            "ff02ffff01ff02ffff01ff02ffff03ff0bffff01ff02ffff03ffff09ff05ffff",
            "1dff0bffff1effff0bff0bffff02ff06ffff04ff02ffff04ff17ff8080808080",
            "808080ffff01ff02ff17ff2f80ffff01ff088080ff0180ffff01ff04ffff04ff",
            "04ffff04ff05ffff04ffff02ff06ffff04ff02ffff04ff17ff80808080ff8080",
            "8080ffff02ff17ff2f808080ff0180ffff04ffff01ff32ff02ffff03ffff07ff",
            "0580ffff01ff0bffff0102ffff02ff06ffff04ff02ffff04ff09ff80808080ff",
            "ff02ff06ffff04ff02ffff04ff0dff8080808080ffff01ff0bffff0101ff0580",
            "80ff0180ff018080ffff04ffff01b08cf5533a94afae0f4613d3ea565e47abc5",
            "373415967ef5824fd009c602cb629e259908ce533c21de7fd7a68eb96c52d0ff",
            "018080"
        ))
        .unwrap();
        let solution1 = hex::decode(concat!(
            "ff80ffff01ffff3dffa080115c1c71035a2cd60a49499fb9e5cb55be8d6e25e8",
            "680bfc0409b7acaeffd48080ff8080"
        ))
        .unwrap();
        let puzzle2 = hex::decode(concat!(
            "ff01ffff33ffa01b7ab2079fa635554ad9bd4812c622e46ee3b1875a7813afba",
            "127bb0cc9794f9ff887f808e9291e6c00080ffff33ffa06f184a7074c925ef86",
            "88ce56941eb8929be320265f824ec7e351356cc745d38aff887f808e9291e6c0",
            "008080"
        ))
        .unwrap();
        let solution2 = hex::decode("80").unwrap();

        let coin1 = Coin {
            parent_coin_info: Bytes32::parse(
                &hex::decode("ccd5bb71183532bff220ba46c268991a00000000000000000000000000036840")
                    .unwrap(),
            )
            .unwrap(),
            puzzle_hash: Bytes32::parse(
                &hex::decode("fcc78a9e396df6ceebc217d2446bc016e0b3d5922fb32e5783ec5a85d490cfb6")
                    .unwrap(),
            )
            .unwrap(),
            amount: 1_750_000_000_000,
        };
        let coin2 = Coin {
            parent_coin_info: Bytes32::parse(
                &hex::decode("ccd5bb71183532bff220ba46c268991a00000000000000000000000000000000")
                    .unwrap(),
            )
            .unwrap(),
            puzzle_hash: Bytes32::parse(
                &hex::decode("d23da14695a188ae5708dd152263c4db883eb27edeb936178d4d988b8f3ce5fc")
                    .unwrap(),
            )
            .unwrap(),
            amount: 18_375_000_000_000_000_000,
        };

        let spends = [
            CoinSpend {
                coin: coin1,
                puzzle_reveal: crate::clvm::program::SerializedProgram::from(puzzle1.clone()),
                solution: crate::clvm::program::SerializedProgram::from(solution1.clone()),
            },
            CoinSpend {
                coin: coin2,
                puzzle_reveal: crate::clvm::program::SerializedProgram::from(puzzle2.clone()),
                solution: crate::clvm::program::SerializedProgram::from(solution2.clone()),
            },
        ];

        let generator = solution_generator_from_coin_spends(&spends).expect("assemble");
        let expected = hex::decode(
            [
                "ff01ffffffa0",
                "ccd5bb71183532bff220ba46c268991a00000000000000000000000000000000",
                "ff",
                "ff01ffff33ffa01b7ab2079fa635554ad9bd4812c622e46ee3b1875a7813afba",
                "127bb0cc9794f9ff887f808e9291e6c00080ffff33ffa06f184a7074c925ef86",
                "88ce56941eb8929be320265f824ec7e351356cc745d38aff887f808e9291e6c0",
                "008080",
                "ff8900ff011d2523cd8000ff",
                "80",
                "80ffffa0",
                "ccd5bb71183532bff220ba46c268991a00000000000000000000000000036840",
                "ff",
                "ff02ffff01ff02ffff01ff02ffff03ff0bffff01ff02ffff03ffff09ff05ffff",
                "1dff0bffff1effff0bff0bffff02ff06ffff04ff02ffff04ff17ff8080808080",
                "808080ffff01ff02ff17ff2f80ffff01ff088080ff0180ffff01ff04ffff04ff",
                "04ffff04ff05ffff04ffff02ff06ffff04ff02ffff04ff17ff80808080ff8080",
                "8080ffff02ff17ff2f808080ff0180ffff04ffff01ff32ff02ffff03ffff07ff",
                "0580ffff01ff0bffff0102ffff02ff06ffff04ff02ffff04ff09ff80808080ff",
                "ff02ff06ffff04ff02ffff04ff0dff8080808080ffff01ff0bffff0101ff0580",
                "80ff0180ff018080ffff04ffff01b08cf5533a94afae0f4613d3ea565e47abc5",
                "373415967ef5824fd009c602cb629e259908ce533c21de7fd7a68eb96c52d0ff",
                "018080",
                "ff8601977420dc00ff",
                "ff80ffff01ffff3dffa080115c1c71035a2cd60a49499fb9e5cb55be8d6e25e8",
                "680bfc0409b7acaeffd48080ff8080",
                "808080",
            ]
            .concat(),
        )
        .unwrap();
        assert_eq!(
            generator.as_ref(),
            expected.as_slice(),
            "solution_generator_from_coin_spends must emit chia_rs solution_generator's exact bytes"
        );
    }

    // Byte-parity vector for the compressed generator: same two spends as the plain vector
    // above; the compressed form replaces the two repeated puzzle-hash subtrees with
    // `0xfe`-prefixed back-references. With the plain vector this proves the two forms
    // encode the SAME program.
    #[test]
    fn compressed_generator_matches_chia_rs_backrefs() {
        use crate::consensus::block_generator::compressed_solution_generator_from_coin_spends;

        let spends = chia_rs_backref_fixture_spends();
        let generator =
            compressed_solution_generator_from_coin_spends(&spends).expect("assemble compressed");
        let expected = hex::decode(
            [
                "ff01ffffffa0",
                "ccd5bb71183532bff220ba46c268991a00000000000000000000000000000000",
                "ff",
                "ff01ffff33ffa01b7ab2079fa635554ad9bd4812c622e46ee3b1875a7813afba",
                "127bb0cc9794f9ff887f808e9291e6c00080ffff33ffa06f184a7074c925ef86",
                "88ce56941eb8929be320265f824ec7e351356cc745d38a",
                "fe3b",
                "80ff8900ff011d2523cd8000ff8080ffffa0",
                "ccd5bb71183532bff220ba46c268991a00000000000000000000000000036840",
                "ff",
                "ff02ffff01ff02ffff01ff02ffff03ff0bffff01ff02ffff03ffff09ff05ffff",
                "1dff0bffff1effff0bff0bffff02ff06ffff04ff02ffff04ff17ff8080808080",
                "808080ffff01ff02ff17ff2f80ffff01ff088080ff0180ffff01ff04ffff04ff",
                "04ffff04ff05ffff04ff",
                "fe8401",
                "6b6b7fff80808080ff",
                "fe820d",
                "b78080",
                "ff0180",
                "ffff04ffff01ff32ff02ffff03ffff07ff0580ffff01ff0bffff0102ffff02ff",
                "06ffff04ff02ffff04ff09ff80808080ffff02ff06ffff04ff02ffff04ff0dff",
                "8080808080ffff01ff0bffff0101",
                "ff0580",
                "80ff0180",
                "ff0180",
                "80ffff04ffff01b08cf5533a94afae0f4613d3ea565e47abc5373415967ef582",
                "4fd009c602cb629e259908ce533c21de7fd7a68eb96c52d0",
                "ff0180",
                "80ff8601977420dc00ffff80ffff01ffff3dffa080115c1c71035a2cd60a4949",
                "9fb9e5cb55be8d6e25e8680bfc0409b7acaeffd48080ff8080808080",
            ]
            .concat(),
        )
        .unwrap();
        assert_eq!(
            generator.as_ref(),
            expected.as_slice(),
            "compressed_solution_generator must emit chia_rs solution_generator_backrefs's exact bytes"
        );
    }

    // The two test spends shared by the plain and compressed byte-parity vectors.
    fn chia_rs_backref_fixture_spends() -> Vec<CoinSpend> {
        use crate::clvm::program::SerializedProgram;
        let puzzle1 = hex::decode(concat!(
            "ff02ffff01ff02ffff01ff02ffff03ff0bffff01ff02ffff03ffff09ff05ffff",
            "1dff0bffff1effff0bff0bffff02ff06ffff04ff02ffff04ff17ff8080808080",
            "808080ffff01ff02ff17ff2f80ffff01ff088080ff0180ffff01ff04ffff04ff",
            "04ffff04ff05ffff04ffff02ff06ffff04ff02ffff04ff17ff80808080ff8080",
            "8080ffff02ff17ff2f808080ff0180ffff04ffff01ff32ff02ffff03ffff07ff",
            "0580ffff01ff0bffff0102ffff02ff06ffff04ff02ffff04ff09ff80808080ff",
            "ff02ff06ffff04ff02ffff04ff0dff8080808080ffff01ff0bffff0101ff0580",
            "80ff0180ff018080ffff04ffff01b08cf5533a94afae0f4613d3ea565e47abc5",
            "373415967ef5824fd009c602cb629e259908ce533c21de7fd7a68eb96c52d0ff",
            "018080"
        ))
        .unwrap();
        let solution1 = hex::decode(concat!(
            "ff80ffff01ffff3dffa080115c1c71035a2cd60a49499fb9e5cb55be8d6e25e8",
            "680bfc0409b7acaeffd48080ff8080"
        ))
        .unwrap();
        let puzzle2 = hex::decode(concat!(
            "ff01ffff33ffa01b7ab2079fa635554ad9bd4812c622e46ee3b1875a7813afba",
            "127bb0cc9794f9ff887f808e9291e6c00080ffff33ffa06f184a7074c925ef86",
            "88ce56941eb8929be320265f824ec7e351356cc745d38aff887f808e9291e6c0",
            "008080"
        ))
        .unwrap();
        let solution2 = hex::decode("80").unwrap();
        let coin1 = Coin {
            parent_coin_info: Bytes32::parse(
                &hex::decode("ccd5bb71183532bff220ba46c268991a00000000000000000000000000036840")
                    .unwrap(),
            )
            .unwrap(),
            puzzle_hash: Bytes32::parse(
                &hex::decode("fcc78a9e396df6ceebc217d2446bc016e0b3d5922fb32e5783ec5a85d490cfb6")
                    .unwrap(),
            )
            .unwrap(),
            amount: 1_750_000_000_000,
        };
        let coin2 = Coin {
            parent_coin_info: Bytes32::parse(
                &hex::decode("ccd5bb71183532bff220ba46c268991a00000000000000000000000000000000")
                    .unwrap(),
            )
            .unwrap(),
            puzzle_hash: Bytes32::parse(
                &hex::decode("d23da14695a188ae5708dd152263c4db883eb27edeb936178d4d988b8f3ce5fc")
                    .unwrap(),
            )
            .unwrap(),
            amount: 18_375_000_000_000_000_000,
        };
        vec![
            CoinSpend {
                coin: coin1,
                puzzle_reveal: SerializedProgram::from(puzzle1),
                solution: SerializedProgram::from(solution1),
            },
            CoinSpend {
                coin: coin2,
                puzzle_reveal: SerializedProgram::from(puzzle2),
                solution: SerializedProgram::from(solution2),
            },
        ]
    }

    // The compressed generator must (a) round-trip through the back-ref DECODER to the SAME program
    // the plain form encodes, (b) be strictly smaller when a subtree repeats, and (c) run through OUR
    // validator to the IDENTICAL cost and conditions as the plain form. Built from three spends of
    // the SAME create-coin puzzle (distinct parents), so the 34-byte puzzle reveal repeats and
    // compresses — the packing lever, proven end to end against our own validator.
    #[test]
    fn compressed_generator_round_trips_and_validates_like_plain() {
        use crate::clvm::parser::{sexp_from_bytes_backrefs, sexp_to_bytes};
        use crate::consensus::block_generator::{
            compressed_solution_generator_from_coin_spends, solution_generator_from_coin_spends,
        };
        use std::io::Cursor;

        let created_ph = Bytes32::from([0x11u8; 32]);
        let created_amount = 500u64;
        // puzzle = (q . ((51 created_ph created_amount))) — one CREATE_COIN, any solution. Identical
        // across every spend, so its serialized reveal is a repeated subtree the back-ref serializer
        // deduplicates.
        let create_coin = SExp::from(vec![
            SExp::from(&BigInt::from(51u8)),
            atom(created_ph.bytes().to_vec()),
            SExp::from(&BigInt::from(created_amount)),
        ]);
        let conditions = SExp::from(vec![create_coin]);
        let puzzle = SExp::Pair(PairBuf::Owned((
            Arc::new(atom(vec![1u8])),
            Arc::new(conditions),
        )));
        let puzzle_reveal = sexp_to_bytes(&puzzle).expect("serialize puzzle");
        let solution = sexp_to_bytes(&atom(vec![])).expect("serialize nil solution");

        let spends: Vec<CoinSpend> = (0u8..3)
            .map(|i| CoinSpend {
                coin: Coin {
                    parent_coin_info: Bytes32::from([0x40u8 + i; 32]),
                    puzzle_hash: Bytes32::from([0u8; 32]),
                    amount: 1000 + u64::from(i),
                },
                puzzle_reveal: puzzle_reveal.clone(),
                solution: solution.clone(),
            })
            .collect();

        let plain = solution_generator_from_coin_spends(&spends).expect("plain");
        let compressed =
            compressed_solution_generator_from_coin_spends(&spends).expect("compressed");

        // (d) the packing lever: repeated puzzle reveal ⇒ strictly smaller.
        assert!(
            compressed.as_ref().len() < plain.as_ref().len(),
            "compressed ({}) must be smaller than plain ({}) when the puzzle repeats",
            compressed.as_ref().len(),
            plain.as_ref().len()
        );

        // (a) round-trip: both decode (via the back-ref decoder validation uses) to the same tree.
        let plain_tree =
            sexp_from_bytes_backrefs(&mut Cursor::new(plain.as_ref())).expect("decode plain");
        let compressed_tree = sexp_from_bytes_backrefs(&mut Cursor::new(compressed.as_ref()))
            .expect("decode compressed");
        assert_eq!(
            compressed_tree, plain_tree,
            "compressed must decode to the identical program"
        );

        // (c) validates to identical cost + conditions under our own validator.
        let height = MAINNET.hard_fork_height + 4000;
        let run = |prog: crate::clvm::program::SerializedProgram| {
            execute_block_generator_result(&BlockGeneratorInput {
                transactions_generator: prog,
                generator_refs: Vec::new(),
                constants: MAINNET,
                height,
                flags: BlockGeneratorFlags::for_height(&MAINNET, height),
            })
            .expect("generator runs in our validator")
        };
        let plain_len = plain.as_ref().len() as u64;
        let compressed_len = compressed.as_ref().len() as u64;
        let plain_conds = run(plain);
        let compressed_conds = run(compressed);
        // (c) same program ⇒ same conditions; the ONLY cost difference is the byte cost, which drops
        // by exactly the serialized-size saving × cost_per_byte. This is the packing win made
        // concrete: fewer bytes ⇒ lower block cost ⇒ more room under MAX_BLOCK_COST_CLVM.
        assert!(
            compressed_conds.cost < plain_conds.cost,
            "compressed cost {} must be below plain cost {}",
            compressed_conds.cost,
            plain_conds.cost
        );
        assert_eq!(
            plain_conds.cost - compressed_conds.cost,
            (plain_len - compressed_len) * MAINNET.cost_per_byte,
            "the whole cost delta is the byte-cost saving; execution + condition cost is unchanged"
        );
        assert_eq!(
            compressed_conds.spends.len(),
            plain_conds.spends.len(),
            "same spends recovered"
        );
        assert_eq!(compressed_conds.spends.len(), 3, "all three spends present");
    }
}
