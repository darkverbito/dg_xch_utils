use crate::error::{Error, Result};
use num_bigint::{BigInt, BigUint, Sign};
use num_integer::Integer;
use num_traits::{One, Signed, Zero};
use sha2::{Digest, Sha256};
use std::sync::Mutex;

const MAX_DISCRIMINANT_SIZE_BITS: usize = 1024;

/// Bounded challenge→prime memo for the discriminant derivation. Within a sub-slot every VDF on a
/// chain shares that chain's challenge, so the identical ~1024-bit Fiat-Shamir prime search recurs
/// per proof check (measured on mainnet blocks 0..=1023: 5,098 derivations collapse to 1,094
/// unique seeds — 4.7x redundancy, the hottest challenge derived 110 times, ~381 primality
/// candidates each). Mirrors the reference node's `@lru_cache(maxsize=200)` on `get_discriminant`
/// (chia-blockchain, chia/types/blockchain_format/vdf.py). `get_b`'s 264-bit hash_prime inputs
/// hash the proof forms themselves and were measured ~90% unique — deliberately NOT cached.
const DISCRIMINANT_CACHE_CAPACITY: usize = 200;

/// `(seed, size_bits, prime)`, least-recently-used at the front. Linear scan + Vec rotate: at 200
/// entries the whole structure is a few KB and a lookup is microseconds against a ~56 ms miss.
static DISCRIMINANT_CACHE: Mutex<Vec<(Vec<u8>, usize, BigInt)>> = Mutex::new(Vec::new());

fn discriminant_cache_get(seed: &[u8], size_bits: usize) -> Option<BigInt> {
    let mut cache = DISCRIMINANT_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let idx = cache
        .iter()
        .position(|(s, bits, _)| *bits == size_bits && s == seed)?;
    let entry = cache.remove(idx);
    let prime = entry.2.clone();
    cache.push(entry);
    Some(prime)
}

fn discriminant_cache_put(seed: &[u8], size_bits: usize, prime: &BigInt) {
    let mut cache = DISCRIMINANT_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Concurrent verifiers of the same challenge race the derivation; first insert wins, the
    // duplicate result is identical by construction (the derivation is deterministic).
    if cache
        .iter()
        .any(|(s, bits, _)| *bits == size_bits && s == seed)
    {
        return;
    }
    if cache.len() >= DISCRIMINANT_CACHE_CAPACITY {
        cache.remove(0);
    }
    cache.push((seed.to_vec(), size_bits, prime.clone()));
}

pub fn create_discriminant(seed: &[u8], result: &mut [u8]) -> bool {
    match create_discriminant_bytes(seed, result.len() * 8) {
        Ok(discriminant) if discriminant.len() <= result.len() => {
            result.fill(0);
            let offset = result.len() - discriminant.len();
            result[offset..].copy_from_slice(&discriminant);
            true
        }
        _ => false,
    }
}

pub fn create_discriminant_bytes(seed: &[u8], size_bits: usize) -> Result<Vec<u8>> {
    let discriminant = create_discriminant_int(seed, size_bits)?;
    let (_, bytes) = (-discriminant).to_bytes_be();
    Ok(bytes)
}

pub(crate) fn create_discriminant_int(seed: &[u8], size_bits: usize) -> Result<BigInt> {
    if size_bits == 0 || size_bits > MAX_DISCRIMINANT_SIZE_BITS || !size_bits.is_multiple_of(8) {
        return Err(Error::InvalidDiscriminantSize);
    }
    if seed.is_empty() {
        return Err(Error::EmptySeed);
    }

    if let Some(prime) = discriminant_cache_get(seed, size_bits) {
        return Ok(-prime);
    }
    let prime = hash_prime(seed, size_bits, &[0, 1, 2, size_bits - 1]);
    discriminant_cache_put(seed, size_bits, &prime);
    Ok(-prime)
}

pub(crate) fn hash_prime(seed: &[u8], size_bits: usize, bitmask: &[usize]) -> BigInt {
    let mut sprout = seed.to_vec();

    #[cfg(feature = "hashprime-probe")]
    let mut probe_candidates: u64 = 0;

    loop {
        let mut blob = Vec::with_capacity(size_bits / 8);
        while blob.len() * 8 < size_bits {
            increment_big_endian(&mut sprout);
            let hash = Sha256::digest(&sprout);
            let remaining = size_bits / 8 - blob.len();
            blob.extend_from_slice(&hash[..remaining.min(hash.len())]);
        }

        let mut candidate = BigUint::from_bytes_be(&blob);
        for bit in bitmask {
            candidate.set_bit((*bit).try_into().expect("bit index fits u64"), true);
        }
        candidate.set_bit(0, true);

        #[cfg(feature = "hashprime-probe")]
        {
            probe_candidates += 1;
        }

        if is_probable_prime(&candidate) {
            #[cfg(feature = "hashprime-probe")]
            {
                let seed_h = Sha256::digest(seed);
                eprintln!(
                    "HASHPRIME bits={} seedlen={} seedh={:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x} candidates={}",
                    size_bits,
                    seed.len(),
                    seed_h[0],
                    seed_h[1],
                    seed_h[2],
                    seed_h[3],
                    seed_h[4],
                    seed_h[5],
                    seed_h[6],
                    seed_h[7],
                    probe_candidates
                );
            }
            return BigInt::from_biguint(Sign::Plus, candidate);
        }
    }
}

fn increment_big_endian(bytes: &mut [u8]) {
    for byte in bytes.iter_mut().rev() {
        *byte = byte.wrapping_add(1);
        if *byte != 0 {
            break;
        }
    }
}

fn is_probable_prime(n: &BigUint) -> bool {
    // GMP mpz_probab_prime_p with reps=24: small-prime screen + Baillie-PSW exactly (MR base 2 +
    // strong Lucas) — extra random-base MR rounds run only for reps > 24 (vendored
    // gmp-6.3.0-c/mpz/millerrabin.c: `reps -= 24; if (reps > 0)`). This matches the consensus
    // reference: chiavdf's `integer::prime()` is `is_prime_bpsw` (chiavdf src/integer_common.h →
    // src/primetest.h, a "strengthened Baillie-PSW" with no extra MR rounds). Exceeding the
    // reference is not extra safety here: a BPSW pseudoprime (none known) that chiavdf accepts
    // but an extra round rejects would make this search continue to a DIFFERENT prime — a
    // consensus fork. Parity means exactly BPSW.
    let g = rug::Integer::from_digits(&n.to_bytes_be(), rug::integer::Order::MsfBe);
    g.is_probably_prime(24) != rug::integer::IsPrime::No
}

#[allow(dead_code)]
fn is_probable_prime_native(n: &BigUint) -> bool {
    if *n < BigUint::from(2u8) {
        return false;
    }

    const SMALL_PRIMES: [u64; 64] = [
        2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47, 53, 59, 61, 67, 71, 73, 79, 83, 89,
        97, 101, 103, 107, 109, 113, 127, 131, 137, 139, 149, 151, 157, 163, 167, 173, 179, 181,
        191, 193, 197, 199, 211, 223, 227, 229, 233, 239, 241, 251, 257, 263, 269, 271, 277, 281,
        283, 293, 307, 311,
    ];

    // Chunked trial division: pack the small primes into u64 products so each candidate pays a
    // handful of bigint-by-word remainders instead of one bigint division per prime (the prime
    // search does this for ~90 candidates per hash_prime). Identical accept/reject semantics:
    // n ≡ 0 (mod p) still returns `n == p`.
    let mut i = 0;
    while i < SMALL_PRIMES.len() {
        let start = i;
        let mut chunk: u64 = 1;
        while i < SMALL_PRIMES.len() {
            match chunk.checked_mul(SMALL_PRIMES[i]) {
                Some(c) => {
                    chunk = c;
                    i += 1;
                }
                None => break,
            }
        }
        let rem = (n % BigUint::from(chunk))
            .to_u64_digits()
            .first()
            .copied()
            .unwrap_or(0);
        for &p in &SMALL_PRIMES[start..i] {
            if rem.is_multiple_of(p) {
                return *n == BigUint::from(p);
            }
        }
    }

    miller_rabin(
        n,
        &[
            2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47, 53, 59, 61, 67, 71, 73, 79, 83,
            89, 97, 101, 103, 107, 109, 113, 127,
        ],
    )
}

fn miller_rabin(n: &BigUint, bases: &[u64]) -> bool {
    let one = BigUint::one();
    let two = BigUint::from(2u8);
    let n_minus_one = n - &one;
    let mut d = n_minus_one.clone();
    let mut s = 0usize;
    while d.is_even() {
        d >>= 1;
        s += 1;
    }

    'bases: for base in bases {
        let a = BigUint::from(*base);
        if &a >= n {
            continue;
        }
        let mut x = a.modpow(&d, n);
        if x == one || x == n_minus_one {
            continue;
        }
        for _ in 1..s {
            x = x.modpow(&two, n);
            if x == n_minus_one {
                continue 'bases;
            }
        }
        return false;
    }
    true
}

pub(crate) fn bigint_from_be(bytes: &[u8]) -> BigInt {
    BigInt::from_biguint(Sign::Plus, BigUint::from_bytes_be(bytes))
}

pub(crate) fn bigint_to_fixed_le(value: &BigInt, size: usize) -> Option<Vec<u8>> {
    if value.is_negative() {
        return None;
    }
    let (_, mut bytes) = value.to_bytes_le();
    if bytes.len() > size {
        return None;
    }
    bytes.resize(size, 0);
    Some(bytes)
}

pub(crate) fn bigint_from_le(bytes: &[u8]) -> BigInt {
    BigInt::from_biguint(Sign::Plus, BigUint::from_bytes_le(bytes))
}

pub(crate) fn bit_len(value: &BigInt) -> usize {
    if value.is_zero() {
        0
    } else {
        value.abs().to_biguint().expect("abs is nonnegative").bits() as usize
    }
}

pub(crate) fn u64_low_word(value: &BigInt) -> u64 {
    value
        .to_biguint()
        .and_then(|v| v.iter_u64_digits().next())
        .unwrap_or(0)
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    /// Differential gate for the memo: the cached path must be byte-identical to a direct
    /// (uncached) hash_prime derivation, on both the miss and the hit path.
    #[test]
    fn cached_discriminant_is_byte_identical_to_direct_derivation() {
        for i in 0..3u8 {
            let seed = [0x40 | i; 32];
            let direct = -hash_prime(&seed, 512, &[0, 1, 2, 511]);
            let first = create_discriminant_int(&seed, 512).expect("derivation succeeds");
            let second = create_discriminant_int(&seed, 512).expect("derivation succeeds");
            assert_eq!(direct, first, "miss path must equal direct derivation");
            assert_eq!(first, second, "hit path must equal miss path");
        }
    }

    /// The cache stays bounded past capacity, and an evicted entry re-derives byte-identically.
    #[test]
    fn cache_stays_bounded_and_eviction_rederives_identically() {
        let mut first_seed = [0u8; 32];
        first_seed[0] = 0x80;
        let direct = -hash_prime(&first_seed, 512, &[0, 1, 2, 511]);
        assert_eq!(
            create_discriminant_int(&first_seed, 512).expect("derivation succeeds"),
            direct
        );

        for i in 0..(DISCRIMINANT_CACHE_CAPACITY as u32 + 8) {
            let mut seed = [0u8; 32];
            seed[0] = 0x81;
            seed[28..32].copy_from_slice(&i.to_be_bytes());
            create_discriminant_int(&seed, 512).expect("derivation succeeds");
        }

        {
            let cache = DISCRIMINANT_CACHE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(
                cache.len() <= DISCRIMINANT_CACHE_CAPACITY,
                "cache exceeded its bound: {}",
                cache.len()
            );
        }

        // first_seed has been evicted by now; the re-derivation must match the original.
        assert_eq!(
            create_discriminant_int(&first_seed, 512).expect("derivation succeeds"),
            direct
        );
    }
}
