use crate::blockchain::sized_bytes::Bytes48;
use crate::constants::AUG_SCHEME_DST;
use blst::BLST_ERROR;
use blst::min_pk::{PublicKey, SecretKey, Signature};

#[must_use]
pub fn verify_signature(public_key: &PublicKey, msg: &[u8], signature: &Signature) -> bool {
    matches!(
        signature.verify(
            true,
            msg,
            AUG_SCHEME_DST,
            &public_key.to_bytes(),
            public_key,
            true
        ),
        BLST_ERROR::BLST_SUCCESS
    )
}

pub fn aggregate_verify_signature(
    public_keys: &[Bytes48],
    msgs: &Vec<&[u8]>,
    signature: &Signature,
) -> bool {
    let mut new_msgs: Vec<Vec<u8>> = Vec::new();
    let mut keys: Vec<PublicKey> = Vec::new();
    for (key, msg) in public_keys.iter().zip(msgs) {
        let mut combined = Vec::new();
        combined.extend(*key);
        combined.extend(*msg);
        new_msgs.push(combined);
        keys.push((*key).into());
    }
    matches!(
        signature.aggregate_verify(
            true,
            &new_msgs.iter().map(Vec::as_slice).collect::<Vec<&[u8]>>(),
            AUG_SCHEME_DST,
            &keys.iter().collect::<Vec<&PublicKey>>(),
            true,
        ),
        BLST_ERROR::BLST_SUCCESS
    )
}

/// Aggregate a set of 96-byte compressed G2 signatures — chia `AugSchemeMPL.aggregate(sigs)`, the
/// block producer's step when combining the included mempool items' `aggregated_signature`s into
/// `TransactionsInfo.aggregated_signature` (chia/full_node/mempool.py `create_bundle_from_mempool_items`
/// / chia_rs `BlockBuilder.signature.aggregate`). An empty input yields the G2 identity (the infinity
/// point, `0xc0` prefix) — chia's `G2Element()` default. Inputs are NOT group-checked here: the
/// producer aggregates signatures that already passed `validate_block_aggregate_signature` at mempool
/// admission (chia aggregates unchecked in the same spot).
///
/// # Errors
/// Returns `Err` with the malformed signature's index if any input fails to deserialize.
pub fn aggregate_signatures<
    'a,
    I: IntoIterator<Item = &'a crate::blockchain::sized_bytes::Bytes96>,
>(
    signatures: I,
) -> Result<crate::blockchain::sized_bytes::Bytes96, String> {
    use blst::min_pk::AggregateSignature;
    let mut parsed: Vec<Signature> = Vec::new();
    for (i, sig) in signatures.into_iter().enumerate() {
        parsed.push(
            Signature::from_bytes(sig.as_ref())
                .map_err(|e| format!("signature {i} failed to deserialize: {e:?}"))?,
        );
    }
    let Some((first, rest)) = parsed.split_first() else {
        // chia G2Element() — the compressed infinity point.
        let mut infinity = [0_u8; 96];
        infinity[0] = 0xc0;
        return Ok(crate::blockchain::sized_bytes::Bytes96::from(infinity));
    };
    let mut agg = AggregateSignature::from_signature(first);
    for sig in rest {
        agg.add_signature(sig, false)
            .map_err(|e| format!("aggregation failed: {e:?}"))?;
    }
    Ok(crate::blockchain::sized_bytes::Bytes96::from(
        agg.to_signature().to_bytes(),
    ))
}

#[must_use]
pub fn sign(local_sk: &SecretKey, msg: &[u8]) -> Signature {
    local_sk.sign(msg, AUG_SCHEME_DST, &local_sk.sk_to_pk().to_bytes())
}

#[must_use]
pub fn sign_prepend(local_sk: &SecretKey, msg: &[u8], prepend_pk: &PublicKey) -> Signature {
    local_sk.sign(msg, AUG_SCHEME_DST, &prepend_pk.to_bytes())
}
