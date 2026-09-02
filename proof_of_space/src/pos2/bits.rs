//! The proof wire format: 128 x values of `k` bits each, packed big endian.

/// Pack `k` bit values into bytes, most significant bit first.
#[must_use]
pub fn compact_bits(x_values: &[u32], k: u8) -> Vec<u8> {
    assert!(
        k >= 8,
        "k below 8 would let trailing padding read as a value"
    );
    let mut out = Vec::with_capacity((x_values.len() * usize::from(k)).div_ceil(8));
    let mut byte = 0u8;
    let mut bits_left = 8u8;
    for x in x_values {
        let mut x_bits = k;
        while x_bits > 0 {
            let take = x_bits.min(bits_left);
            let chunk = ((x >> (x_bits - take)) as u8) & ((1u16 << take) - 1) as u8;
            byte |= chunk << (bits_left - take);
            bits_left -= take;
            x_bits -= take;
            if bits_left == 0 {
                out.push(byte);
                byte = 0;
                bits_left = 8;
            }
        }
    }
    if bits_left < 8 {
        out.push(byte);
    }
    out
}

/// Unpack `k` bit values from a proof buffer. `None` when the buffer does not end on a value
/// boundary, which is how a truncated proof is caught.
#[must_use]
pub fn expand_bits(proof: &[u8], k: u8) -> Option<Vec<u32>> {
    if k == 0 || k > 32 {
        return None;
    }
    let mut values = Vec::with_capacity(proof.len() * 8 / usize::from(k));
    let mut value = 0u32;
    let mut bits_left = k;
    for byte in proof {
        let mut byte_bits = 8u8;
        let mut byte_mask = 0xFFu8;
        while byte_bits > 0 {
            if bits_left > byte_bits {
                value |= u32::from(byte & byte_mask) << (bits_left - byte_bits);
            } else {
                value |= u32::from(byte & byte_mask) >> (byte_bits - bits_left);
            }
            let copied = byte_bits.min(bits_left);
            bits_left -= copied;
            byte_bits -= copied;
            if byte_bits > 0 {
                byte_mask >>= copied;
            }
            if bits_left == 0 {
                values.push(value);
                bits_left = k;
                value = 0;
            }
        }
    }
    if value == 0 { Some(values) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packing_round_trips() {
        for k in [8u8, 18, 24, 28, 32] {
            let mask = if k == 32 { u32::MAX } else { (1u32 << k) - 1 };
            let xs: Vec<u32> = (0..128u32)
                .map(|i| i.wrapping_mul(2_654_435_761) & mask)
                .collect();
            let packed = compact_bits(&xs, k);
            assert_eq!(packed.len(), (128 * usize::from(k)).div_ceil(8), "k{k}");
            assert_eq!(expand_bits(&packed, k).expect("expands"), xs, "k{k}");
        }
    }

    #[test]
    fn a_k18_proof_is_288_bytes() {
        assert_eq!(compact_bits(&[0u32; 128], 18).len(), 288);
    }
}
