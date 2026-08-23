//! chia's BIP158 block transaction filter — the `PyBIP158(byte_array_tx).GetEncoded()` a producer feeds
//! into `FoliageTransactionBlock.filter_hash = std_hash(encoded)`
//! (`chia/consensus/block_creation.py:205-206`).
//!
//! chia does NOT use standard BIP158 parameters. `chiabip158`'s `PyBIP158` constructor builds the
//! Golomb-coded set with a fixed, chia-specific `GCSFilter::Params`:
//!
//! ```cpp
//! // chiabip158/python-bindings/PyBIP158.cpp
//! filter = new GCSFilter({0, 0, 20, 1 << 20}, elements);
//! //                       k0 k1  P   M
//! ```
//!
//! i.e. `siphash_k0 = 0`, `siphash_k1 = 0`, `P = 20`, `M = 1 << 20 = 1_048_576`. This differs from
//! Bitcoin Core's `BASIC_FILTER_P = 19` / `BASIC_FILTER_M = 784931` (`chiabip158/src/blockfilter.h`), and
//! from `rust-bitcoin`'s `bip158`, which keys siphash off the block hash. chia keys siphash with a fixed
//! all-zero key and passes NO block hash — so the encoding depends only on the element set.
//!
//! The GCS encode/hash algorithm itself is the unmodified Bitcoin Core implementation
//! (`chiabip158/src/blockfilter.cpp`): dedup the raw elements, siphash-2-4 each into `[0, N*M)` via
//! Lemire's multiply-shift `MapIntoRange`, sort, then Golomb-Rice encode the deltas behind a
//! `WriteCompactSize(N)` prefix, MSB-first (`BitStreamWriter`, `chiabip158/src/streams.h`).
//!
//! Ported (not FFI-bound): `chiabip158` ships only as a compiled C++ extension with a bindgen Rust
//! shim (`chiabip158/rust-bindings`), which would drag a C++ toolchain into the dg_xch build. The GCS
//! core is ~100 lines of fixed consensus math, so we port it faithfully and pin it byte-for-byte against
//! `chiabip158`'s own harvested test vector (see `chia_block_filter_matches_chiabip158_vector`).

/// chia's Golomb-Rice parameter `P` (`chiabip158/python-bindings/PyBIP158.cpp`: `{0, 0, 20, 1 << 20}`).
const GCS_P: u32 = 20;

/// chia's inverse false-positive rate `M` (`chiabip158/python-bindings/PyBIP158.cpp`: `1 << 20`).
const GCS_M: u64 = 1 << 20;

/// SipHash-2-4 key half `k0` for chia's filter — fixed `0` (`PyBIP158.cpp` params).
const SIPHASH_K0: u64 = 0;

/// SipHash-2-4 key half `k1` for chia's filter — fixed `0` (`PyBIP158.cpp` params).
const SIPHASH_K1: u64 = 0;

/// Encode a chia BIP158 block transaction filter over `items` (the `byte_array_tx` list: each addition's
/// puzzle hash, then each removal's coin name), byte-identical to
/// `chiabip158`'s `PyBIP158(items).GetEncoded()`.
///
/// The empty-item case returns the single byte `[0]` (the `WriteCompactSize(0)` element count with no GCS
/// body) — the same constant genesis / no-tx-content blocks carry, so
/// `filter_hash == std_hash([0]) == sha256([0])`.
///
/// Elements are deduplicated by raw bytes first: `chiabip158` inserts into a
/// `std::unordered_set<Element>` (`GCSFilter(params, elements)` in `src/blockfilter.cpp`), so repeated
/// puzzle hashes / coin names collapse to a single set member and `N` counts distinct elements only.
#[must_use]
pub fn chia_block_filter(items: &[Vec<u8>]) -> Vec<u8> {
    // chia: GCSFilter::ElementSet is std::unordered_set<Element> — dedup the raw element bytes.
    // Sort+dedup on the raw bytes reproduces the set membership (encode order is set by the sorted
    // hashes below, not by element order, so any stable dedup is equivalent).
    let mut distinct: Vec<&Vec<u8>> = items.iter().collect();
    distinct.sort_unstable();
    distinct.dedup();

    let n = distinct.len() as u64;
    let mut out = Vec::new();
    // chia: WriteCompactSize(m_N) prefixes the encoded filter (src/blockfilter.cpp).
    write_compact_size(&mut out, n);
    if n == 0 {
        // chia's empty filter: just the N=0 CompactSize byte, no GCS body. Yields [0].
        return out;
    }

    // chia: m_F = N * M. HashToRange maps each element uniformly into [0, F).
    let f = n * GCS_M;
    let mut hashed: Vec<u64> = distinct
        .iter()
        .map(|e| map_into_range(siphash24(SIPHASH_K0, SIPHASH_K1, e.as_slice()), f))
        .collect();
    // chia: BuildHashedSet sorts the hashed values ascending (duplicate hashes are kept => delta 0).
    hashed.sort_unstable();

    // chia: for each sorted value, GolombRiceEncode(P, value - last_value), MSB-first, then Flush().
    let mut writer = BitStreamWriter::new(out);
    let mut last_value = 0u64;
    for value in hashed {
        let delta = value - last_value;
        golomb_rice_encode(&mut writer, GCS_P, delta);
        last_value = value;
    }
    writer.finish()
}

/// Decode a chia BIP158 filter back into its sorted hashed-value set — the inverse of
/// [`chia_block_filter`], mirroring `chiabip158`'s `GCSFilter(params, encoded_filter)` decode
/// constructor (`src/blockfilter.cpp`): read the `CompactSize` element count, then N
/// Golomb-Rice-coded deltas (P = 20), reconstructing the ascending hashed values. Returns `None`
/// on malformed input (truncated bit stream, bad CompactSize) — never panics.
///
/// The decoded vec's LENGTH is the filter's `N`, which [`chia_block_filter_match`] needs to
/// reproduce the `HashToRange` domain `[0, N*M)`.
#[must_use]
pub fn decode_chia_block_filter(filter: &[u8]) -> Option<Vec<u64>> {
    let (n, body_start) = read_compact_size(filter)?;
    if n == 0 {
        return Some(Vec::new());
    }
    // Defensive size bound: each element takes at least 1 bit of quotient terminator + P bits of
    // remainder, so a valid filter carries at least n*(P+1)/8 body bytes.
    let body = filter.get(body_start..)?;
    if (u128::from(n) * u128::from(GCS_P + 1)) > (body.len() as u128) * 8 {
        return None;
    }
    let mut reader = BitStreamReader::new(body);
    let mut values = Vec::with_capacity(usize::try_from(n).ok()?);
    let mut last_value: u64 = 0;
    for _ in 0..n {
        // GolombRiceDecode: unary quotient (1-bits until a 0), then P remainder bits.
        let mut quotient: u64 = 0;
        while reader.read(1)? == 1 {
            quotient = quotient.checked_add(1)?;
        }
        let remainder = reader.read(GCS_P)?;
        let delta = quotient
            .checked_mul(1u64 << GCS_P)?
            .checked_add(remainder)?;
        last_value = last_value.checked_add(delta)?;
        values.push(last_value);
    }
    Some(values)
}

/// Whether `item` is (probabilistically) a member of a filter decoded by
/// [`decode_chia_block_filter`] — `chiabip158` `GCSFilter::Match` semantics: hash the element
/// into `[0, N*M)` with the same fixed-key siphash and binary-search the sorted value set.
#[must_use]
pub fn chia_block_filter_match(decoded: &[u64], item: &[u8]) -> bool {
    if decoded.is_empty() {
        return false;
    }
    let f = decoded.len() as u64 * GCS_M;
    let hashed = map_into_range(siphash24(SIPHASH_K0, SIPHASH_K1, item), f);
    decoded.binary_search(&hashed).is_ok()
}

/// Bitcoin Core `ReadCompactSize`: returns `(value, bytes consumed)`; `None` on truncation.
fn read_compact_size(bytes: &[u8]) -> Option<(u64, usize)> {
    match *bytes.first()? {
        n if n < 253 => Some((u64::from(n), 1)),
        0xfd => Some((
            u64::from(u16::from_le_bytes(bytes.get(1..3)?.try_into().ok()?)),
            3,
        )),
        0xfe => Some((
            u64::from(u32::from_le_bytes(bytes.get(1..5)?.try_into().ok()?)),
            5,
        )),
        _ => Some((u64::from_le_bytes(bytes.get(1..9)?.try_into().ok()?), 9)),
    }
}

// Bitcoin Core `BitStreamReader` (`chiabip158/src/streams.h`): consume bits MSB-first.
struct BitStreamReader<'a> {
    bytes: &'a [u8],
    // absolute bit cursor
    position: usize,
}

impl<'a> BitStreamReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    // Read `nbits` (<= 64) MSB-first; None past the end.
    fn read(&mut self, nbits: u32) -> Option<u64> {
        let mut out: u64 = 0;
        for _ in 0..nbits {
            let byte = self.bytes.get(self.position / 8)?;
            let bit = (byte >> (7 - (self.position % 8))) & 1;
            out = (out << 1) | u64::from(bit);
            self.position += 1;
        }
        Some(out)
    }
}

/// Bitcoin Core `WriteCompactSize` (`chiabip158/src/serialize.h`): 1 byte `< 253`, else `0xfd`+u16 LE,
/// `0xfe`+u32 LE, or `0xff`+u64 LE.
fn write_compact_size(out: &mut Vec<u8>, n: u64) {
    if n < 253 {
        out.push(n as u8);
    } else if n <= u64::from(u16::MAX) {
        out.push(0xfd);
        out.extend_from_slice(&(n as u16).to_le_bytes());
    } else if n <= u64::from(u32::MAX) {
        out.push(0xfe);
        out.extend_from_slice(&(n as u32).to_le_bytes());
    } else {
        out.push(0xff);
        out.extend_from_slice(&n.to_le_bytes());
    }
}

/// chia `MapIntoRange` (`chiabip158/src/blockfilter.cpp`): map `x` uniformly in `[0, 2^64)` to `[0, n)`
/// via the upper 64 bits of the 128-bit product `x * n` (Lemire's multiply-shift).
#[inline]
fn map_into_range(x: u64, n: u64) -> u64 {
    ((u128::from(x) * u128::from(n)) >> 64) as u64
}

/// chia `GolombRiceEncode` (`chiabip158/src/blockfilter.cpp`): write the quotient `x >> P` as unary
/// (that many `1` bits, then a `0`), then the low `P` bits of `x` as the remainder.
fn golomb_rice_encode(writer: &mut BitStreamWriter, p: u32, x: u64) {
    let mut q = x >> p;
    while q > 0 {
        // chia writes at most 64 unary bits per Write() call.
        let nbits = if q <= 64 { q as u32 } else { 64 };
        writer.write(u64::MAX, nbits);
        q -= u64::from(nbits);
    }
    writer.write(0, 1);
    // The remainder is the bottom P bits of x; Write() drops the high bits, so no masking is needed.
    writer.write(x, p);
}

/// Bitcoin Core `BitStreamWriter` (`chiabip158/src/streams.h`): buffers bits MSB-first into whole octets,
/// `Flush()` zero-pads the final partial byte.
struct BitStreamWriter {
    out: Vec<u8>,
    buffer: u8,
    offset: u32,
}

impl BitStreamWriter {
    fn new(out: Vec<u8>) -> Self {
        Self {
            out,
            buffer: 0,
            offset: 0,
        }
    }

    /// Write the `nbits` least-significant bits of `data` (`nbits <= 64`), MSB-first. Faithful port of
    /// `BitStreamWriter::Write`: `m_buffer |= (data << (64 - nbits)) >> (64 - 8 + m_offset)`.
    fn write(&mut self, data: u64, mut nbits: u32) {
        while nbits > 0 {
            let bits = (8 - self.offset).min(nbits);
            // (data << (64 - nbits)) aligns the nbits payload to the MSB, then >> (56 + offset) drops it
            // into the current byte's next free bit. The u8 truncation mirrors C++'s uint8_t m_buffer.
            let shifted = (data << (64 - nbits)) >> (56 + self.offset);
            self.buffer |= shifted as u8;
            self.offset += bits;
            nbits -= bits;
            if self.offset == 8 {
                self.out.push(self.buffer);
                self.buffer = 0;
                self.offset = 0;
            }
        }
    }

    /// chia `BitStreamWriter::Flush()`: emit the final partial byte (zero-padded) if any bits are pending.
    fn finish(mut self) -> Vec<u8> {
        if self.offset != 0 {
            self.out.push(self.buffer);
        }
        self.out
    }
}

/// SipHash-2-4 (Bitcoin Core `CSipHasher` / `crypto/siphash.cpp`) over `data` with 64-bit key halves
/// `k0`/`k1`. chia's filter uses `k0 = k1 = 0`. Matches the reference: 2 compression + 4 finalization
/// rounds, final block high byte carries `len & 0xff`.
fn siphash24(k0: u64, k1: u64, data: &[u8]) -> u64 {
    let mut v0 = k0 ^ 0x736f_6d65_7073_6575;
    let mut v1 = k1 ^ 0x646f_7261_6e64_6f6d;
    let mut v2 = k0 ^ 0x6c79_6765_6e65_7261;
    let mut v3 = k1 ^ 0x7465_6462_7974_6573;

    macro_rules! sipround {
        () => {
            v0 = v0.wrapping_add(v1);
            v1 = v1.rotate_left(13);
            v1 ^= v0;
            v0 = v0.rotate_left(32);
            v2 = v2.wrapping_add(v3);
            v3 = v3.rotate_left(16);
            v3 ^= v2;
            v0 = v0.wrapping_add(v3);
            v3 = v3.rotate_left(21);
            v3 ^= v0;
            v2 = v2.wrapping_add(v1);
            v1 = v1.rotate_left(17);
            v1 ^= v2;
            v2 = v2.rotate_left(32);
        };
    }

    let mut chunks = data.chunks_exact(8);
    for chunk in &mut chunks {
        let m = u64::from_le_bytes(chunk.try_into().expect("chunks_exact(8) yields 8 bytes"));
        v3 ^= m;
        sipround!();
        sipround!();
        v0 ^= m;
    }

    // Final block: remaining < 8 bytes, little-endian, with (len & 0xff) in the top byte.
    let rem = chunks.remainder();
    let mut b: u64 = (data.len() as u64 & 0xff) << 56;
    for (i, &byte) in rem.iter().enumerate() {
        b |= u64::from(byte) << (8 * i);
    }
    v3 ^= b;
    sipround!();
    sipround!();
    v0 ^= b;

    v2 ^= 0xff;
    sipround!();
    sipround!();
    sipround!();
    sipround!();

    v0 ^ v1 ^ v2 ^ v3
}

#[cfg(test)]
mod tests {
    use super::chia_block_filter;
    use crate::utils::hash_256;

    fn sha256(bytes: &[u8]) -> Vec<u8> {
        hash_256(bytes).to_vec()
    }

    // HARVESTED chiabip158 vector: chiabip158/rust-bindings/src/lib.rs `test_filter` asserts
    //   Bip158Filter::new(&[sha256("abc"), sha256("xyz"), sha256("123")]).encode()
    //     == [3, 174, 90, 204, 224, 219, 7, 253, 91]
    // The leading 3 is WriteCompactSize(N=3); the remaining 8 bytes are the Golomb-Rice body. This is the
    // #1 byte-parity gate: it pins P=20, M=1<<20, siphash k0=k1=0, MapIntoRange, and the MSB-first
    // BitStreamWriter all at once against chia's own reference output.
    #[test]
    fn chia_block_filter_matches_chiabip158_vector() {
        let items = vec![sha256(b"abc"), sha256(b"xyz"), sha256(b"123")];
        assert_eq!(
            chia_block_filter(&items),
            vec![3u8, 174, 90, 204, 224, 219, 7, 253, 91],
            "must match chiabip158 rust-bindings test_filter vector byte-for-byte"
        );
    }

    // Genesis / no-tx-content: the empty element set encodes to [0] (WriteCompactSize(0), no body), so
    // filter_hash == sha256([0]). This is the constant chia's fast path hardcodes
    // (chia/full_node/full_block_utils.py:311) and the value producer.rs's genesis test asserts.
    #[test]
    fn empty_filter_is_single_zero_byte() {
        assert_eq!(chia_block_filter(&[]), vec![0u8]);
        assert_eq!(hash_256(chia_block_filter(&[])), hash_256(vec![0u8]));
    }

    // Duplicate raw elements collapse (std::unordered_set semantics): the N prefix counts distinct
    // elements, so a filter over [x, x] equals the filter over [x].
    #[test]
    fn duplicate_elements_are_deduplicated() {
        let x = sha256(b"dup");
        let once = chia_block_filter(std::slice::from_ref(&x));
        let twice = chia_block_filter(&[x.clone(), x]);
        assert_eq!(
            once, twice,
            "duplicate elements must collapse to one (N distinct)"
        );
        assert_eq!(once[0], 1, "N == 1 distinct element");
    }

    // Element order does not change the encoding: chia sorts the hashed values before Golomb-Rice
    // encoding, so a permutation of the same set yields identical bytes.
    #[test]
    fn element_order_does_not_matter() {
        let a = sha256(b"abc");
        let b = sha256(b"xyz");
        let c = sha256(b"123");
        let forward = chia_block_filter(&[a.clone(), b.clone(), c.clone()]);
        let shuffled = chia_block_filter(&[c, a, b]);
        assert_eq!(forward, shuffled);
    }
}

#[cfg(test)]
mod decode_tests {
    use super::*;

    #[test]
    fn decode_round_trips_and_matches() {
        let items: Vec<Vec<u8>> = (0u8..100).map(|i| vec![i; 32]).collect();
        let filter = chia_block_filter(&items);
        let decoded = decode_chia_block_filter(&filter).expect("well-formed filter decodes");
        assert_eq!(decoded.len(), 100, "N survives the round trip");
        for item in &items {
            assert!(
                chia_block_filter_match(&decoded, item),
                "every encoded member matches"
            );
        }
        // A non-member misses (false-positive odds 1 in M = 2^20 per probe).
        assert!(!chia_block_filter_match(&decoded, &[0xAB; 33]));
    }

    #[test]
    fn decode_is_defensive_on_garbage() {
        // truncated CompactSize
        assert!(decode_chia_block_filter(&[0xfd]).is_none());
        // empty filter: the single zero byte
        assert_eq!(decode_chia_block_filter(&[0]), Some(Vec::new()));
        assert!(decode_chia_block_filter(&[]).is_none());
        // element count with a body too short to carry it
        assert!(decode_chia_block_filter(&[5, 0x01]).is_none());
    }
}
