use crate::blockchain::block_record::BlockRecord;
use crate::blockchain::coin::Coin;
use crate::blockchain::foliage::Foliage;
use crate::blockchain::foliage_block_data::FoliageBlockData;
use crate::blockchain::foliage_transaction_block::FoliageTransactionBlock;
use crate::blockchain::full_block::FullBlock;
use crate::blockchain::pool_target::PoolTarget;
use crate::blockchain::proof_of_space::ProofOfSpace;
use crate::blockchain::reward_chain_block::RewardChainBlock;
use crate::blockchain::reward_chain_block_unfinished::RewardChainBlockUnfinished;
use crate::blockchain::sized_bytes::{Bytes32, Bytes48, Bytes96};
use crate::blockchain::subslot_bundle::SubSlotBundle;
use crate::blockchain::transactions_info::TransactionsInfo;
use crate::blockchain::unfinished_block::UnfinishedBlock;
use crate::blockchain::vdf_info::VdfInfo;
use crate::blockchain::vdf_proof::VdfProof;
use crate::clvm::program::SerializedProgram;
use crate::consensus::block_filter::chia_block_filter;
use crate::consensus::block_generator::{
    canonical_additions_root, canonical_removals_root, transactions_generator_refs_root,
    transactions_generator_root, transactions_info_hash,
};
use crate::consensus::block_rewards::{calculate_base_farmer_reward, calculate_pool_reward};
use crate::consensus::coinbase::{create_farmer_coin, create_pool_coin};
use crate::consensus::constants::ConsensusConstants;
use crate::errors::ChiaError;
use crate::traits::SizedBytes;
use crate::utils::hash_256;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};

/// The BLS12-381 G2 point at infinity, serialized *compressed* as `0xc0` followed by 95
/// zero bytes — the `TransactionsInfo.aggregated_signature` for a transaction block
/// carrying no spend bundle.
///
/// NOTE — parity trap: this is NOT `Bytes96::default()` (all zeros). An all-zero G2 is not a valid
/// point encoding, and the producer/validator boundary treats infinity as `0xc0..`: see
/// `block_generator.rs::validate_block_aggregate_signature`, which accepts the empty-signature block
/// only when `aggregated_signature == [0xc0, 0x00 * 95]`.
///
/// This is also the correct PLACEHOLDER for the two foliage plot signatures at declare time,
/// before the farmer signs them; the real signatures arrive on `SignedValues`.
/// See [`FarmerSignatures`].
#[must_use]
pub fn g2_infinity() -> Bytes96 {
    let mut buf = [0u8; 96];
    buf[0] = 0xc0;
    Bytes96::new(buf)
}

/// `extension_data` is farmer-chosen entropy whose only consensus commitment is via the
/// plot signature over `FoliageBlockData::hash`; any 32-byte value is valid. It is derived
/// deterministically as `sha256(seed)`.
#[must_use]
fn extension_data_from_seed(seed: &[u8]) -> Bytes32 {
    Bytes32::new(hash_256(seed))
}

/// The four BLS plot signatures the FARMER supplies for a block it declared. In the live
/// node flow the producer never holds the plot secret key: the two signage-point
/// signatures arrive on the `DeclareProofOfSpace` message
/// (`challenge_chain_sp_signature`/`reward_chain_sp_signature`), and the two foliage
/// signatures arrive later on the `SignedValues` reply, after the node sends the farmer
/// the two foliage hashes to sign (`RequestSignedValues`).
///
/// `foliage_transaction_block_signature` is meaningful only for a transaction block
/// (`is_transaction_block == true`); it is ignored otherwise.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct FarmerSignatures {
    pub challenge_chain_sp_signature: Bytes96,
    pub reward_chain_sp_signature: Bytes96,
    pub foliage_block_data_signature: Bytes96,
    pub foliage_transaction_block_signature: Bytes96,
}

/// The candidate's total iters at its infusion point. When the signage point is in the
/// overflow region of its sub-slot (`sp_iters > ip_iters`, i.e. the SP is past where the
/// infusion lands so infusion spills into the NEXT sub-slot) one `sub_slot_iters` is
/// added; otherwise the infusion is in the same sub-slot.
///
/// This is the value [`create_unfinished_block`] takes as `infusion_point_total_iters` and
/// writes verbatim into `RewardChainBlockUnfinished.total_iters`. The caller (the declare
/// handler) must compute it here so the overflow case is handled at exactly one place.
/// All sums are `u128`.
#[must_use]
pub fn calculate_infusion_point_total_iters(
    sub_slot_start_total_iters: u128,
    sp_iters: u64,
    ip_iters: u64,
    sub_slot_iters: u64,
) -> u128 {
    let overflow = sp_iters > ip_iters;
    sub_slot_start_total_iters
        + u128::from(ip_iters)
        + if overflow {
            u128::from(sub_slot_iters)
        } else {
            0
        }
}

/// A single reward claim incorporated into a transaction block: the pool + farmer coins
/// minted for one prior block.
///
/// The claims come from walking the block records backwards from the previous block to the
/// most recent transaction block (and the non-transaction blocks between it and the one
/// before); the caller supplies the already-walked list. `fees` is the claimed block's own
/// fee total, added to the farmer reward for the *previous transaction block only* — pass
/// `fees == 0` for the skipped non-transaction blocks.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct RewardBlockClaim {
    pub height: u32,
    pub pool_puzzle_hash: Bytes32,
    pub farmer_puzzle_hash: Bytes32,
    pub fees: u64,
}

/// The transaction payload of a block being produced.
/// `additions`/`removals` are the SPEND coins only — reward coins are added separately from
/// [`RewardBlockClaim`]s. When this is `None` (no spend bundle), `create_foliage` still emits a
/// `TransactionsInfo` for a transaction block, with the generator-root / refs-root / signature / cost
/// defaults documented in [`create_foliage`].
#[derive(Clone, Debug)]
pub struct BlockTransactions {
    pub program: SerializedProgram,
    pub block_refs: Vec<u32>,
    pub additions: Vec<Coin>,
    pub removals: Vec<Coin>,
    pub aggregated_signature: Bytes96,
    pub cost: u64,
}

/// The foliage-assembly output:
/// `(Foliage, FoliageTransactionBlock | None, TransactionsInfo | None)`. The two trailing members are
/// `Some` iff `is_transaction_block`.
#[derive(Clone, Debug)]
pub struct FoliageResult {
    pub foliage: Foliage,
    pub foliage_transaction_block: Option<FoliageTransactionBlock>,
    pub transactions_info: Option<TransactionsInfo>,
}

/// Block fee = Σ(removal amounts) − Σ(addition amounts).
/// Sums are taken in `u128` (a block holds ≤ `MAX_SPENDS_PER_BLOCK` coins, each a `u64`, so the sum
/// cannot overflow `u128`). Errors when the
/// additions exceed the removals (a would-be minting block) or the fee exceeds `u64`.
///
/// # Errors
/// [`ChiaError::InvalidBlockFeeAmount`] when additions exceed removals or the fee overflows `u64`.
pub fn compute_block_fee(additions: &[Coin], removals: &[Coin]) -> Result<u64, ChiaError> {
    let removal_amount: u128 = removals.iter().map(|c| u128::from(c.amount)).sum();
    let addition_amount: u128 = additions.iter().map(|c| u128::from(c.amount)).sum();
    let fee = removal_amount
        .checked_sub(addition_amount)
        .ok_or(ChiaError::InvalidBlockFeeAmount)?;
    u64::try_from(fee).map_err(|_| ChiaError::InvalidBlockFeeAmount)
}

/// Assemble the foliage + reward coins for a block being produced. Given the
/// unfinished reward block's hash, the reward claims, and (optionally) a transaction payload, this
/// builds:
///   * the pool + farmer reward coins for every [`RewardBlockClaim`] (via
///     `coinbase::{create_pool_coin, create_farmer_coin}` + `block_rewards::calculate_*`),
///   * [`FoliageBlockData`],
///   * [`Foliage`] with real BLS plot signatures: `foliage_block_data_signature` over
///     `foliage_data.get_hash()` and — for a transaction block — `foliage_transaction_block_signature`
///     over `foliage_transaction_block.get_hash()`, both produced by `plot_signer`,
///   * for a transaction block, [`TransactionsInfo`] and [`FoliageTransactionBlock`].
///
/// This STOPS before assembling the `RewardChainBlockUnfinished` / `UnfinishedBlock`: it
/// therefore takes `reward_block_unfinished_hash`
/// directly rather than the whole object. Two more inputs are pre-resolved for the same reason,
/// being derived from the block records:
///   * `is_transaction_block` — the previous-transaction-block result (always `true` for
///     genesis);
///   * `prev_block_hash` / `prev_transaction_block_hash` — the genesis challenge at height 0,
///     else the respective ancestor's `header_hash`.
///
/// Field order and hashing are fixed so the resulting `foliage_transaction_block_hash` /
/// `transactions_info_hash` match: reward coins → `tx_additions` → `additions_root`/`removals_root`
/// (reusing the `block_generator::canonical_*` merkle helpers) → `TransactionsInfo` →
/// `FoliageTransactionBlock` → its hash feeding `Foliage`.
///
/// The BIP158 transaction filter is built internally via [`chia_block_filter`], so
/// `filter_hash = std_hash(chia_block_filter(...))` for both the empty (genesis /
/// no-tx-content) case and non-empty transaction filters.
///
/// `plot_public_key` is the proof-of-space's aggregate plot public key
/// (`reward_block_unfinished.proof_of_space.plot_public_key`), forwarded to `plot_signer` at each
/// signing point.
///
/// `plot_signer`: given a 32-byte message and the plot public key, it returns the BLS G2
/// element (AugScheme) plot signature. The producer never holds the plot secret key — the
/// farmer/harvester owns the key material (see the module note on taproot/pool aggregation).
///
/// # Errors
/// [`ChiaError::BadFarmerCoinAmount`] on farmer reward + fees overflow; [`ChiaError::BadAdditionRoot`]
/// if the additions merkle set fails to build; [`ChiaError::InvalidFoliageBlockHash`] if the
/// `FoliageBlockData` / `FoliageTransactionBlock` fail to serialize for hashing (the plot-signature
/// message); and the propagated errors of
/// [`compute_block_fee`]/`transactions_*_root`/[`transactions_info_hash`].
/// How the two foliage plot signatures are produced. `Signer` is the local-signing
/// callback model used by tests and the local-signing path; `Precomputed`
/// is the live-node farmer-supplied model — the node inserts signatures it received from the farmer
/// (placeholders at declare time, real ones spliced in at `signed_values`), never holding the plot
/// key. See [`FarmerSignatures`].
enum FoliageSigning<'a> {
    Signer {
        plot_public_key: Bytes48,
        sign: &'a dyn Fn(Bytes32, &Bytes48) -> Bytes96,
    },
    Precomputed {
        foliage_block_data_signature: Bytes96,
        foliage_transaction_block_signature: Bytes96,
    },
}

/// [`create_foliage`] with a local `plot_signer`. See the
/// module-level docs on the signer seam; forwards to [`create_foliage_inner`] with
/// [`FoliageSigning::Signer`].
///
/// # Errors
/// See [`create_foliage_inner`].
#[allow(clippy::too_many_arguments)]
pub fn create_foliage(
    constants: &ConsensusConstants,
    reward_block_unfinished_hash: Bytes32,
    height: u32,
    is_transaction_block: bool,
    reward_claims: &[RewardBlockClaim],
    transactions: Option<&BlockTransactions>,
    prev_block_hash: Bytes32,
    prev_transaction_block_hash: Bytes32,
    pool_target: PoolTarget,
    pool_signature: Option<Bytes96>,
    plot_public_key: Bytes48,
    farmer_reward_puzzle_hash: Bytes32,
    timestamp: u64,
    seed: &[u8],
    plot_signer: impl Fn(Bytes32, &Bytes48) -> Bytes96,
) -> Result<FoliageResult, ChiaError> {
    create_foliage_inner(
        constants,
        reward_block_unfinished_hash,
        height,
        is_transaction_block,
        reward_claims,
        transactions,
        prev_block_hash,
        prev_transaction_block_hash,
        pool_target,
        pool_signature,
        farmer_reward_puzzle_hash,
        timestamp,
        seed,
        &FoliageSigning::Signer {
            plot_public_key,
            sign: &plot_signer,
        },
    )
}

/// [`create_foliage`] with FARMER-supplied foliage signatures instead of a local signer — the live
/// node path (the candidate foliage is built with infinity placeholders, then
/// `signed_values` splices the real signatures in). Pass
/// [`g2_infinity`] placeholders at declare time; the real values are spliced later via
/// [`splice_farmer_foliage_signatures`] (or passed here directly when both are already known).
/// `foliage_transaction_block_signature` is used only when `is_transaction_block`.
///
/// The two foliage HASHES the farmer must sign are recoverable from the returned [`FoliageResult`]:
/// `result.foliage.foliage_block_data.hash()` and `result.foliage.foliage_transaction_block_hash`.
///
/// # Errors
/// See [`create_foliage_inner`].
#[allow(clippy::too_many_arguments)]
pub fn create_foliage_with_sigs(
    constants: &ConsensusConstants,
    reward_block_unfinished_hash: Bytes32,
    height: u32,
    is_transaction_block: bool,
    reward_claims: &[RewardBlockClaim],
    transactions: Option<&BlockTransactions>,
    prev_block_hash: Bytes32,
    prev_transaction_block_hash: Bytes32,
    pool_target: PoolTarget,
    pool_signature: Option<Bytes96>,
    farmer_reward_puzzle_hash: Bytes32,
    timestamp: u64,
    seed: &[u8],
    foliage_block_data_signature: Bytes96,
    foliage_transaction_block_signature: Bytes96,
) -> Result<FoliageResult, ChiaError> {
    create_foliage_inner(
        constants,
        reward_block_unfinished_hash,
        height,
        is_transaction_block,
        reward_claims,
        transactions,
        prev_block_hash,
        prev_transaction_block_hash,
        pool_target,
        pool_signature,
        farmer_reward_puzzle_hash,
        timestamp,
        seed,
        &FoliageSigning::Precomputed {
            foliage_block_data_signature,
            foliage_transaction_block_signature,
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn create_foliage_inner(
    constants: &ConsensusConstants,
    reward_block_unfinished_hash: Bytes32,
    height: u32,
    is_transaction_block: bool,
    reward_claims: &[RewardBlockClaim],
    transactions: Option<&BlockTransactions>,
    prev_block_hash: Bytes32,
    prev_transaction_block_hash: Bytes32,
    pool_target: PoolTarget,
    pool_signature: Option<Bytes96>,
    farmer_reward_puzzle_hash: Bytes32,
    timestamp: u64,
    seed: &[u8],
    signing: &FoliageSigning,
) -> Result<FoliageResult, ChiaError> {
    // extension_data makes blocks differ by header hash; see the note on the fn
    let extension_data = extension_data_from_seed(seed);

    // FoliageBlockData(reward_block_unfinished.get_hash(), pool_target, pool_target_signature,
    // farmer_reward_puzzlehash, extension_data). The plot signature over foliage_data.get_hash()
    // is not a field of FoliageBlockData itself.
    let foliage_data = FoliageBlockData {
        unfinished_reward_block_hash: reward_block_unfinished_hash,
        pool_target,
        pool_signature,
        farmer_reward_puzzle_hash,
        extension_data,
    };

    // foliage_block_data_signature over foliage_data.get_hash() is always signed (tx block
    // or not). In the Precomputed (farmer-supplied) path this is the signature the farmer
    // returned — or its g2_infinity() placeholder while awaiting SignedValues.
    let foliage_block_data_hash = foliage_data
        .hash()
        .map_err(|_| ChiaError::InvalidFoliageBlockHash)?;
    let foliage_block_data_signature = match signing {
        FoliageSigning::Signer {
            plot_public_key,
            sign,
        } => sign(foliage_block_data_hash, plot_public_key),
        FoliageSigning::Precomputed {
            foliage_block_data_signature,
            ..
        } => *foliage_block_data_signature,
    };

    let mut foliage_transaction_block: Option<FoliageTransactionBlock> = None;
    let mut transactions_info: Option<TransactionsInfo> = None;
    let mut foliage_transaction_block_hash: Option<Bytes32> = None;
    let mut foliage_transaction_block_signature: Option<Bytes96> = None;

    if is_transaction_block {
        // reward_claims_incorporated += [pool_coin, farmer_coin] per walked block, only when
        // height > 0 (the genesis transaction block claims nothing). Preserve the [pool, farmer] order.
        let mut reward_claims_incorporated: Vec<Coin> = Vec::new();
        if height > 0 {
            for claim in reward_claims {
                let pool_coin = create_pool_coin(
                    claim.height,
                    claim.pool_puzzle_hash,
                    calculate_pool_reward(claim.height),
                    constants.genesis_challenge,
                );
                // calculate_base_farmer_reward(curr.height) + curr.fees, overflow-checked.
                let farmer_amount = calculate_base_farmer_reward(claim.height)
                    .checked_add(claim.fees)
                    .ok_or(ChiaError::BadFarmerCoinAmount)?;
                let farmer_coin = create_farmer_coin(
                    claim.height,
                    claim.farmer_puzzle_hash,
                    farmer_amount,
                    constants.genesis_challenge,
                );
                reward_claims_incorporated.push(pool_coin);
                reward_claims_incorporated.push(farmer_coin);
            }
        }

        // tx_additions (reward coins first, then spend additions) and byte_array_tx (each
        // addition's puzzle_hash, then each removal's coin name) are built in this exact order.
        let mut tx_additions: Vec<Coin> = Vec::with_capacity(reward_claims_incorporated.len());
        let mut tx_removal_names: Vec<Bytes32> = Vec::new();
        let mut byte_array_tx: Vec<Vec<u8>> = Vec::new();
        for coin in &reward_claims_incorporated {
            tx_additions.push(*coin);
            byte_array_tx.push(coin.puzzle_hash.bytes().to_vec());
        }

        // TransactionsInfo defaults when there is no generator:
        //   generator_hash = zeros; generator_refs_hash = [1; 32];
        //   signature = infinity; cost = 0; spend_bundle_fees = 0.
        let (generator_root, generator_refs_root, aggregated_signature, cost, spend_bundle_fees) =
            if let Some(tx) = transactions {
                for coin in &tx.additions {
                    tx_additions.push(*coin);
                    byte_array_tx.push(coin.puzzle_hash.bytes().to_vec());
                }
                for coin in &tx.removals {
                    let cname = coin.name();
                    tx_removal_names.push(cname);
                    byte_array_tx.push(cname.bytes().to_vec());
                }
                let generator_root = transactions_generator_root(&tx.program);
                // transactions_generator_refs_root returns [1; 32] for an empty list.
                let generator_refs_root = transactions_generator_refs_root(&tx.block_refs)?;
                // spend_bundle_fees = compute_block_fee(additions, removals)
                let fees = compute_block_fee(&tx.additions, &tx.removals)?;
                (
                    generator_root,
                    generator_refs_root,
                    tx.aggregated_signature,
                    tx.cost,
                    fees,
                )
            } else {
                (
                    Bytes32::default(),
                    transactions_generator_refs_root(&[])?,
                    g2_infinity(),
                    0u64,
                    0u64,
                )
            };

        // additions_root over merkle items (puzzle_hash, hash_coin_ids(coin_ids)); removals_root
        // over the removal coin names. Both reuse block_generator's canonical_* helpers.
        let additions_root =
            canonical_additions_root(&tx_additions).map_err(|_| ChiaError::BadAdditionRoot)?;
        let removals_root = canonical_removals_root(&tx_removal_names);

        // filter_hash = std_hash(chia_block_filter(byte_array_tx))
        let encoded = chia_block_filter(&byte_array_tx);
        let filter_hash = Bytes32::new(hash_256(encoded));

        let info = TransactionsInfo {
            generator_root,
            generator_refs_root,
            aggregated_signature,
            fees: spend_bundle_fees,
            cost,
            reward_claims_incorporated,
        };

        let ftb = FoliageTransactionBlock {
            prev_transaction_block_hash,
            timestamp,
            filter_hash,
            additions_root,
            removals_root,
            transactions_info_hash: transactions_info_hash(&info)?,
        };
        // foliage_transaction_block_hash is computed once and reused as the signing message,
        // so the (hash Some) == (signature Some) invariant is established atomically here.
        let ftb_hash = ftb.hash().map_err(|_| ChiaError::InvalidFoliageBlockHash)?;
        foliage_transaction_block_hash = Some(ftb_hash);
        foliage_transaction_block_signature = Some(match signing {
            FoliageSigning::Signer {
                plot_public_key,
                sign,
            } => sign(ftb_hash, plot_public_key),
            FoliageSigning::Precomputed {
                foliage_transaction_block_signature,
                ..
            } => *foliage_transaction_block_signature,
        });
        foliage_transaction_block = Some(ftb);
        transactions_info = Some(info);
    }

    // Foliage(prev_block_hash, reward_block_unfinished.get_hash(), foliage_data,
    // foliage_block_data_signature, foliage_transaction_block_hash, foliage_transaction_block_signature).
    // The invariant `(ftb_hash is None) == (ftb_signature is None)` holds because both are set
    // together inside the is_transaction_block branch above and both remain None otherwise.
    debug_assert_eq!(
        foliage_transaction_block_hash.is_some(),
        foliage_transaction_block_signature.is_some(),
        "(ftb_hash Some) == (ftb_signature Some) must hold"
    );
    let foliage = Foliage {
        prev_block_hash,
        reward_block_hash: reward_block_unfinished_hash,
        foliage_block_data: foliage_data,
        foliage_block_data_signature,
        foliage_transaction_block_hash,
        foliage_transaction_block_signature,
    };

    Ok(FoliageResult {
        foliage,
        foliage_transaction_block,
        transactions_info,
    })
}

/// Assemble a full [`UnfinishedBlock`] from the signage-point state plus the foliage payload.
/// This builds the
/// [`RewardChainBlockUnfinished`] from the proof-of-space and the challenge-/reward-chain signage-point
/// VDFs, calls [`create_foliage`] with that reward block's hash, and packs the result into an
/// [`UnfinishedBlock`] (the infusion-point VDFs are filled later by `unfinished_block_to_full_block`).
///
///   * `infusion_point_total_iters` becomes the reward block's `total_iters`;
///   * `pos_ss_cc_challenge_hash` is the slot's cc challenge;
///   * `challenge_chain_sp_vdf` / `reward_chain_sp_vdf` are the signage point's cc/rc VDFs
///     (`Option`: `None` at a sub-slot's first signage point);
///   * `challenge_chain_sp_proof` / `reward_chain_sp_proof` are carried through unchanged into
///     the `UnfinishedBlock`;
///   * `finished_sub_slots` is already copied by the caller.
///
/// The remaining parameters (`height` .. `plot_signer`) are forwarded verbatim to [`create_foliage`];
/// see its docs for the block-store-derived inputs pre-resolved at the call site.
///
/// `plot_signer` produces `challenge_chain_sp_signature` over `cc_sp_hash` and
/// `reward_chain_sp_signature` over `rc_sp_hash`, and the same signer is forwarded into
/// [`create_foliage`] for the two foliage signatures. `proof_of_space.plot_public_key` is the G1 key
/// handed to the signer at every point.
///
/// `cc_sp_hash` / `rc_sp_hash` are the signage-point message hashes, pre-resolved by the caller.
/// In testing mode the caller
/// sets `cc_sp_hash = signage_point.cc_vdf.output.get_hash()` and
/// `rc_sp_hash = signage_point.rc_vdf.output.get_hash()`; on the real path it derives `rc_sp_hash` from
/// the last finished sub-slot's reward chain (or the genesis challenge / the ancestor's reward-slot
/// hash) and `cc_sp_hash = slot_cc_challenge`. The VDFs enter the reward block exactly as supplied.
///
/// # Errors
/// [`ChiaError::InvalidRewardBlockHash`] if the assembled [`RewardChainBlockUnfinished`] fails to
/// serialize for hashing, plus the propagated errors of [`create_foliage`].
#[allow(clippy::too_many_arguments)]
pub fn create_unfinished_block(
    constants: &ConsensusConstants,
    infusion_point_total_iters: u128,
    signage_point_index: u8,
    proof_of_space: ProofOfSpace,
    pos_ss_cc_challenge_hash: Bytes32,
    challenge_chain_sp_vdf: Option<VdfInfo>,
    challenge_chain_sp_proof: Option<VdfProof>,
    reward_chain_sp_vdf: Option<VdfInfo>,
    reward_chain_sp_proof: Option<VdfProof>,
    cc_sp_hash: Bytes32,
    rc_sp_hash: Bytes32,
    finished_sub_slots: Vec<SubSlotBundle>,
    height: u32,
    is_transaction_block: bool,
    reward_claims: &[RewardBlockClaim],
    transactions: Option<&BlockTransactions>,
    prev_block_hash: Bytes32,
    prev_transaction_block_hash: Bytes32,
    pool_target: PoolTarget,
    pool_signature: Option<Bytes96>,
    farmer_reward_puzzle_hash: Bytes32,
    timestamp: u64,
    seed: &[u8],
    plot_signer: impl Fn(Bytes32, &Bytes48) -> Bytes96,
) -> Result<UnfinishedBlock, ChiaError> {
    // plot_public_key is captured (Copy) before proof_of_space is moved into the reward block below,
    // and is also forwarded to create_foliage for the two foliage plot signatures.
    let plot_public_key = proof_of_space.plot_public_key;
    let challenge_chain_sp_signature = plot_signer(cc_sp_hash, &plot_public_key);
    let reward_chain_sp_signature = plot_signer(rc_sp_hash, &plot_public_key);

    // Field order mirrors the model reward_chain_block_unfinished.rs exactly.
    let reward_chain_block = RewardChainBlockUnfinished {
        total_iters: infusion_point_total_iters,
        signage_point_index,
        pos_ss_cc_challenge_hash,
        proof_of_space,
        challenge_chain_sp_vdf,
        challenge_chain_sp_signature,
        reward_chain_sp_vdf,
        reward_chain_sp_signature,
    };

    // create_foliage takes the reward block's hash directly, so compute it once here and forward it
    // (borrow before the reward block is moved into the UnfinishedBlock below).
    let reward_block_unfinished_hash = reward_chain_block
        .hash()
        .map_err(|_| ChiaError::InvalidRewardBlockHash)?;

    let foliage_result = create_foliage(
        constants,
        reward_block_unfinished_hash,
        height,
        is_transaction_block,
        reward_claims,
        transactions,
        prev_block_hash,
        prev_transaction_block_hash,
        pool_target,
        pool_signature,
        plot_public_key,
        farmer_reward_puzzle_hash,
        timestamp,
        seed,
        plot_signer,
    )?;

    // The UnfinishedBlock model types the ref list as a bare Vec<u32> (never None; empty when
    // absent) — see blockchain/full_block.rs for the same shape.
    let (transactions_generator, transactions_generator_ref_list) = match transactions {
        Some(tx) => (Some(tx.program.clone()), tx.block_refs.clone()),
        None => (None, Vec::new()),
    };

    Ok(UnfinishedBlock {
        finished_sub_slots,
        reward_chain_block,
        challenge_chain_sp_proof,
        reward_chain_sp_proof,
        foliage: foliage_result.foliage,
        foliage_transaction_block: foliage_result.foliage_transaction_block,
        transactions_info: foliage_result.transactions_info,
        transactions_generator,
        transactions_generator_ref_list,
    })
}

/// [`create_unfinished_block`] with FARMER-supplied signatures instead of a local `plot_signer` — the
/// live-node emit path: the SP signatures come FROM THE DECLARE MESSAGE and the two foliage
/// signatures are infinity placeholders at declare time.
///
/// The node never holds the plot key, so:
///   * `farmer_sigs.challenge_chain_sp_signature` / `.reward_chain_sp_signature` are taken verbatim
///     from the `DeclareProofOfSpace` message (they are already there — do NOT placeholder them);
///   * `farmer_sigs.foliage_block_data_signature` / `.foliage_transaction_block_signature` are the
///     [`g2_infinity`] PLACEHOLDER at declare time and the REAL farmer signatures (from `SignedValues`)
///     once known. When building the placeholder candidate, splice the real ones in later with
///     [`splice_farmer_foliage_signatures`] rather than rebuilding.
///
/// After building the placeholder candidate, the caller reads the two hashes the farmer must sign from
/// the returned block: `block.foliage.foliage_block_data.hash()` and
/// `block.foliage.foliage_transaction_block_hash` (the `RequestSignedValues` payload).
///
/// # Errors
/// [`ChiaError::InvalidRewardBlockHash`] if the reward block fails to hash, plus the propagated errors
/// of [`create_foliage_with_sigs`].
#[allow(clippy::too_many_arguments)]
pub fn create_unfinished_block_with_sigs(
    constants: &ConsensusConstants,
    infusion_point_total_iters: u128,
    signage_point_index: u8,
    proof_of_space: ProofOfSpace,
    pos_ss_cc_challenge_hash: Bytes32,
    challenge_chain_sp_vdf: Option<VdfInfo>,
    challenge_chain_sp_proof: Option<VdfProof>,
    reward_chain_sp_vdf: Option<VdfInfo>,
    reward_chain_sp_proof: Option<VdfProof>,
    finished_sub_slots: Vec<SubSlotBundle>,
    height: u32,
    is_transaction_block: bool,
    reward_claims: &[RewardBlockClaim],
    transactions: Option<&BlockTransactions>,
    prev_block_hash: Bytes32,
    prev_transaction_block_hash: Bytes32,
    pool_target: PoolTarget,
    pool_signature: Option<Bytes96>,
    farmer_reward_puzzle_hash: Bytes32,
    timestamp: u64,
    seed: &[u8],
    farmer_sigs: FarmerSignatures,
) -> Result<UnfinishedBlock, ChiaError> {
    // The SP signatures come from the DeclareProofOfSpace message, NOT a signer.
    // Unlike the signer path, cc_sp_hash / rc_sp_hash are not needed here — the SP signatures are
    // already resolved — so this sibling drops those two params.
    let reward_chain_block = RewardChainBlockUnfinished {
        total_iters: infusion_point_total_iters,
        signage_point_index,
        pos_ss_cc_challenge_hash,
        proof_of_space,
        challenge_chain_sp_vdf,
        challenge_chain_sp_signature: farmer_sigs.challenge_chain_sp_signature,
        reward_chain_sp_vdf,
        reward_chain_sp_signature: farmer_sigs.reward_chain_sp_signature,
    };

    let reward_block_unfinished_hash = reward_chain_block
        .hash()
        .map_err(|_| ChiaError::InvalidRewardBlockHash)?;

    let foliage_result = create_foliage_with_sigs(
        constants,
        reward_block_unfinished_hash,
        height,
        is_transaction_block,
        reward_claims,
        transactions,
        prev_block_hash,
        prev_transaction_block_hash,
        pool_target,
        pool_signature,
        farmer_reward_puzzle_hash,
        timestamp,
        seed,
        farmer_sigs.foliage_block_data_signature,
        farmer_sigs.foliage_transaction_block_signature,
    )?;

    let (transactions_generator, transactions_generator_ref_list) = match transactions {
        Some(tx) => (Some(tx.program.clone()), tx.block_refs.clone()),
        None => (None, Vec::new()),
    };

    Ok(UnfinishedBlock {
        finished_sub_slots,
        reward_chain_block,
        challenge_chain_sp_proof,
        reward_chain_sp_proof,
        foliage: foliage_result.foliage,
        foliage_transaction_block: foliage_result.foliage_transaction_block,
        transactions_info: foliage_result.transactions_info,
        transactions_generator,
        transactions_generator_ref_list,
    })
}

/// Splice the farmer's real foliage signatures into a candidate built with placeholders —
/// the node half of the `signed_values` flow. The `foliage_block_data_signature` is always
/// overwritten; the `foliage_transaction_block_signature` is overwritten ONLY for a
/// transaction block (`candidate.foliage.foliage_transaction_block_hash.is_some()`) — a
/// non-transaction block has no `foliage_transaction_block_signature` slot (it stays `None`).
///
/// The caller MUST first verify `foliage_block_data_signature` against the candidate's
/// `reward_chain_block.proof_of_space.plot_public_key` over
/// `candidate.foliage.foliage_block_data.hash()` before calling
/// this — this helper only performs the splice, it does not validate.
pub fn splice_farmer_foliage_signatures(
    candidate: &mut UnfinishedBlock,
    foliage_block_data_signature: Bytes96,
    foliage_transaction_block_signature: Bytes96,
) {
    candidate.foliage.foliage_block_data_signature = foliage_block_data_signature;
    // Only a transaction block carries a foliage_transaction_block_signature. The
    // (ftb_hash Some) == (ftb_signature Some) invariant from create_foliage means we key the splice
    // on the hash slot being present.
    if candidate.foliage.foliage_transaction_block_hash.is_some() {
        candidate.foliage.foliage_transaction_block_signature =
            Some(foliage_transaction_block_signature);
    }
}

/// Verify a farmer plot signature (`Bytes96`) over `msg` against a plot public key (`Bytes48`) under
/// AugScheme. Kept here so callers (the full node's `signed_values` handler) never touch `blst`
/// directly. Returns `false` on any decode failure (fail-closed — a malformed signature is a
/// verify miss).
#[must_use]
pub fn verify_plot_signature(plot_public_key: &Bytes48, msg: Bytes32, signature: &Bytes96) -> bool {
    use blst::min_pk::{PublicKey, Signature};
    let pk: PublicKey = plot_public_key.into();
    let Ok(sig) = Signature::try_from(signature) else {
        return false;
    };
    crate::clvm::bls_bindings::verify_signature(&pk, msg.as_ref(), &sig)
}

/// Finish an [`UnfinishedBlock`] into a [`FullBlock`] by infusing the timelord's infusion-point VDFs
/// and tweaking the height / weight / foliage links the foliage could not know at signage time.
///
/// The reward-chain block's signage-point-and-earlier fields are copied verbatim from the unfinished
/// reward block; the three infusion-point VDFs (`cc_ip`, `rc_ip`, optional `icc_ip`) and their proofs
/// come from the timelord's `NewInfusionPointVDF`. `is_transaction_block` is computed by the daemon
/// against the block store and passed in (core holds no store); it is forced `true` at genesis.
///
/// The foliage `reward_block_hash` is re-derived from the finished reward-chain block. On a non-genesis
/// block the foliage's `prev_block_hash` is set to the previous block's header hash, and a
/// non-transaction block additionally nulls the `foliage_transaction_block_hash` + its signature
/// and drops the transaction foliage/info/generator. The genesis block keeps
/// its foliage untouched save for `reward_block_hash`.
///
/// # Errors
/// [`ChiaError::InvalidRewardBlockHash`] if the finished reward-chain block fails to hash.
#[allow(clippy::too_many_arguments)]
pub fn unfinished_block_to_full_block(
    unfinished_block: &UnfinishedBlock,
    cc_ip_vdf: VdfInfo,
    cc_ip_proof: VdfProof,
    rc_ip_vdf: VdfInfo,
    rc_ip_proof: VdfProof,
    icc_ip_vdf: Option<VdfInfo>,
    icc_ip_proof: Option<VdfProof>,
    finished_sub_slots: Vec<SubSlotBundle>,
    prev_block: Option<&BlockRecord>,
    is_transaction_block: bool,
    difficulty: u64,
) -> Result<FullBlock, ChiaError> {
    let rcb_u = &unfinished_block.reward_chain_block;
    // prev None ⇒ genesis transaction block at height 0, weight = difficulty; else extend the
    // prev block with the caller-supplied is_transaction_block.
    let (is_transaction_block, new_weight, new_height) = match prev_block {
        None => (true, u128::from(difficulty), 0u32),
        Some(prev) => (
            is_transaction_block,
            prev.weight + u128::from(difficulty),
            prev.height + 1,
        ),
    };
    // only a genesis or a transaction block keeps the transaction foliage/info/generator
    let keep_tx = prev_block.is_none() || is_transaction_block;
    let (new_foliage_transaction_block, new_tx_info, new_generator, new_generator_ref_list) =
        if keep_tx {
            (
                unfinished_block.foliage_transaction_block,
                unfinished_block.transactions_info.clone(),
                unfinished_block.transactions_generator.clone(),
                unfinished_block.transactions_generator_ref_list.clone(),
            )
        } else {
            (None, None, None, Vec::new())
        };
    // RewardChainBlock: SP-and-earlier fields verbatim from the
    // unfinished reward block, the three infusion-point VDFs spliced in, new height/weight/is_tx.
    let reward_chain_block = RewardChainBlock {
        weight: new_weight,
        height: new_height,
        total_iters: rcb_u.total_iters,
        signage_point_index: rcb_u.signage_point_index,
        pos_ss_cc_challenge_hash: rcb_u.pos_ss_cc_challenge_hash,
        proof_of_space: rcb_u.proof_of_space.clone(),
        challenge_chain_sp_vdf: rcb_u.challenge_chain_sp_vdf,
        challenge_chain_sp_signature: rcb_u.challenge_chain_sp_signature,
        challenge_chain_ip_vdf: cc_ip_vdf,
        reward_chain_sp_vdf: rcb_u.reward_chain_sp_vdf,
        reward_chain_sp_signature: rcb_u.reward_chain_sp_signature,
        reward_chain_ip_vdf: rc_ip_vdf,
        infused_challenge_chain_ip_vdf: icc_ip_vdf,
        is_transaction_block,
    };
    // foliage.replace(reward_block_hash=..., [prev_block_hash,
    // foliage_transaction_block_hash/signature nulled for a non-tx block]).
    let reward_block_hash = reward_chain_block
        .hash()
        .map_err(|_| ChiaError::InvalidRewardBlockHash)?;
    let mut new_foliage = unfinished_block.foliage;
    new_foliage.reward_block_hash = reward_block_hash;
    if let Some(prev) = prev_block {
        new_foliage.prev_block_hash = prev.header_hash;
        if !is_transaction_block {
            new_foliage.foliage_transaction_block_hash = None;
            new_foliage.foliage_transaction_block_signature = None;
        }
    }
    Ok(FullBlock {
        finished_sub_slots,
        reward_chain_block,
        challenge_chain_sp_proof: unfinished_block.challenge_chain_sp_proof.clone(),
        challenge_chain_ip_proof: cc_ip_proof,
        reward_chain_sp_proof: unfinished_block.reward_chain_sp_proof.clone(),
        reward_chain_ip_proof: rc_ip_proof,
        infused_challenge_chain_ip_proof: icc_ip_proof,
        foliage: new_foliage,
        foliage_transaction_block: new_foliage_transaction_block,
        transactions_info: new_tx_info,
        transactions_generator: new_generator,
        transactions_generator_ref_list: new_generator_ref_list,
    })
}

/// A non-genesis block whose
/// pool target is the `(GENESIS_PRE_FARM_POOL_PUZZLE_HASH, 0)` pre-farm target AND whose proof-of-space
/// carries a pool PUBLIC KEY (the plot-NFT-less pooling scheme) must carry a valid pool signature over
/// `bytes(pool_target)`. A plot-NFT plot (`pool_public_key is None`, a pool contract puzzle hash instead)
/// needs no signature at this gate, and a genesis block (`prev_block_hash == GENESIS_CHALLENGE`) is
/// exempt. Fail-closed on a missing/malformed signature (a verify miss); `true`
/// whenever the guard does not apply.
#[must_use]
pub fn has_valid_pool_sig(constants: &ConsensusConstants, block: &FullBlock) -> bool {
    use blst::min_pk::{PublicKey, Signature};
    let fbd = &block.foliage.foliage_block_data;
    let Some(pool_pk) = block.reward_chain_block.proof_of_space.pool_public_key else {
        // Plot-NFT plot (pool contract puzzle hash): no key-based pool signature at this gate.
        return true;
    };
    let is_pre_farm_target = fbd.pool_target.puzzle_hash
        == constants.genesis_pre_farm_pool_puzzle_hash
        && fbd.pool_target.max_height == 0;
    if !is_pre_farm_target || block.foliage.prev_block_hash == constants.genesis_challenge {
        // The signature is only checked for a pre-farm-target block that is NOT genesis; otherwise
        // the pool signature is validated elsewhere (block body) — this early gate passes.
        return true;
    }
    let (Ok(pool_target_bytes), Some(pool_sig)) = (
        fbd.pool_target.to_bytes(ChiaProtocolVersion::default()),
        fbd.pool_signature,
    ) else {
        return false;
    };
    let pk: PublicKey = (&pool_pk).into();
    let Ok(sig) = Signature::try_from(&pool_sig) else {
        return false;
    };
    crate::clvm::bls_bindings::verify_signature(&pk, &pool_target_bytes, &sig)
}

#[cfg(test)]
mod producer_foliage_tests {
    use super::{
        BlockTransactions, RewardBlockClaim, compute_block_fee, create_foliage, g2_infinity,
    };
    use crate::blockchain::coin::Coin;
    use crate::blockchain::pool_target::PoolTarget;
    use crate::blockchain::sized_bytes::{Bytes32, Bytes48, Bytes96};
    use crate::clvm::bls_bindings::{sign, verify_signature};
    use crate::consensus::block_generator::canonical_additions_root;
    use crate::consensus::block_rewards::{calculate_base_farmer_reward, calculate_pool_reward};
    use crate::consensus::coinbase::{create_farmer_coin, create_pool_coin};
    use crate::consensus::constants::MAINNET;
    use crate::traits::SizedBytes;
    use crate::utils::hash_256;
    use blst::min_pk::{PublicKey, SecretKey, Signature};

    fn ph(byte: u8) -> Bytes32 {
        Bytes32::new([byte; 32])
    }

    /// A deterministic plot secret key for tests. The plot
    /// public key `sk.sk_to_pk()` is the G1 handed to the signer and verified against — the single-key
    /// straightforward AugScheme case (taproot/pool aggregation is a farmer/harvester concern; see the
    /// module note).
    fn plot_sk(seed: u8) -> SecretKey {
        SecretKey::key_gen_v3(&[seed; 32], &[]).expect("deterministic plot sk")
    }

    /// The plot public key as the wire `Bytes48` (`ProofOfSpace.plot_public_key`).
    fn plot_pk_bytes(sk: &SecretKey) -> Bytes48 {
        sk.sk_to_pk().into()
    }

    /// A real AugScheme plot signer closure: sign the
    /// 32-byte message with `sk`, prepending `sk`'s own public key (AugScheme), returning the G2
    /// signature as `Bytes96`. `sign` (bls_bindings) uses `sk_to_pk()` as the prepend, so the result
    /// verifies against `plot_pk_bytes(sk)` under [`verify_signature`].
    fn real_signer(sk: &SecretKey) -> impl Fn(Bytes32, &Bytes48) -> Bytes96 + '_ {
        move |msg: Bytes32, _plot_pk: &Bytes48| -> Bytes96 { sign(sk, msg.as_ref()).into() }
    }

    /// Verify a `Bytes96` plot signature over `msg` against a `Bytes48` plot public key under AugScheme.
    fn verifies(plot_pk: &Bytes48, msg: Bytes32, sig: &Bytes96) -> bool {
        let pk: PublicKey = plot_pk.into();
        let Ok(sig) = Signature::try_from(sig) else {
            return false;
        };
        verify_signature(&pk, msg.as_ref(), &sig)
    }

    // fee = sum(removals) - sum(additions); negative (minting) is an error.
    #[test]
    fn compute_block_fee_matches_chia_vectors() {
        let mk = |amts: &[u64]| -> Vec<Coin> {
            amts.iter()
                .enumerate()
                .map(|(i, a)| Coin {
                    parent_coin_info: ph(i as u8),
                    puzzle_hash: ph(0xF0 | i as u8),
                    amount: *a,
                })
                .collect()
        };
        let add_cases: Vec<Vec<u64>> = vec![vec![0], vec![1, 2, 3], vec![]];
        let rem_cases: Vec<Vec<u64>> = vec![vec![0], vec![1, 2, 3], vec![]];
        for add in &add_cases {
            for rem in &rem_cases {
                let additions = mk(add);
                let removals = mk(rem);
                let add_sum: i128 = add.iter().map(|a| i128::from(*a)).sum();
                let rem_sum: i128 = rem.iter().map(|a| i128::from(*a)).sum();
                let expected = rem_sum - add_sum;
                let got = compute_block_fee(&additions, &removals);
                if expected < 0 {
                    assert!(got.is_err(), "add={add:?} rem={rem:?} must error (minting)");
                } else {
                    assert_eq!(
                        got.expect("non-minting fee"),
                        expected as u64,
                        "add={add:?} rem={rem:?}"
                    );
                }
            }
        }
    }

    // INVARIANT: parent ids = genesis_challenge half ++ 16-byte big-endian height.
    // Since block_height is uint32, the 16-byte height is 12 zero bytes then the 4-byte height, so the
    // parent id is 16 challenge bytes ++ 12 zero bytes ++ height.to_be_bytes(). pool uses [:16],
    // farmer uses [16:]; the two must differ, and rebuilding must be deterministic.
    #[test]
    fn reward_coin_parent_ids_compose_from_genesis_challenge_and_height() {
        let gc = MAINNET.genesis_challenge;
        let height = 0x00AB_CDEFu32;
        let pool = create_pool_coin(height, ph(0x11), 1, gc);
        let farmer = create_farmer_coin(height, ph(0x22), 1, gc);

        let pool_pid = pool.parent_coin_info.bytes();
        let farmer_pid = farmer.parent_coin_info.bytes();
        assert_eq!(
            &pool_pid[0..16],
            &gc.bytes()[0..16],
            "pool prefix = challenge[:16]"
        );
        assert_eq!(
            &farmer_pid[0..16],
            &gc.bytes()[16..32],
            "farmer prefix = challenge[16:]"
        );
        assert_eq!(
            &pool_pid[16..28],
            &[0u8; 12],
            "12 zero bytes of the 16-byte BE height"
        );
        assert_eq!(
            &pool_pid[28..32],
            &height.to_be_bytes(),
            "height in the low 4 bytes"
        );
        assert_eq!(&farmer_pid[28..32], &height.to_be_bytes());
        assert_ne!(pool_pid, farmer_pid, "pool and farmer parent ids differ");
        // deterministic
        assert_eq!(create_pool_coin(height, ph(0x11), 1, gc), pool);
    }

    // A non-transaction block returns (Foliage, None, None) and no foliage_transaction fields.
    #[test]
    fn non_transaction_block_has_no_foliage_transaction_block() {
        let pool_target = PoolTarget {
            puzzle_hash: ph(0x01),
            max_height: 0,
        };
        let sk = plot_sk(0x11);
        let res = create_foliage(
            &MAINNET,
            ph(0xAA), // reward_block_unfinished_hash
            10,       // height
            false,    // is_transaction_block
            &[],
            None,
            ph(0xBB), // prev_block_hash
            ph(0xCC), // prev_transaction_block_hash (unused here)
            pool_target,
            None,               // pool_signature
            plot_pk_bytes(&sk), // plot_public_key
            ph(0xDD),           // farmer_reward_puzzle_hash
            123,                // timestamp
            b"seed-nontx",
            real_signer(&sk),
        )
        .expect("foliage builds");
        assert!(res.foliage_transaction_block.is_none());
        assert!(res.transactions_info.is_none());
        assert!(res.foliage.foliage_transaction_block_hash.is_none());
        assert!(res.foliage.foliage_transaction_block_signature.is_none());
        // foliage_block_data_signature is real (non-placeholder) and verifies against the plot key.
        assert_ne!(
            res.foliage.foliage_block_data_signature,
            Bytes96::default(),
            "signed, not the zero placeholder"
        );
        assert!(verifies(
            &plot_pk_bytes(&sk),
            res.foliage.foliage_block_data.hash().unwrap(),
            &res.foliage.foliage_block_data_signature,
        ));
        assert_eq!(res.foliage.reward_block_hash, ph(0xAA));
        assert_eq!(res.foliage.prev_block_hash, ph(0xBB));
        assert_eq!(
            res.foliage.foliage_block_data.farmer_reward_puzzle_hash,
            ph(0xDD)
        );
    }

    // The genesis transaction block: height 0, no reward claims, no spends. TransactionsInfo has
    // generator_hash = zeros, generator_refs_hash = [1;32], signature = infinity (0xc0..),
    // fees = 0, cost = 0, empty reward_claims_incorporated. The empty addition/removal merkle
    // sets are all-zeros, and the empty BIP158 filter is [0] so filter_hash = sha256([0]).
    #[test]
    fn genesis_transaction_block_defaults_match_chia() {
        let pool_target = PoolTarget {
            puzzle_hash: ph(0x01),
            max_height: 0,
        };
        let sk = plot_sk(0x22);
        let res = create_foliage(
            &MAINNET,
            ph(0xAA),
            0,                         // genesis height
            true,                      // genesis is a transaction block
            &[],                       // no reward claims at height 0
            None,                      // no spends
            MAINNET.genesis_challenge, // prev_block_hash
            MAINNET.genesis_challenge, // prev_transaction_block_hash
            pool_target,
            None,               // pool_signature
            plot_pk_bytes(&sk), // plot_public_key
            ph(0xDD),
            1_600_000_000,
            b"genesis-seed",
            real_signer(&sk),
        )
        .expect("genesis foliage builds");

        let info = res.transactions_info.expect("genesis tx info");
        assert_eq!(
            info.generator_root,
            Bytes32::default(),
            "no generator => zeros"
        );
        assert_eq!(
            info.generator_refs_root,
            Bytes32::new([1u8; 32]),
            "empty refs => [1;32]"
        );
        assert_eq!(
            info.aggregated_signature,
            g2_infinity(),
            "empty sig => G2 infinity 0xc0.."
        );
        assert_ne!(
            info.aggregated_signature,
            Bytes96::default(),
            "must NOT be all-zeros Bytes96"
        );
        assert_eq!(info.fees, 0);
        assert_eq!(info.cost, 0);
        assert!(info.reward_claims_incorporated.is_empty());

        let ftb = res.foliage_transaction_block.expect("genesis ftb");
        assert_eq!(
            ftb.additions_root,
            Bytes32::default(),
            "empty additions merkle => zeros"
        );
        assert_eq!(
            ftb.removals_root,
            Bytes32::default(),
            "empty removals merkle => zeros"
        );
        assert_eq!(
            ftb.filter_hash,
            Bytes32::new(hash_256(vec![0u8])),
            "empty BIP158 filter [0] => sha256([0])"
        );
        assert_eq!(ftb.prev_transaction_block_hash, MAINNET.genesis_challenge);
        // Foliage commits the ftb hash and mirrors the (hash Some) == (sig Some) invariant.
        assert_eq!(
            res.foliage.foliage_transaction_block_hash,
            Some(ftb.hash().unwrap())
        );
        // Both foliage plot signatures are real and verify against the plot key under AugScheme.
        let ftb_sig = res
            .foliage
            .foliage_transaction_block_signature
            .expect("ftb signed");
        assert!(verifies(&plot_pk_bytes(&sk), ftb.hash().unwrap(), &ftb_sig));
        assert!(verifies(
            &plot_pk_bytes(&sk),
            res.foliage.foliage_block_data.hash().unwrap(),
            &res.foliage.foliage_block_data_signature,
        ));
    }

    // INVARIANT: a transaction block above genesis mints the reward coins from its claims. Pool amount
    // == calculate_pool_reward(height); farmer amount == calculate_base_farmer_reward(height) + fees;
    // reward_claims_incorporated preserves [pool, farmer] order; and those coins are exactly the block's
    // additions, so additions_root == canonical_additions_root([pool, farmer]).
    #[test]
    fn reward_claims_mint_pool_and_farmer_coins() {
        let claim = RewardBlockClaim {
            height: 100,
            pool_puzzle_hash: ph(0x33),
            farmer_puzzle_hash: ph(0x44),
            fees: 555,
        };
        let pool_target = PoolTarget {
            puzzle_hash: ph(0x01),
            max_height: 0,
        };
        let sk = plot_sk(0x33);
        let res = create_foliage(
            &MAINNET,
            ph(0xAA),
            101,  // height (> 0 so claims are incorporated)
            true, // transaction block
            std::slice::from_ref(&claim),
            None,
            ph(0xBB),
            ph(0xCC),
            pool_target,
            None,               // pool_signature
            plot_pk_bytes(&sk), // plot_public_key
            ph(0xDD),
            1_600_000_000,
            b"reward-seed",
            // Two reward puzzle hashes => a non-empty filter (real BIP158 now); this test only
            // exercises the coin/roots invariants, not the filter_hash value.
            real_signer(&sk),
        )
        .expect("reward foliage builds");

        let info = res.transactions_info.expect("tx info");
        assert_eq!(info.reward_claims_incorporated.len(), 2, "pool + farmer");
        let expected_pool = create_pool_coin(
            claim.height,
            claim.pool_puzzle_hash,
            calculate_pool_reward(claim.height),
            MAINNET.genesis_challenge,
        );
        let expected_farmer = create_farmer_coin(
            claim.height,
            claim.farmer_puzzle_hash,
            calculate_base_farmer_reward(claim.height) + claim.fees,
            MAINNET.genesis_challenge,
        );
        assert_eq!(
            info.reward_claims_incorporated[0], expected_pool,
            "pool first"
        );
        assert_eq!(
            info.reward_claims_incorporated[1], expected_farmer,
            "farmer second"
        );
        assert_eq!(
            info.reward_claims_incorporated[0].amount,
            calculate_pool_reward(claim.height)
        );
        assert_eq!(
            info.reward_claims_incorporated[1].amount,
            calculate_base_farmer_reward(claim.height) + claim.fees
        );

        let ftb = res.foliage_transaction_block.expect("ftb");
        let expected_root =
            canonical_additions_root(&[expected_pool, expected_farmer]).expect("root");
        assert_eq!(
            ftb.additions_root, expected_root,
            "additions are exactly the reward coins"
        );
        assert_eq!(
            ftb.removals_root,
            Bytes32::default(),
            "no removals => empty merkle"
        );
    }

    // A block with spends: fees flow from compute_block_fee(additions, removals) into TransactionsInfo,
    // and removals appear in removals_root. (No CLVM generator needed for these foliage invariants.)
    #[test]
    fn transaction_block_fee_and_removals_flow_into_info() {
        let removed = Coin {
            parent_coin_info: ph(0x55),
            puzzle_hash: ph(0x66),
            amount: 1_000,
        };
        let created = Coin {
            parent_coin_info: removed.name(),
            puzzle_hash: ph(0x77),
            amount: 900,
        };
        let tx = BlockTransactions {
            program: crate::clvm::program::SerializedProgram::from_bytes(&[0x80]),
            block_refs: Vec::new(),
            additions: vec![created],
            removals: vec![removed],
            aggregated_signature: g2_infinity(),
            cost: 42,
        };
        let pool_target = PoolTarget {
            puzzle_hash: ph(0x01),
            max_height: 0,
        };
        let sk = plot_sk(0x44);
        let res = create_foliage(
            &MAINNET,
            ph(0xAA),
            101,
            true,
            &[], // isolate the spend path from reward claims
            Some(&tx),
            ph(0xBB),
            ph(0xCC),
            pool_target,
            None,               // pool_signature
            plot_pk_bytes(&sk), // plot_public_key
            ph(0xDD),
            1_600_000_000,
            b"spend-seed",
            real_signer(&sk),
        )
        .expect("spend foliage builds");
        let info = res.transactions_info.expect("tx info");
        assert_eq!(info.fees, 100, "1000 removed - 900 created");
        assert_eq!(info.cost, 42);
        let ftb = res.foliage_transaction_block.expect("ftb");
        assert_ne!(
            ftb.removals_root,
            Bytes32::default(),
            "one removal => non-empty merkle"
        );
    }

    // THE ROUND TRIP. Four hashes are plot-signed per block. Prove the BLS layer
    // (bls_bindings::sign / verify_signature, AugScheme DST) closes that loop: sign each of the
    // four message hashes with a fixed-seed plot key and assert every signature VERIFIES against
    // the plot public key; then assert a signature made with the WRONG key FAILS.
    #[test]
    fn plot_signatures_round_trip_and_wrong_key_fails() {
        let sk = plot_sk(0x5A);
        let plot_pk = plot_pk_bytes(&sk);
        let signer = real_signer(&sk);

        // The four signing-point messages, standing in for foliage_data.get_hash(),
        // foliage_transaction_block.get_hash(), cc_sp_hash, rc_sp_hash.
        let messages = [
            Bytes32::new(hash_256(b"foliage_block_data")),
            Bytes32::new(hash_256(b"foliage_transaction_block")),
            Bytes32::new(hash_256(b"cc_sp_hash")),
            Bytes32::new(hash_256(b"rc_sp_hash")),
        ];

        for (i, msg) in messages.iter().copied().enumerate() {
            let sig = signer(msg, &plot_pk);
            assert_ne!(
                sig,
                Bytes96::default(),
                "message {i}: real signature, not the zero placeholder"
            );
            assert!(
                verifies(&plot_pk, msg, &sig),
                "message {i}: signature must verify against the plot public key (AugScheme)"
            );
        }

        // WRONG KEY: a signature over the same message from a different plot key must NOT verify against
        // the original plot public key.
        let wrong_sk = plot_sk(0xA5);
        let wrong_pk = plot_pk_bytes(&wrong_sk);
        assert_ne!(plot_pk, wrong_pk, "distinct plot keys");
        let msg = messages[0];
        let good_sig = signer(msg, &plot_pk);
        assert!(
            !verifies(&wrong_pk, msg, &good_sig),
            "correct signature must FAIL against the wrong plot public key"
        );
        let wrong_sig = real_signer(&wrong_sk)(msg, &wrong_pk);
        assert!(
            !verifies(&plot_pk, msg, &wrong_sig),
            "wrong-key signature must FAIL against the correct plot public key"
        );

        // Sanity: sign() prepends sk_to_pk(), so a signature verifies ONLY under its own signer's key —
        // confirm the two are not cross-verifiable in either direction.
        assert!(verifies(&wrong_pk, msg, &wrong_sig));
        assert!(!verifies(&plot_pk, msg, &wrong_sig));
    }
}

#[cfg(test)]
mod producer_unfinished_block_tests {
    // INVARIANT-ASSERT harness for `create_unfinished_block`: assemble an UnfinishedBlock from
    // hand-built signage-point inputs and a genesis foliage and assert the structural
    // invariants: field propagation into RewardChainBlockUnfinished, the
    // foliage↔reward-block-hash tie, the is_transaction_block consistency, and hash stability.
    use super::{BlockTransactions, create_unfinished_block, g2_infinity};
    use crate::blockchain::class_group_element::ClassgroupElement;
    use crate::blockchain::coin::Coin;
    use crate::blockchain::pool_target::PoolTarget;
    use crate::blockchain::proof_of_space::{ProofBytes, ProofOfSpace};
    use crate::blockchain::sized_bytes::{Bytes32, Bytes48, Bytes96};
    use crate::blockchain::unsized_bytes::UnsizedBytes;
    use crate::blockchain::vdf_info::VdfInfo;
    use crate::blockchain::vdf_proof::VdfProof;
    use crate::clvm::bls_bindings::{sign, verify_signature};
    use crate::clvm::program::SerializedProgram;
    use crate::consensus::constants::MAINNET;
    use crate::traits::SizedBytes;
    use blst::min_pk::{PublicKey, SecretKey, Signature};

    fn ph(byte: u8) -> Bytes32 {
        Bytes32::new([byte; 32])
    }

    fn plot_sk(seed: u8) -> SecretKey {
        SecretKey::key_gen_v3(&[seed; 32], &[]).expect("deterministic plot sk")
    }

    fn plot_pk_bytes(sk: &SecretKey) -> Bytes48 {
        sk.sk_to_pk().into()
    }

    /// Real AugScheme plot signer; the produced signature verifies against
    /// `plot_pk_bytes(sk)` under [`verify_signature`].
    fn real_signer(sk: &SecretKey) -> impl Fn(Bytes32, &Bytes48) -> Bytes96 + '_ {
        move |msg: Bytes32, _plot_pk: &Bytes48| -> Bytes96 { sign(sk, msg.as_ref()).into() }
    }

    /// A structural marker signer: it does NOT produce a valid BLS signature; it packs the signed message
    /// into the first 32 bytes of the `Bytes96` so a test can assert exactly WHICH hash was signed into
    /// WHICH slot (cc_sp vs rc_sp vs foliage). Never fed to `verify_signature`.
    fn marker_signer(msg: Bytes32, _plot_pk: &Bytes48) -> Bytes96 {
        let mut buf = [0u8; 96];
        buf[..32].copy_from_slice(msg.as_ref());
        Bytes96::new(buf)
    }

    fn verifies(plot_pk: &Bytes48, msg: Bytes32, sig: &Bytes96) -> bool {
        let pk: PublicKey = plot_pk.into();
        let Ok(sig) = Signature::try_from(sig) else {
            return false;
        };
        verify_signature(&pk, msg.as_ref(), &sig)
    }

    /// Proof of space carrying an explicit plot public key (the G1 handed to the plot signer).
    fn mk_pos_with_key(plot_public_key: Bytes48) -> ProofOfSpace {
        ProofOfSpace {
            challenge: ph(0x01),
            pool_public_key: None,
            pool_contract_puzzle_hash: Some(ph(0x02)),
            plot_public_key,
            size: 32,
            proof: ProofBytes::from(vec![0x07u8; 64]),
        }
    }

    fn mk_pos() -> ProofOfSpace {
        mk_pos_with_key(Bytes48::new([0x03; 48]))
    }

    fn mk_vdf(challenge: u8, iters: u64) -> VdfInfo {
        VdfInfo {
            challenge: ph(challenge),
            number_of_iterations: iters,
            output: ClassgroupElement::get_default_element(),
        }
    }

    fn mk_vdf_proof(witness_type: u8) -> VdfProof {
        VdfProof {
            witness_type,
            witness: UnsizedBytes::new(vec![0xAA, 0xBB, 0xCC]),
            normalized_to_identity: true,
        }
    }

    fn pool_target() -> PoolTarget {
        PoolTarget {
            puzzle_hash: ph(0x01),
            max_height: 0,
        }
    }

    // The genesis unfinished block: height 0, transaction block, no claims / no spends. Assert every
    // signage-point field propagates into the reward block, the sp VDF proofs carry into the
    // UnfinishedBlock, the foliage commits the reward block's hash, is_transaction_block is consistent,
    // and the reward block's hash is stable across rebuilds.
    #[test]
    fn genesis_unfinished_block_invariants_match_chia() {
        let cc_sp_vdf = mk_vdf(0x10, 1_000);
        let rc_sp_vdf = mk_vdf(0x11, 2_000);
        let total_iters: u128 = 123_456;
        let sp_index: u8 = 7;
        let cc_challenge = ph(0x20);
        // Pre-resolved signage-point message hashes (cc_sp_hash / rc_sp_hash). Distinct so the
        // marker signer can prove each landed in the correct reward-block slot.
        let cc_sp_hash = ph(0x30);
        let rc_sp_hash = ph(0x31);

        let build = || {
            create_unfinished_block(
                &MAINNET,
                total_iters,
                sp_index,
                mk_pos(),
                cc_challenge,
                Some(cc_sp_vdf),
                Some(mk_vdf_proof(1)),
                Some(rc_sp_vdf),
                Some(mk_vdf_proof(2)),
                cc_sp_hash,
                rc_sp_hash,
                Vec::new(),                // finished_sub_slots
                0,                         // height (genesis)
                true,                      // is_transaction_block
                &[],                       // no reward claims at height 0
                None,                      // no spends
                MAINNET.genesis_challenge, // prev_block_hash
                MAINNET.genesis_challenge, // prev_transaction_block_hash
                pool_target(),
                None,
                ph(0xDD),
                1_600_000_000,
                b"genesis-unfinished",
                marker_signer,
            )
        };
        let ub = build().expect("genesis unfinished block builds");

        let rc = &ub.reward_chain_block;
        // total_iters / signage_point_index / pos challenge propagate.
        assert_eq!(rc.total_iters, total_iters);
        assert_eq!(rc.signage_point_index, sp_index);
        assert_eq!(rc.pos_ss_cc_challenge_hash, cc_challenge);
        assert_eq!(rc.proof_of_space, mk_pos());
        // signage-point VDFs propagate into the reward block.
        assert_eq!(rc.challenge_chain_sp_vdf, Some(cc_sp_vdf));
        assert_eq!(rc.reward_chain_sp_vdf, Some(rc_sp_vdf));
        // real signing: the marker signer proves cc_sp_hash was signed into the cc slot and
        // rc_sp_hash into the rc slot (not the zero placeholder, and not swapped).
        assert_eq!(
            rc.challenge_chain_sp_signature,
            marker_signer(cc_sp_hash, &rc.proof_of_space.plot_public_key),
            "cc slot signs cc_sp_hash"
        );
        assert_eq!(
            rc.reward_chain_sp_signature,
            marker_signer(rc_sp_hash, &rc.proof_of_space.plot_public_key),
            "rc slot signs rc_sp_hash"
        );
        assert_ne!(rc.challenge_chain_sp_signature, super::Bytes96::default());
        assert_ne!(rc.reward_chain_sp_signature, super::Bytes96::default());
        assert_ne!(
            rc.challenge_chain_sp_signature, rc.reward_chain_sp_signature,
            "cc and rc signatures are over different hashes"
        );

        // signage-point VDF proofs carried into the UnfinishedBlock unchanged.
        assert_eq!(ub.challenge_chain_sp_proof, Some(mk_vdf_proof(1)));
        assert_eq!(ub.reward_chain_sp_proof, Some(mk_vdf_proof(2)));
        assert!(ub.finished_sub_slots.is_empty());

        // foliage.reward_block_hash == reward_chain_block.hash().
        let rc_hash = rc.hash().expect("rc hash");
        assert_eq!(ub.foliage.reward_block_hash, rc_hash);

        // is_transaction_block consistency: ftb present <=> tx_info present (and both present at genesis).
        assert_eq!(
            ub.foliage_transaction_block.is_some(),
            ub.transactions_info.is_some()
        );
        assert!(ub.foliage_transaction_block.is_some());
        assert!(ub.transactions_info.is_some());
        assert_eq!(
            ub.foliage.foliage_transaction_block_hash.is_some(),
            ub.foliage_transaction_block.is_some()
        );

        // no spend generator => transactions_generator None; the ref list is a bare empty Vec.
        assert!(ub.transactions_generator.is_none());
        assert_eq!(ub.transactions_generator_ref_list, Vec::<u32>::new());

        // partial_hash stability: identical inputs => identical reward-block hash and identical block.
        let ub2 = build().expect("rebuild");
        assert_eq!(
            ub2.reward_chain_block.hash().unwrap(),
            rc_hash,
            "partial_hash stable"
        );
        assert_eq!(ub2, ub, "identical inputs => identical UnfinishedBlock");
    }

    // A non-transaction block: no foliage_transaction_block, no transactions_info, and a first-signage-
    // point reward block (all sp VDFs / proofs None). Foliage still commits the reward block's hash.
    #[test]
    fn non_transaction_unfinished_block_has_no_tx_members() {
        let ub = create_unfinished_block(
            &MAINNET,
            999,
            3,
            mk_pos(),
            ph(0x20),
            None,     // no cc sp vdf (first sp of a sub-slot)
            None,     // no cc sp proof
            None,     // no rc sp vdf
            None,     // no rc sp proof
            ph(0x30), // cc_sp_hash
            ph(0x31), // rc_sp_hash
            Vec::new(),
            10,    // height
            false, // NOT a transaction block
            &[],
            None,
            ph(0xBB),
            ph(0xCC),
            pool_target(),
            None,
            ph(0xDD),
            123,
            b"nontx",
            marker_signer,
        )
        .expect("non-tx unfinished block builds");

        assert!(ub.foliage_transaction_block.is_none());
        assert!(ub.transactions_info.is_none());
        assert_eq!(
            ub.foliage_transaction_block.is_some(),
            ub.transactions_info.is_some()
        );
        assert!(ub.foliage.foliage_transaction_block_hash.is_none());
        assert!(ub.reward_chain_block.challenge_chain_sp_vdf.is_none());
        assert!(ub.reward_chain_block.reward_chain_sp_vdf.is_none());
        assert!(ub.challenge_chain_sp_proof.is_none());
        assert!(ub.reward_chain_sp_proof.is_none());
        assert_eq!(
            ub.foliage.reward_block_hash,
            ub.reward_chain_block.hash().unwrap()
        );
        assert!(ub.transactions_generator.is_none());
        assert_eq!(ub.transactions_generator_ref_list, Vec::<u32>::new());
    }

    // A transaction block carrying a spend generator: transactions_generator and the ref list propagate
    // from the BlockTransactions payload.
    #[test]
    fn transaction_generator_and_ref_list_propagate() {
        let removed = Coin {
            parent_coin_info: ph(0x55),
            puzzle_hash: ph(0x66),
            amount: 1_000,
        };
        let created = Coin {
            parent_coin_info: removed.name(),
            puzzle_hash: ph(0x77),
            amount: 900,
        };
        let program = SerializedProgram::from_bytes(&[0x80]);
        let tx = BlockTransactions {
            program: program.clone(),
            block_refs: vec![3, 5, 8],
            additions: vec![created],
            removals: vec![removed],
            aggregated_signature: g2_infinity(),
            cost: 42,
        };
        let ub = create_unfinished_block(
            &MAINNET,
            777,
            2,
            mk_pos(),
            ph(0x20),
            Some(mk_vdf(0x10, 500)),
            Some(mk_vdf_proof(1)),
            Some(mk_vdf(0x11, 600)),
            Some(mk_vdf_proof(2)),
            ph(0x30), // cc_sp_hash
            ph(0x31), // rc_sp_hash
            Vec::new(),
            101,  // height (> 0)
            true, // transaction block
            &[],  // isolate the spend path from reward claims
            Some(&tx),
            ph(0xBB),
            ph(0xCC),
            pool_target(),
            None,
            ph(0xDD),
            1_600_000_000,
            b"spend-unfinished",
            // non-empty byte_array_tx (a removal) => the real chia_block_filter runs; its filter_hash
            // value is not asserted here (this test isolates generator / ref-list propagation).
            marker_signer,
        )
        .expect("spend unfinished block builds");

        assert_eq!(ub.transactions_generator, Some(program));
        assert_eq!(ub.transactions_generator_ref_list, vec![3, 5, 8]);
        assert!(ub.foliage_transaction_block.is_some());
        assert!(ub.transactions_info.is_some());
        assert_eq!(ub.transactions_info.as_ref().unwrap().fees, 100);
        assert_eq!(
            ub.foliage.reward_block_hash,
            ub.reward_chain_block.hash().unwrap()
        );
    }

    // END-TO-END REAL SIGNING: build a genesis UnfinishedBlock with a real plot key and the
    // AugScheme signer, then assert ALL FOUR plot signatures verify against the proof-of-space's
    // plot public key.
    #[test]
    fn all_four_plot_signatures_verify_end_to_end() {
        let sk = plot_sk(0x7E);
        let plot_pk = plot_pk_bytes(&sk);
        let cc_sp_hash = ph(0x30);
        let rc_sp_hash = ph(0x31);

        let ub = create_unfinished_block(
            &MAINNET,
            123_456,
            7,
            mk_pos_with_key(plot_pk), // real plot public key in the proof of space
            ph(0x20),
            Some(mk_vdf(0x10, 1_000)),
            Some(mk_vdf_proof(1)),
            Some(mk_vdf(0x11, 2_000)),
            Some(mk_vdf_proof(2)),
            cc_sp_hash,
            rc_sp_hash,
            Vec::new(),
            0,    // genesis height
            true, // transaction block
            &[],
            None,
            MAINNET.genesis_challenge,
            MAINNET.genesis_challenge,
            pool_target(),
            None,
            ph(0xDD),
            1_600_000_000,
            b"genesis-signed",
            real_signer(&sk),
        )
        .expect("signed genesis unfinished block builds");

        let rc = &ub.reward_chain_block;
        // 1 + 2: the two signage-point signatures verify over their pre-resolved hashes.
        assert!(
            verifies(&plot_pk, cc_sp_hash, &rc.challenge_chain_sp_signature),
            "challenge_chain_sp_signature verifies"
        );
        assert!(
            verifies(&plot_pk, rc_sp_hash, &rc.reward_chain_sp_signature),
            "reward_chain_sp_signature verifies"
        );

        // 3: foliage_block_data_signature verifies over foliage_data.get_hash().
        assert!(
            verifies(
                &plot_pk,
                ub.foliage.foliage_block_data.hash().unwrap(),
                &ub.foliage.foliage_block_data_signature,
            ),
            "foliage_block_data_signature verifies"
        );

        // 4: foliage_transaction_block_signature verifies over foliage_transaction_block.get_hash().
        let ftb = ub
            .foliage_transaction_block
            .as_ref()
            .expect("genesis ftb present");
        let ftb_sig = ub
            .foliage
            .foliage_transaction_block_signature
            .expect("ftb signed");
        assert!(
            verifies(&plot_pk, ftb.hash().unwrap(), &ftb_sig),
            "foliage_transaction_block_signature verifies"
        );

        // The (ftb_hash Some) == (ftb_sig Some) invariant still holds under real signing.
        assert_eq!(
            ub.foliage.foliage_transaction_block_hash.is_some(),
            ub.foliage.foliage_transaction_block_signature.is_some()
        );

        // None of the four are the zero placeholder.
        for sig in [
            &rc.challenge_chain_sp_signature,
            &rc.reward_chain_sp_signature,
            &ub.foliage.foliage_block_data_signature,
            &ftb_sig,
        ] {
            assert_ne!(*sig, Bytes96::default(), "no zero-placeholder signatures");
        }

        // WRONG KEY: the sp signature must not verify against a different plot public key.
        let wrong_pk = plot_pk_bytes(&plot_sk(0xE7));
        assert!(
            !verifies(&wrong_pk, cc_sp_hash, &rc.challenge_chain_sp_signature),
            "cc_sp_signature must fail against the wrong plot key"
        );
    }
}

// The farmer-supplied-signature emit path. These prove the producer half of the
// declare→RequestSignedValues→signed_values flow: the SP signatures come from the declare
// message (not a signer), the foliage signatures are placeholders at declare time and are
// spliced in from SignedValues, and the resulting block is byte-identical to the signer path
// fed the same four signatures.
#[cfg(test)]
mod producer_emit_path_tests {
    use super::{
        BlockTransactions, FarmerSignatures, calculate_infusion_point_total_iters,
        create_unfinished_block, create_unfinished_block_with_sigs, g2_infinity,
        splice_farmer_foliage_signatures,
    };
    use crate::blockchain::class_group_element::ClassgroupElement;
    use crate::blockchain::coin::Coin;
    use crate::blockchain::pool_target::PoolTarget;
    use crate::blockchain::proof_of_space::{ProofBytes, ProofOfSpace};
    use crate::blockchain::sized_bytes::{Bytes32, Bytes48, Bytes96};
    use crate::blockchain::unsized_bytes::UnsizedBytes;
    use crate::blockchain::vdf_info::VdfInfo;
    use crate::blockchain::vdf_proof::VdfProof;
    use crate::clvm::bls_bindings::{sign, verify_signature};
    use crate::consensus::constants::MAINNET;
    use crate::traits::SizedBytes;
    use blst::min_pk::{PublicKey, SecretKey, Signature};

    fn ph(byte: u8) -> Bytes32 {
        Bytes32::new([byte; 32])
    }
    fn plot_sk(seed: u8) -> SecretKey {
        SecretKey::key_gen_v3(&[seed; 32], &[]).expect("deterministic plot sk")
    }
    fn plot_pk_bytes(sk: &SecretKey) -> Bytes48 {
        sk.sk_to_pk().into()
    }
    fn verifies(plot_pk: &Bytes48, msg: Bytes32, sig: &Bytes96) -> bool {
        let pk: PublicKey = plot_pk.into();
        let Ok(sig) = Signature::try_from(sig) else {
            return false;
        };
        verify_signature(&pk, msg.as_ref(), &sig)
    }
    fn mk_pos(plot_public_key: Bytes48) -> ProofOfSpace {
        ProofOfSpace {
            challenge: ph(0x01),
            pool_public_key: None,
            pool_contract_puzzle_hash: Some(ph(0x02)),
            plot_public_key,
            size: 32,
            proof: ProofBytes::from(vec![0x07u8; 64]),
        }
    }
    fn mk_vdf(challenge: u8, iters: u64) -> VdfInfo {
        VdfInfo {
            challenge: ph(challenge),
            number_of_iterations: iters,
            output: ClassgroupElement::get_default_element(),
        }
    }
    fn mk_vdf_proof(w: u8) -> VdfProof {
        VdfProof {
            witness_type: w,
            witness: UnsizedBytes::new(vec![0xAA, 0xBB]),
            normalized_to_identity: true,
        }
    }
    fn pool_target() -> PoolTarget {
        PoolTarget {
            puzzle_hash: ph(0x01),
            max_height: 0,
        }
    }

    // overflow (sp_iters > ip_iters) adds one sub_slot_iters
    #[test]
    fn infusion_point_total_iters_overflow_math_matches_chia() {
        // Non-overflow: sp_iters <= ip_iters => start + ip_iters.
        assert_eq!(
            calculate_infusion_point_total_iters(1_000, 10, 20, 100_000),
            1_020
        );
        // Overflow: sp_iters > ip_iters => start + ip_iters + sub_slot_iters.
        assert_eq!(
            calculate_infusion_point_total_iters(1_000, 30, 20, 100_000),
            101_020
        );
        // Boundary: sp_iters == ip_iters is NOT overflow (strict >).
        assert_eq!(calculate_infusion_point_total_iters(0, 20, 20, 100_000), 20);
    }

    // THE ASSEMBLY TEST. Given a candidate's inputs + SP VDFs + farmer signatures, the produced
    // UnfinishedBlock is well-formed: the SP signatures from the declare message land in the reward
    // block, the foliage carries the farmer's foliage signatures (verifying against the plot key), the
    // foliage↔reward-block-hash tie holds, and the tx-block invariants hold.
    #[test]
    fn with_sigs_assembles_wellformed_block_carrying_farmer_sigs() {
        let sk = plot_sk(0x5A);
        let plot_pk = plot_pk_bytes(&sk);
        let pos = mk_pos(plot_pk);
        let cc_sp_vdf = mk_vdf(0x10, 1_000);
        let rc_sp_vdf = mk_vdf(0x11, 2_000);

        // Genesis-shaped: height 0, transaction block, no claims/spends.
        // The four farmer signatures — cc/rc "from the declare message", foliage "from SignedValues".
        // We produce cc/rc as real AugScheme signatures so verifies() can confirm them, but the producer
        // treats them as opaque bytes (it does no signing). The foliage sigs are declare-time placeholders.
        let farmer_sigs = FarmerSignatures {
            challenge_chain_sp_signature: sign(&sk, ph(0x30).as_ref()).into(),
            reward_chain_sp_signature: sign(&sk, ph(0x31).as_ref()).into(),
            foliage_block_data_signature: g2_infinity(), // placeholder at declare
            foliage_transaction_block_signature: g2_infinity(),
        };

        let ub = create_unfinished_block_with_sigs(
            &MAINNET,
            123_456,
            7,
            pos.clone(),
            ph(0x20),
            Some(cc_sp_vdf),
            Some(mk_vdf_proof(1)),
            Some(rc_sp_vdf),
            Some(mk_vdf_proof(2)),
            Vec::new(),
            0,
            true,
            &[],
            None,
            MAINNET.genesis_challenge,
            MAINNET.genesis_challenge,
            pool_target(),
            None,
            ph(0xDD),
            1_600_000_000,
            b"emit-seed",
            farmer_sigs,
        )
        .expect("with_sigs builds");

        let rc = &ub.reward_chain_block;
        // SP signatures from the declare message landed verbatim in the reward block.
        assert_eq!(
            rc.challenge_chain_sp_signature,
            farmer_sigs.challenge_chain_sp_signature
        );
        assert_eq!(
            rc.reward_chain_sp_signature,
            farmer_sigs.reward_chain_sp_signature
        );
        assert_eq!(rc.total_iters, 123_456);
        assert_eq!(rc.signage_point_index, 7);
        assert_eq!(rc.challenge_chain_sp_vdf, Some(cc_sp_vdf));
        assert_eq!(rc.reward_chain_sp_vdf, Some(rc_sp_vdf));

        // Placeholder foliage signatures at declare time — the infinity placeholder, NOT zeros.
        assert_eq!(ub.foliage.foliage_block_data_signature, g2_infinity());
        assert_ne!(ub.foliage.foliage_block_data_signature, Bytes96::default());
        assert_eq!(
            ub.foliage.foliage_transaction_block_signature,
            Some(g2_infinity()),
            "tx block carries a (placeholder) ftb signature"
        );

        // foliage.reward_block_hash == reward_chain_block.hash().
        let rc_hash = rc.hash().expect("rc hash");
        assert_eq!(ub.foliage.reward_block_hash, rc_hash);
        // tx-block consistency.
        assert!(ub.foliage_transaction_block.is_some());
        assert!(ub.transactions_info.is_some());
        assert!(ub.foliage.foliage_transaction_block_hash.is_some());
    }

    // THE SPLICE. A placeholder candidate + a SignedValues reply => the real foliage signatures land in
    // the foliage and verify against the plot key; the ftb signature is spliced for a tx block.
    #[test]
    fn splice_replaces_placeholder_foliage_sigs_and_verifies() {
        let sk = plot_sk(0x77);
        let plot_pk = plot_pk_bytes(&sk);
        let pos = mk_pos(plot_pk);
        let farmer_sigs = FarmerSignatures {
            challenge_chain_sp_signature: g2_infinity(),
            reward_chain_sp_signature: g2_infinity(),
            foliage_block_data_signature: g2_infinity(),
            foliage_transaction_block_signature: g2_infinity(),
        };
        let mut candidate = create_unfinished_block_with_sigs(
            &MAINNET,
            10,
            0,
            pos,
            MAINNET.genesis_challenge,
            None,
            None,
            None,
            None,
            Vec::new(),
            0,
            true,
            &[],
            None,
            MAINNET.genesis_challenge,
            MAINNET.genesis_challenge,
            pool_target(),
            None,
            ph(0xDD),
            1_600_000_000,
            b"splice-seed",
            farmer_sigs,
        )
        .expect("candidate builds");

        // The two hashes the farmer signs (the RequestSignedValues payload).
        let fbd_hash = candidate
            .foliage
            .foliage_block_data
            .hash()
            .expect("fbd hash");
        let ftb_hash = candidate
            .foliage
            .foliage_transaction_block_hash
            .expect("tx block => ftb hash");
        // The farmer's real signatures over those hashes (SignedValues).
        let real_fbd: Bytes96 = sign(&sk, fbd_hash.as_ref()).into();
        let real_ftb: Bytes96 = sign(&sk, ftb_hash.as_ref()).into();

        // Pre-splice: placeholders.
        assert_eq!(
            candidate.foliage.foliage_block_data_signature,
            g2_infinity()
        );

        splice_farmer_foliage_signatures(&mut candidate, real_fbd, real_ftb);

        assert_eq!(candidate.foliage.foliage_block_data_signature, real_fbd);
        assert_eq!(
            candidate.foliage.foliage_transaction_block_signature,
            Some(real_ftb)
        );
        // Both spliced signatures verify against the plot key over the hashes the farmer was given.
        assert!(verifies(&plot_pk, fbd_hash, &real_fbd));
        assert!(verifies(&plot_pk, ftb_hash, &real_ftb));
    }

    // A non-transaction candidate has no ftb slot: the splice overwrites fbd but leaves ftb None.
    #[test]
    fn splice_leaves_ftb_none_for_non_transaction_block() {
        let sk = plot_sk(0x33);
        let pos = mk_pos(plot_pk_bytes(&sk));
        let farmer_sigs = FarmerSignatures {
            challenge_chain_sp_signature: g2_infinity(),
            reward_chain_sp_signature: g2_infinity(),
            foliage_block_data_signature: g2_infinity(),
            foliage_transaction_block_signature: g2_infinity(),
        };
        let mut candidate = create_unfinished_block_with_sigs(
            &MAINNET,
            10,
            3,
            pos,
            ph(0x20),
            Some(mk_vdf(0x10, 1)),
            Some(mk_vdf_proof(1)),
            Some(mk_vdf(0x11, 2)),
            Some(mk_vdf_proof(2)),
            Vec::new(),
            10,
            false, // NOT a transaction block
            &[],
            None,
            ph(0xBB),
            ph(0xCC),
            pool_target(),
            None,
            ph(0xDD),
            1_600_000_000,
            b"nontx-splice",
            farmer_sigs,
        )
        .expect("non-tx candidate builds");
        assert!(candidate.foliage.foliage_transaction_block_hash.is_none());

        let fbd_hash = candidate
            .foliage
            .foliage_block_data
            .hash()
            .expect("fbd hash");
        let real_fbd: Bytes96 = sign(&sk, fbd_hash.as_ref()).into();
        splice_farmer_foliage_signatures(&mut candidate, real_fbd, Bytes96::new([0xEE; 96]));

        assert_eq!(candidate.foliage.foliage_block_data_signature, real_fbd);
        assert_eq!(
            candidate.foliage.foliage_transaction_block_signature, None,
            "non-tx block keeps ftb signature None regardless of the supplied value"
        );
    }

    // EQUIVALENCE: the farmer-supplied path produces the SAME UnfinishedBlock the signer
    // path would, when the signer is fed exactly the four signatures — the same bytes go on
    // the wire either way.
    #[test]
    fn with_sigs_equals_signer_path_for_the_same_four_signatures() {
        let sk = plot_sk(0x9C);
        let plot_pk = plot_pk_bytes(&sk);
        let pos = mk_pos(plot_pk);
        let cc_sp_hash = ph(0x40);
        let rc_sp_hash = ph(0x41);

        // A spend so the tx-block path (ftb hash + signature) is exercised.
        let removed = Coin {
            parent_coin_info: ph(0x55),
            puzzle_hash: ph(0x66),
            amount: 1_000,
        };
        let created = Coin {
            parent_coin_info: removed.name(),
            puzzle_hash: ph(0x77),
            amount: 900,
        };
        let tx = BlockTransactions {
            program: crate::clvm::program::SerializedProgram::from_bytes(&[0x80]),
            block_refs: Vec::new(),
            additions: vec![created],
            removals: vec![removed],
            aggregated_signature: g2_infinity(),
            cost: 42,
        };

        // The signer path signs each hash with sk (AugScheme). Build it FIRST so we can read the exact
        // four signatures it produced for the four hashes, then feed those into the with_sigs path.
        let signer_block = create_unfinished_block(
            &MAINNET,
            999,
            5,
            pos.clone(),
            ph(0x20),
            Some(mk_vdf(0x10, 1_000)),
            Some(mk_vdf_proof(1)),
            Some(mk_vdf(0x11, 2_000)),
            Some(mk_vdf_proof(2)),
            cc_sp_hash,
            rc_sp_hash,
            Vec::new(),
            101,
            true,
            &[],
            Some(&tx),
            ph(0xBB),
            ph(0xCC),
            pool_target(),
            None,
            ph(0xDD),
            1_600_000_000,
            b"equiv-seed",
            |msg: Bytes32, _pk: &Bytes48| -> Bytes96 { sign(&sk, msg.as_ref()).into() },
        )
        .expect("signer path builds");

        // The four signatures the signer produced: cc over cc_sp_hash, rc over rc_sp_hash, fbd over the
        // foliage_block_data hash, ftb over the foliage_transaction_block hash.
        let fbd_hash = signer_block
            .foliage
            .foliage_block_data
            .hash()
            .expect("fbd hash");
        let ftb_hash = signer_block
            .foliage
            .foliage_transaction_block_hash
            .expect("ftb hash");
        let farmer_sigs = FarmerSignatures {
            challenge_chain_sp_signature: sign(&sk, cc_sp_hash.as_ref()).into(),
            reward_chain_sp_signature: sign(&sk, rc_sp_hash.as_ref()).into(),
            foliage_block_data_signature: sign(&sk, fbd_hash.as_ref()).into(),
            foliage_transaction_block_signature: sign(&sk, ftb_hash.as_ref()).into(),
        };

        let with_sigs_block = create_unfinished_block_with_sigs(
            &MAINNET,
            999,
            5,
            pos,
            ph(0x20),
            Some(mk_vdf(0x10, 1_000)),
            Some(mk_vdf_proof(1)),
            Some(mk_vdf(0x11, 2_000)),
            Some(mk_vdf_proof(2)),
            Vec::new(),
            101,
            true,
            &[],
            Some(&tx),
            ph(0xBB),
            ph(0xCC),
            pool_target(),
            None,
            ph(0xDD),
            1_600_000_000,
            b"equiv-seed",
            farmer_sigs,
        )
        .expect("with_sigs path builds");

        assert_eq!(
            signer_block, with_sigs_block,
            "farmer-supplied path must be byte-identical to the signer path for the same signatures"
        );
    }
}
