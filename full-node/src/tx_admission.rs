// The ONE spend-bundle admission seam — chia `full_node.add_transaction` (full_node.py:2872-2988).
// Every transaction ingress shares it: HTTP `push_tx` (rpc.rs), the gossip validator worker
// (daemon.rs `spawn_tx_validator`, chia TransactionQueue consumer), and the wallet's p2p
// `SendTransaction` (code 48). One path means one set of semantics: the bundle→conditions CLVM run
// (mempool mode, next-block height) + aggregate-signature check happen server-side, admission goes
// through `Mempool::admit`, and a NEWLY resident item queues its `NewTransaction` re-broadcast —
// chia `broadcast_added_tx` fires identically for RPC- and p2p-admitted spends, and only for
// SUCCESS ("Only broadcast successful transactions, not pending ones. Otherwise it's a DOS
// vector", full_node.py:2977-2980).
//
// Idempotence is chia's exactly: a bundle already in the mempool short-circuits to success without
// re-validating and WITHOUT re-queueing an announce (full_node_api.py:1539-1543 fast path;
// full_node.py:2887-2889; the under-lock recheck at :2938-2940 covers the concurrent-submit race —
// mirrored here by rechecking residency under the same mempool lock that admits).

use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::blockchain::spend_bundle::SpendBundle;
use dg_xch_core::blockchain::tx_status::TXStatus;
use dg_xch_core::consensus::block_generator::{
    conditions_from_spend_bundle, validate_block_aggregate_signature,
};
use dg_xch_core::consensus::constants::ConsensusConstants;
use dg_xch_core::errors::ChiaError;
use dg_xch_core::protocols::full_node::NewTransaction;
use dg_xch_node::{Mempool, MempoolError};
use dg_xch_stores::{BlockStore, CoinStore, StoreError};
use std::fmt;
use tokio::sync::Mutex;

// Why a submitted bundle was not admitted, keeping the source error so each ingress surface can
// shape its own reply: `push_tx` maps to `RpcError` (HTTP 400/500 + human string), the p2p arm
// maps to chia's `TransactionAck` `(status, Err.name)`.
#[derive(Debug)]
pub(crate) enum TxAdmissionError {
    // The CLVM conditions run or the aggregate-signature check rejected the bundle.
    Validation(ChiaError),
    // `Mempool::admit` rejected it (double-spend, conflict, timelock, fee, ...).
    Mempool(MempoolError),
    // The peak lookup failed.
    Store(StoreError),
    // The store served a peak hash without its record.
    Corrupt(String),
}

impl fmt::Display for TxAdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TxAdmissionError::Validation(e) => write!(f, "invalid spend bundle: {e:?}"),
            TxAdmissionError::Mempool(e) => write!(f, "mempool rejected: {e}"),
            TxAdmissionError::Store(e) => write!(f, "store error: {e}"),
            TxAdmissionError::Corrupt(s) => write!(f, "corrupt store: {s}"),
        }
    }
}

impl TxAdmissionError {
    /// chia's `(MempoolInclusionStatus, Err.name)` for this reject — what
    /// `full_node_api.py::send_transaction` puts in the `TransactionAck` (`uint8(status.value)`,
    /// `error.name`). Validation rejects are always FAILED (chia pre_validate_spendbundle raises,
    /// add_transaction returns `(FAILED, e.code)`); mempool rejects follow
    /// [`MempoolError::ack`]'s pend-vs-fail split; infrastructure failures ack FAILED UNKNOWN
    /// (chia `_handle_one_transaction`'s catch-all, full_node.py:2949-2951).
    pub(crate) fn ack(&self) -> (TXStatus, &'static str) {
        match self {
            TxAdmissionError::Validation(e) => (TXStatus::FAILED, chia_validation_err_name(e)),
            TxAdmissionError::Mempool(e) => e.ack(),
            TxAdmissionError::Store(_) | TxAdmissionError::Corrupt(_) => {
                (TXStatus::FAILED, "UNKNOWN")
            }
        }
    }
}

// chia's `Err.name` for a validation-stage reject. `ChiaError` mirrors chia's `Err` enum
// numerically (core/src/errors.rs), so the names are the chia member names; anything the mempool
// path cannot produce a precise chia name for falls back to INVALID_SPEND_BUNDLE — chia's own
// soft-failure catch-all (`add_transaction` maps every `ValueError` from the validator to
// `Err.INVALID_SPEND_BUNDLE`, full_node.py:2907-2911).
fn chia_validation_err_name(e: &ChiaError) -> &'static str {
    match e {
        ChiaError::BadAggregateSignature => "BAD_AGGREGATE_SIGNATURE",
        ChiaError::WrongPuzzleHash => "WRONG_PUZZLE_HASH",
        ChiaError::DoubleSpend => "DOUBLE_SPEND",
        ChiaError::DuplicateOutput => "DUPLICATE_OUTPUT",
        ChiaError::BlockCostExceedsMax => "BLOCK_COST_EXCEEDS_MAX",
        ChiaError::InvalidCondition => "INVALID_CONDITION",
        ChiaError::CoinAmountExceedsMaximum => "COIN_AMOUNT_EXCEEDS_MAXIMUM",
        ChiaError::MintingCoin => "MINTING_COIN",
        ChiaError::ReserveFeeConditionFailed => "RESERVE_FEE_CONDITION_FAILED",
        _ => "INVALID_SPEND_BUNDLE",
    }
}

/// Validate `bundle` at next-block height and admit it to the mempool, queueing the
/// `NewTransaction` announce iff it became NEWLY resident. Returns the bundle name (the txid the
/// caller acks/serves). Chia-exact idempotence: an already-resident bundle is success with no
/// re-validation and no duplicate announce.
///
/// # Errors
/// [`TxAdmissionError`] with the validation, mempool, or store reason — see its `ack()` for the
/// chia wire mapping.
pub(crate) async fn admit_spend_bundle<S>(
    store: &S,
    mempool: &Mutex<Mempool>,
    constants: &ConsensusConstants,
    tx_announce: &Mutex<Vec<NewTransaction>>,
    bundle: SpendBundle,
) -> Result<Bytes32, TxAdmissionError>
where
    S: BlockStore + CoinStore + Sync,
{
    let name = bundle
        .name()
        .map_err(|e| TxAdmissionError::Mempool(MempoolError::Name(e.to_string())))?;
    // chia full_node_api.py:1539-1543 / full_node.py:2887-2891: already in the mempool → SUCCESS
    // immediately (and the seen entry clears so a post-block resubmit revalidates); otherwise a
    // SEEN id (recently validated, or known-invalid from a prior ValidationError) short-circuits
    // FAILED ALREADY_INCLUDING_TRANSACTION — no second CLVM/signature run.
    {
        let mut mp = mempool.lock().await;
        if mp.get(&name).is_some() {
            mp.remove_seen(&name);
            return Ok(name);
        }
        if mp.seen(&name) {
            return Err(TxAdmissionError::Mempool(MempoolError::AlreadyIncluding(
                name,
            )));
        }
    }
    let height = match store.get_peak().await.map_err(TxAdmissionError::Store)? {
        Some((hh, _)) => {
            store
                .get_block_record(&hh)
                .await
                .map_err(TxAdmissionError::Store)?
                .ok_or_else(|| TxAdmissionError::Corrupt(format!("peak record {hh} missing")))?
                .height
                + 1
        }
        None => 0,
    };
    // A ValidationError-class reject stays in the seen cache — "Keep known-invalid bundles in
    // seen-cache to prevent re-validation" (chia full_node.py:2917-2920).
    let conds = match conditions_from_spend_bundle(&bundle, height, constants) {
        Ok(conds) => conds,
        Err(e) => {
            mempool.lock().await.add_seen(name);
            return Err(TxAdmissionError::Validation(e));
        }
    };
    if let Err(e) = validate_block_aggregate_signature(&conds, &bundle.aggregated_signature, constants)
    {
        mempool.lock().await.add_seen(name);
        return Err(TxAdmissionError::Validation(e));
    }
    let cost = conds.cost;
    let fees = u64::try_from(conds.removal_amount.saturating_sub(conds.addition_amount))
        .unwrap_or(u64::MAX);
    {
        // One lock scope for recheck + admit: the concurrent-duplicate race resolves like chia's
        // under-lock recheck (full_node.py:2938-2940) — the loser sees the winner's item and
        // returns success without queueing a second announce.
        let mut mp = mempool.lock().await;
        if mp.get(&name).is_some() {
            return Ok(name);
        }
        mp.admit(store, bundle, conds)
            .await
            .map_err(TxAdmissionError::Mempool)?;
        // chia add_and_maybe_pop_seen after a successful validation (full_node.py:2925).
        mp.add_seen(name);
    }
    tx_announce.lock().await.push(NewTransaction {
        transaction_id: name,
        cost,
        fees,
    });
    Ok(name)
}
