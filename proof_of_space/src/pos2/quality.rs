use crate::pos2::chainer::QualityChainLinks;
use crate::pos2::constants::NUM_CHAIN_LINKS;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::traits::SizedBytes;
use dg_xch_core::utils::hash_256;

/// The quality commitment a v2 proof is hashed under: one byte of strength, then each chain link
/// little endian. The full proof is the witness to this commitment, which is why a v2
/// `ProofOfSpace` hashes this rather than its proof bytes.
#[must_use]
pub fn serialize_quality(
    fragments: &QualityChainLinks,
    strength: u8,
) -> [u8; NUM_CHAIN_LINKS * 8 + 1] {
    let mut out = [0u8; NUM_CHAIN_LINKS * 8 + 1];
    out[0] = strength;
    for (i, fragment) in fragments.iter().enumerate() {
        out[1 + i * 8..1 + (i + 1) * 8].copy_from_slice(&fragment.to_le_bytes());
    }
    out
}

/// The consensus quality string: the hash of the serialized quality commitment.
#[must_use]
pub fn quality_hash(fragments: &QualityChainLinks, strength: u8) -> Bytes32 {
    Bytes32::new(hash_256(serialize_quality(fragments, strength)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_commitment_is_strength_then_little_endian_links() {
        let mut fragments = [0u64; NUM_CHAIN_LINKS];
        fragments[0] = 25_078_806_449;
        let out = serialize_quality(&fragments, 2);
        assert_eq!(out.len(), 129);
        assert_eq!(out[0], 2);
        // The fragment, little endian.
        assert_eq!(
            &out[1..9],
            &[0xb1, 0x37, 0xd0, 0xd6, 0x05, 0x00, 0x00, 0x00]
        );
        assert_eq!(&out[9..17], &[0u8; 8]);
    }

    #[test]
    fn the_quality_moves_with_every_link_and_the_strength() {
        let fragments = [7u64; NUM_CHAIN_LINKS];
        let base = quality_hash(&fragments, 2);
        assert_eq!(base, quality_hash(&fragments, 2));
        assert_ne!(base, quality_hash(&fragments, 3));
        let mut changed = fragments;
        changed[NUM_CHAIN_LINKS - 1] += 1;
        assert_ne!(base, quality_hash(&changed, 2));
    }
}
