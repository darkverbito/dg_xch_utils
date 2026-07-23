use crate::error::{Error, Result};
use num_bigint::{BigInt, BigUint, Sign};
use num_integer::Integer;
use num_traits::{One, Signed, Zero};
use sha2::{Digest, Sha256};

const MAX_DISCRIMINANT_SIZE_BITS: usize = 1024;

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

    let prime = hash_prime(seed, size_bits, &[0, 1, 2, size_bits - 1]);
    Ok(-prime)
}

pub(crate) fn hash_prime(seed: &[u8], size_bits: usize, bitmask: &[usize]) -> BigInt {
    let mut sprout = seed.to_vec();

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

        if is_probable_prime(&candidate) {
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
    if *n < BigUint::from(2u8) {
        return false;
    }

    const SMALL_PRIMES: [u64; 64] = [
        2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47, 53, 59, 61, 67, 71, 73, 79, 83, 89,
        97, 101, 103, 107, 109, 113, 127, 131, 137, 139, 149, 151, 157, 163, 167, 173, 179, 181,
        191, 193, 197, 199, 211, 223, 227, 229, 233, 239, 241, 251, 257, 263, 269, 271, 277, 281,
        283, 293, 307, 311,
    ];

    for prime in SMALL_PRIMES {
        let p = BigUint::from(prime);
        if n == &p {
            return true;
        }
        if n % &p == BigUint::zero() {
            return false;
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
