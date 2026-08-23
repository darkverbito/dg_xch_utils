use crate::discriminant::{bigint_from_be, bit_len, create_discriminant_int};
use crate::error::{Error, Result};
use crate::form::{
    B_BYTES, FORM_SIZE, Form, fast_pow_form_pair_with, fast_pow_form_with, fast_pow_u64_mod, get_b,
    get_block, nucomp_bound,
};
use num_bigint::BigInt;
use num_traits::One;

const SEGMENT_LEN: usize = 8 + B_BYTES + FORM_SIZE;

pub fn verify_n_wesolowski(
    discriminant: &[u8],
    x_s: &[u8],
    proof: &[u8],
    num_iterations: u64,
    recursion: u64,
) -> bool {
    verify_n_wesolowski_result(discriminant, x_s, proof, num_iterations, recursion).is_ok()
}

/// Whole-proof verification memo — the port of the reference node's `@lru_cache(maxsize=1000)`
/// on `verify_vdf` (chia-blockchain `chia/types/blockchain_format/vdf.py`). The recurrence is
/// real and measured on OUR sync path (a tx-dense window replay, window-drain probe): ~9–12% of a
/// 32-block window's queued proofs are exact repeats — blocks sharing a signage point carry
/// byte-identical sp VDF proofs — and the live tip re-verifies the same proofs again across the
/// unfinished/finished/gossip paths. The key is the EXACT argument bytes, each variable-length
/// field length-prefixed (injective — no hash-collision surface on a consensus gate); the value
/// is the deterministic pure-function result, `true` and `false` alike, exactly as the
/// reference node caches it.
const VERIFY_MEMO_CAPACITY: usize = 1000;

struct VerifyMemo {
    map: std::collections::HashMap<Vec<u8>, (bool, u64)>,
    tick: u64,
}

fn verify_memo() -> &'static std::sync::Mutex<VerifyMemo> {
    static MEMO: std::sync::OnceLock<std::sync::Mutex<VerifyMemo>> = std::sync::OnceLock::new();
    MEMO.get_or_init(|| {
        std::sync::Mutex::new(VerifyMemo {
            map: std::collections::HashMap::with_capacity(VERIFY_MEMO_CAPACITY),
            tick: 0,
        })
    })
}

fn verify_memo_key(
    challenge: &[u8],
    x_s: &[u8],
    proof: &[u8],
    discriminant_size_bits: usize,
    num_iterations: u64,
    recursion: u64,
) -> Vec<u8> {
    let mut key = Vec::with_capacity(challenge.len() + x_s.len() + proof.len() + 48);
    for field in [challenge, x_s, proof] {
        key.extend_from_slice(&(field.len() as u64).to_be_bytes());
        key.extend_from_slice(field);
    }
    key.extend_from_slice(&(discriminant_size_bits as u64).to_be_bytes());
    key.extend_from_slice(&num_iterations.to_be_bytes());
    key.extend_from_slice(&recursion.to_be_bytes());
    key
}

fn verify_memo_get(key: &[u8]) -> Option<bool> {
    let mut memo = verify_memo().lock().ok()?;
    memo.tick += 1;
    let tick = memo.tick;
    let (result, last_used) = memo.map.get_mut(key)?;
    *last_used = tick;
    Some(*result)
}

fn verify_memo_put(key: Vec<u8>, result: bool) {
    let Ok(mut memo) = verify_memo().lock() else {
        return;
    };
    if memo.map.len() >= VERIFY_MEMO_CAPACITY && !memo.map.contains_key(&key) {
        // Evict the least-recently-used entry: O(capacity) scan on insert, but the map is
        // bounded at 1000 entries and the scan is a u64 compare per entry.
        if let Some(oldest) = memo
            .map
            .iter()
            .min_by_key(|(_, (_, used))| *used)
            .map(|(k, _)| k.clone())
        {
            memo.map.remove(&oldest);
        }
    }
    memo.tick += 1;
    let tick = memo.tick;
    memo.map.insert(key, (result, tick));
}

pub fn verify_vdf(
    challenge: &[u8],
    x_s: &[u8],
    proof: &[u8],
    discriminant_size_bits: usize,
    num_iterations: u64,
    recursion: u64,
) -> bool {
    verify_vdf_impl(
        challenge,
        x_s,
        proof,
        discriminant_size_bits,
        num_iterations,
        recursion,
        true,
    )
}

/// [`verify_vdf`] without the two-thread split inside each segment exponentiation. Same result,
/// same memo — group arithmetic is scheduling-independent. For SATURATED batch drains: when a
/// window drain already runs one proof per core, the inner spawn adds a second thread per worker
/// (2× oversubscription) and a thread spawn/join per segment while buying no wall — measured
/// +17% process CPU over the same work verified serially (a tx-dense window drain probe, 12-CPU cgroup).
/// The parallel variant remains right for latency-bound single proofs (the live tip).
pub fn verify_vdf_serial(
    challenge: &[u8],
    x_s: &[u8],
    proof: &[u8],
    discriminant_size_bits: usize,
    num_iterations: u64,
    recursion: u64,
) -> bool {
    verify_vdf_impl(
        challenge,
        x_s,
        proof,
        discriminant_size_bits,
        num_iterations,
        recursion,
        false,
    )
}

fn verify_vdf_impl(
    challenge: &[u8],
    x_s: &[u8],
    proof: &[u8],
    discriminant_size_bits: usize,
    num_iterations: u64,
    recursion: u64,
    parallel: bool,
) -> bool {
    let key = verify_memo_key(
        challenge,
        x_s,
        proof,
        discriminant_size_bits,
        num_iterations,
        recursion,
    );
    if let Some(hit) = verify_memo_get(&key) {
        return hit;
    }
    let result = verify_vdf_uncached(
        challenge,
        x_s,
        proof,
        discriminant_size_bits,
        num_iterations,
        recursion,
        parallel,
    );
    verify_memo_put(key, result);
    result
}

fn verify_vdf_uncached(
    challenge: &[u8],
    x_s: &[u8],
    proof: &[u8],
    discriminant_size_bits: usize,
    num_iterations: u64,
    recursion: u64,
    parallel: bool,
) -> bool {
    let Ok(discriminant) = create_discriminant_int(challenge, discriminant_size_bits) else {
        return false;
    };
    check_n_wesolowski_impl(
        &discriminant,
        x_s,
        proof,
        num_iterations,
        recursion,
        parallel,
    )
    .is_ok()
}

pub fn prove(
    challenge: &[u8],
    x_s: &[u8],
    discriminant_size_bits: usize,
    num_iterations: u64,
) -> Option<Vec<u8>> {
    prove_result(challenge, x_s, discriminant_size_bits, num_iterations).ok()
}

pub fn verify_n_wesolowski_result(
    discriminant: &[u8],
    x_s: &[u8],
    proof: &[u8],
    num_iterations: u64,
    recursion: u64,
) -> Result<()> {
    let discriminant = -bigint_from_be(discriminant);
    check_n_wesolowski(&discriminant, x_s, proof, num_iterations, recursion)
}

pub fn prove_result(
    challenge: &[u8],
    x_s: &[u8],
    discriminant_size_bits: usize,
    num_iterations: u64,
) -> Result<Vec<u8>> {
    let discriminant = create_discriminant_int(challenge, discriminant_size_bits)?;
    let x = Form::deserialize(&discriminant, x_s)?;
    prove_with_discriminant(&discriminant, &x, num_iterations)
}

pub fn check_n_wesolowski(
    discriminant: &BigInt,
    x_s: &[u8],
    proof: &[u8],
    iterations: u64,
    depth: u64,
) -> Result<()> {
    check_n_wesolowski_impl(discriminant, x_s, proof, iterations, depth, true)
}

fn check_n_wesolowski_impl(
    discriminant: &BigInt,
    x_s: &[u8],
    proof: &[u8],
    mut iterations: u64,
    depth: u64,
    parallel: bool,
) -> Result<()> {
    if bit_len(discriminant) == 0 || bit_len(discriminant) > 1024 {
        return Err(Error::InvalidDiscriminant);
    }

    let expected_len = FORM_SIZE
        .checked_mul(2)
        .and_then(|base| base.checked_add(SEGMENT_LEN.checked_mul(depth as usize)?))
        .ok_or(Error::InvalidProofLength)?;
    if proof.len() != expected_len {
        return Err(Error::InvalidProofLength);
    }

    let mut offset = proof.len();
    let mut x = Form::deserialize(discriminant, x_s)?;
    // The discriminant and its NUCOMP bound are shared by every segment and the final Wesolowski
    // check — compute the bound once per proof, as the prover already does. The verifier was
    // paying two 1024-bit sqrts per exponentiation plus a b²−4ac discriminant recompute in every
    // segment's composition.
    let nl = nucomp_bound(discriminant);
    while offset > FORM_SIZE * 2 {
        offset -= SEGMENT_LEN;
        let segment_iters = u64::from_be_bytes(
            proof[offset..offset + 8]
                .try_into()
                .expect("slice has 8 bytes"),
        );
        let b = bigint_from_be(&proof[offset + 8..offset + 8 + B_BYTES]);
        let proof_form = Form::deserialize(
            discriminant,
            &proof[offset + 8 + B_BYTES..offset + SEGMENT_LEN],
        )?;
        x = verify_segment(
            discriminant,
            &nl,
            &x,
            &proof_form,
            &b,
            segment_iters,
            parallel,
        )?;

        if segment_iters > iterations {
            return Err(Error::InvalidSegmentIterations);
        }
        iterations -= segment_iters;
    }

    let y = Form::deserialize(discriminant, &proof[..FORM_SIZE])?;
    let witness = Form::deserialize(discriminant, &proof[FORM_SIZE..FORM_SIZE * 2])?;
    verify_wesolowski(discriminant, &nl, &x, &y, &witness, iterations, parallel)
}

// The Wesolowski check consumes the PRODUCT witness^b · x^r, never the individual powers.
// `parallel` chooses how the product is evaluated — group-identical results either way:
//  * parallel (single-proof latency, the live tip): the two exponentiations are independent, so
//    the two-thread split puts one ~336-op chain on each thread (critical path ≈ one chain) and
//    composes the results.
//  * serial (saturated batch drains, where throughput ≡ total work): the fused Straus/Shamir
//    chain shares the squaring run between the two exponents — ~411 group ops against the
//    two-chain 673 (0.61×), on one thread with no spawn/join.
fn pow_pair_product(
    witness: &Form,
    witness_exp: &BigInt,
    x: &Form,
    x_exp: &BigInt,
    discriminant: &BigInt,
    nl: &BigInt,
    parallel: bool,
) -> Result<Form> {
    if parallel {
        let (f1, f2) = std::thread::scope(|s| {
            let h1 = s.spawn(|| fast_pow_form_with(witness, discriminant, nl, witness_exp));
            let f2 = fast_pow_form_with(x, discriminant, nl, x_exp);
            (h1.join().unwrap_or(Err(Error::InvalidForm)), f2)
        });
        f1?.multiply_with(&f2?, discriminant, nl)
    } else {
        fast_pow_form_pair_with(witness, witness_exp, x, x_exp, discriminant, nl)
    }
}

fn verify_segment(
    discriminant: &BigInt,
    nl: &BigInt,
    x: &Form,
    witness: &Form,
    b: &BigInt,
    iterations: u64,
    parallel: bool,
) -> Result<Form> {
    let r = fast_pow_u64_mod(2, iterations, b)?;
    let y = pow_pair_product(witness, b, x, &r, discriminant, nl, parallel)?;
    if get_b(discriminant, x, &y)? == *b {
        Ok(y)
    } else {
        Err(Error::InvalidForm)
    }
}

fn verify_wesolowski(
    discriminant: &BigInt,
    nl: &BigInt,
    x: &Form,
    y: &Form,
    witness: &Form,
    iterations: u64,
    parallel: bool,
) -> Result<()> {
    let b = get_b(discriminant, x, y)?;
    let r = fast_pow_u64_mod(2, iterations, &b)?;
    if pow_pair_product(witness, &b, x, &r, discriminant, nl, parallel)? == *y {
        Ok(())
    } else {
        Err(Error::InvalidForm)
    }
}

fn prove_with_discriminant(
    discriminant: &BigInt,
    x: &Form,
    num_iterations: u64,
) -> Result<Vec<u8>> {
    let d_bits = bit_len(discriminant);
    let mut y = x.clone();
    let (l, k) = approximate_parameters(num_iterations)?;
    let kl = k.checked_mul(l).ok_or(Error::InvalidProofParameters)?;
    let intermediate_count = num_iterations.div_ceil(kl);
    let mut intermediates = Vec::with_capacity(intermediate_count as usize);
    // NUCOMP bound computed once for the whole iterated-squaring run.
    let nl = nucomp_bound(discriminant);

    for i in 0..num_iterations {
        if i % kl == 0 {
            intermediates.push(y.clone());
        }
        y = y.square_with(discriminant, &nl)?;
    }

    let witness = generate_wesolowski(discriminant, &y, x, &intermediates, num_iterations, k, l)?;
    let mut out = y.serialize(d_bits)?.to_vec();
    out.extend_from_slice(&witness.serialize(d_bits)?);
    Ok(out)
}

fn generate_wesolowski(
    discriminant: &BigInt,
    y: &Form,
    x_init: &Form,
    intermediates: &[Form],
    num_iterations: u64,
    k: u64,
    l: u64,
) -> Result<Form> {
    let b = get_b(discriminant, x_init, y)?;
    let k1 = k / 2;
    let k0 = k - k1;
    let bucket_count = 1usize
        .checked_shl(k.try_into().map_err(|_| Error::InvalidProofParameters)?)
        .ok_or(Error::InvalidProofParameters)?;
    let bucket0_count = 1usize
        .checked_shl(k0.try_into().map_err(|_| Error::InvalidProofParameters)?)
        .ok_or(Error::InvalidProofParameters)?;
    let bucket1_count = 1usize
        .checked_shl(k1.try_into().map_err(|_| Error::InvalidProofParameters)?)
        .ok_or(Error::InvalidProofParameters)?;

    // NUCOMP bound computed once for every loop composition below.
    let nl = nucomp_bound(discriminant);
    let mut x = Form::identity(discriminant)?;
    for j in (0..l).rev() {
        x = fast_pow_form_with(&x, discriminant, &nl, &(BigInt::one() << k as usize))?;

        let mut ys = vec![Form::identity(discriminant)?; bucket_count];
        let chunks = num_iterations.div_ceil(k * l);
        for i in 0..chunks {
            if num_iterations >= k * (i * l + j + 1) {
                let block = get_block(i * l + j, k, num_iterations, &b)?;
                let block_index =
                    usize::try_from(block).map_err(|_| Error::InvalidProofParameters)?;
                if block_index >= ys.len() {
                    return Err(Error::InvalidProofParameters);
                }
                ys[block_index] = ys[block_index].multiply_with(
                    intermediates
                        .get(i as usize)
                        .ok_or(Error::InvalidProofParameters)?,
                    discriminant,
                    &nl,
                )?;
            }
        }

        for b1 in 0..bucket1_count {
            let mut z = Form::identity(discriminant)?;
            for b0 in 0..bucket0_count {
                z = z.multiply_with(&ys[b1 * bucket0_count + b0], discriminant, &nl)?;
            }
            z = fast_pow_form_with(
                &z,
                discriminant,
                &nl,
                &BigInt::from((b1 as u64) * (1u64 << k0)),
            )?;
            x = x.multiply_with(&z, discriminant, &nl)?;
        }

        for b0 in 0..bucket0_count {
            let mut z = Form::identity(discriminant)?;
            for b1 in 0..bucket1_count {
                z = z.multiply_with(&ys[b1 * bucket0_count + b0], discriminant, &nl)?;
            }
            z = fast_pow_form_with(&z, discriminant, &nl, &BigInt::from(b0 as u64))?;
            x = x.multiply_with(&z, discriminant, &nl)?;
        }
    }

    x.reduce();
    Ok(x)
}

fn approximate_parameters(t: u64) -> Result<(u64, u64)> {
    if t == 0 {
        return Ok((1, 1));
    }

    let log_memory = 23.25349666_f64;
    let log_t = (t as f64).log2();
    let l = if log_t - log_memory > 0.000001 {
        2_f64.powf(log_memory - 20.0).ceil() as u64
    } else {
        1
    };
    let intermediate = (t as f64) * std::f64::consts::LN_2 / (2.0 * l as f64);
    let k = if intermediate <= 1.0 {
        1
    } else {
        (intermediate.ln() - intermediate.ln().ln() + 0.25)
            .round()
            .max(1.0) as u64
    };

    if l == 0 || k == 0 || k > 20 {
        return Err(Error::InvalidProofParameters);
    }
    Ok((l, k))
}

#[cfg(test)]
fn verify_memo_len() -> usize {
    verify_memo().lock().map(|m| m.map.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discriminant::create_discriminant_int;
    use crate::form::{B_BYTES, get_b};

    /// Differential gate for the whole-proof memo: the memoized entry point must agree with the
    /// uncached computation on the miss path, the hit path, and under a single-byte proof
    /// corruption — a corrupted proof shares no key with the valid one, so it can never inherit
    /// a cached `true`.
    #[test]
    fn memoized_verify_vdf_is_identical_to_uncached() {
        let challenge =
            hex::decode("9f1fbdf9b1a0b6912cd5e2a4b40a2ffb1810b513994b1dd6c4e6df9c30de5f6e")
                .expect("challenge hex is valid");
        let discriminant = create_discriminant_int(&challenge, 1024).unwrap();
        let mut x_s = [0u8; 100];
        x_s[0] = 0x08;
        let x = Form::deserialize(&discriminant, &x_s).unwrap();
        let proof = prove_with_discriminant(&discriminant, &x, 100).unwrap();

        // Miss, then hit — both must equal the uncached result.
        assert!(verify_vdf(&challenge, &x_s, &proof, 1024, 100, 0));
        assert!(verify_vdf(&challenge, &x_s, &proof, 1024, 100, 0));
        assert!(verify_vdf_uncached(
            &challenge, &x_s, &proof, 1024, 100, 0, true
        ));

        // An invalid proof of the correct length: a distinct key, recomputed, false — on the
        // miss and the hit path (i.e. `false` results are cached too, and a bad proof can never
        // inherit the valid proof's cached `true`).
        let bad = vec![0xFFu8; proof.len()];
        assert!(!verify_vdf(&challenge, &x_s, &bad, 1024, 100, 0));
        assert!(!verify_vdf(&challenge, &x_s, &bad, 1024, 100, 0));
        assert!(!verify_vdf_uncached(
            &challenge, &x_s, &bad, 1024, 100, 0, true
        ));

        // A changed iteration count is a distinct key (length-prefixed fields, fixed-width
        // trailer — no concatenation ambiguity), so the cached `true` above cannot leak here.
        assert!(!verify_vdf(&challenge, &x_s, &proof, 1024, 101, 0));
    }

    /// The serial (no inner threads) verification path must be result-identical to the parallel
    /// path, below the memo — valid proof, valid recursive proof, and an invalid proof alike.
    #[test]
    fn serial_verification_is_result_identical_to_parallel() {
        let challenge =
            hex::decode("1f0c94d5d1f5ea25be3b04e04d17806bcc9a0dbcdcc16346eb388937b5981c37")
                .expect("challenge hex is valid");
        let discriminant = create_discriminant_int(&challenge, 1024).unwrap();
        let mut x_s = [0u8; 100];
        x_s[0] = 0x08;
        let x = Form::deserialize(&discriminant, &x_s).unwrap();
        let proof = prove_with_discriminant(&discriminant, &x, 73).unwrap();

        assert!(check_n_wesolowski_impl(&discriminant, &x_s, &proof, 73, 0, true).is_ok());
        assert!(check_n_wesolowski_impl(&discriminant, &x_s, &proof, 73, 0, false).is_ok());
        // Wrong iteration count: both paths must reject.
        assert!(check_n_wesolowski_impl(&discriminant, &x_s, &proof, 74, 0, true).is_err());
        assert!(check_n_wesolowski_impl(&discriminant, &x_s, &proof, 74, 0, false).is_err());
        // The public serial entry point (through the memo) agrees as well.
        assert!(verify_vdf_serial(&challenge, &x_s, &proof, 1024, 73, 0));
    }

    /// The memo stays bounded past capacity.
    #[test]
    fn verify_memo_stays_bounded() {
        let challenge =
            hex::decode("8be26af52b34a1a7c47a35c7f0c1add793d5b6e2b0e56e6e970cbd6bd4e17e2a")
                .expect("challenge hex is valid");
        let x_s = [0u8; 100];
        // Wrong-length proofs fail fast (length gate precedes any class-group work), each with a
        // unique key via the iteration count.
        for i in 0..(VERIFY_MEMO_CAPACITY as u64 + 100) {
            let _ = verify_vdf(&challenge, &x_s, &[0u8; 8], 1024, i, 0);
        }
        assert!(verify_memo_len() <= VERIFY_MEMO_CAPACITY);
    }

    #[test]
    fn recursive_proof_verifies_segment_before_final_witness() {
        let challenge =
            hex::decode("ccd5bb71183532bff220ba46c268991a3ff07eb358e8255a65c30a2dce0e5fbb")
                .expect("challenge hex is valid");
        let discriminant = create_discriminant_int(&challenge, 1024).unwrap();
        let mut x_s = [0u8; 100];
        x_s[0] = 0x08;
        let x = Form::deserialize(&discriminant, &x_s).unwrap();

        let segment_iterations = 24;
        let final_iterations = 31;
        let segment_proof = prove_with_discriminant(&discriminant, &x, segment_iterations).unwrap();
        let segment_y = Form::deserialize(&discriminant, &segment_proof[..FORM_SIZE]).unwrap();
        let segment_witness = &segment_proof[FORM_SIZE..FORM_SIZE * 2];
        let segment_b = get_b(&discriminant, &x, &segment_y).unwrap();

        let mut recursive_proof =
            prove_with_discriminant(&discriminant, &segment_y, final_iterations).unwrap();
        recursive_proof.extend_from_slice(&segment_iterations.to_be_bytes());
        recursive_proof.extend_from_slice(&fixed_be_bytes(&segment_b, B_BYTES));
        recursive_proof.extend_from_slice(segment_witness);

        check_n_wesolowski(
            &discriminant,
            &x_s,
            &recursive_proof,
            segment_iterations + final_iterations,
            1,
        )
        .expect("depth-1 proof should verify");
    }

    // Whole-proof serial-verify wall time on the chia vdf.txt fixture (129,499,136 iterations,
    // depth 0) — the saturated-drain per-proof unit cost, measured below the memo. Run:
    //   cargo test --release -p dg_xch_vdf --lib bench_serial_whole_proof_verify -- --ignored --nocapture
    #[test]
    #[ignore = "timing tool"]
    fn bench_serial_whole_proof_verify() {
        let challenge =
            hex::decode("9104c5b5e45d48f374efa0488fe6a617790e9aecb3c9cddec06809b09f45ce9b")
                .expect("challenge hex is valid");
        let discriminant = create_discriminant_int(&challenge, 1024).unwrap();
        let mut x_s = [0u8; 100];
        x_s[0] = 0x08;
        let proof = hex::decode(
            "0200553bf0f382fc65a94f20afad5dbce2c1ee8ba3bf93053559ac9960c8fd80ac2222e9b649701a4141a4d8999f0dbfe0c39ea744096598a7528328e5199f0aa30aec8aae8ab5018bf1245329a8272ddff1afbd87ad2eaba1b7fd57bd25edc62e0b010000003f0ffcd0dc307a2aa4678bafba661c77d176ef23afc86e7ea9f4f9eac52b8e1850748019245ecc96547da9b731dc72cded5582a9b0c63e13fd42446c7b28b41d3ded1d0b666d5ddb5b29719e4ebe70969e67e42ddd8591eae60d83dbe619f1250400",
        )
        .expect("proof hex is valid");
        // Warm (faults in the discriminant cache).
        assert!(
            check_n_wesolowski_impl(&discriminant, &x_s, &proof, 129_499_136, 0, false).is_ok()
        );
        const N: u32 = 20;
        let start = std::time::Instant::now();
        for _ in 0..N {
            assert!(
                check_n_wesolowski_impl(&discriminant, &x_s, &proof, 129_499_136, 0, false).is_ok()
            );
        }
        eprintln!("SERIAL-VERIFY: {:?}/op over {N} ops", start.elapsed() / N);
    }

    fn fixed_be_bytes(value: &BigInt, size: usize) -> Vec<u8> {
        let (_, bytes) = value.to_bytes_be();
        assert!(bytes.len() <= size);
        let mut out = vec![0u8; size - bytes.len()];
        out.extend_from_slice(&bytes);
        out
    }
}
