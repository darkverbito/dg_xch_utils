//! Version-tolerant decode for stored `block_record.record` blobs (campaign issue #155).
//!
//! Until #155, `BlockRecord.challenge_vdf_output` / `infused_challenge_vdf_output` serialized as
//! length-prefixed byte vectors (`VdfOutput { data: UnsizedBytes }` — a u32-BE `0x00000064` prefix
//! ahead of each 100-byte VDF output). chia_rs `BlockRecord` carries bare fixed 100-byte
//! `ClassgroupElement`s, and the struct now matches chia byte-for-byte — but every store leg in the
//! fleet still holds records persisted in the legacy layout, and a forced resync is not acceptable.
//!
//! Decode strategy: try the chia layout first (every new write, and the steady state once a leg's
//! records have churned), requiring exact-fit framing — the parse must consume the blob exactly.
//! On any failure, fall back to a field-by-field walk of the legacy layout (also exact-fit). The
//! two layouts cannot be confused: a legacy blob read as chia misplaces every field after byte 101
//! and must survive ~10 constrained Option/bool tag bytes AND land on the exact blob length; the
//! exact-fit gate alone rejects it in every observed case because the legacy form is 4 (or 8)
//! bytes longer than the chia form of the same record. New writes always use the chia layout, so
//! legacy blobs age out as records are rewritten; reads never require a store migration.

use crate::error::StoreError;
use dg_xch_core::blockchain::block_record::BlockRecord;
use dg_xch_core::blockchain::class_group_element::ClassgroupElement;
use dg_xch_core::blockchain::sized_bytes::Bytes100;
use dg_xch_core::blockchain::unsized_bytes::UnsizedBytes;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use std::io::{Cursor, Error, ErrorKind};

const VERSION: ChiaProtocolVersion = ChiaProtocolVersion::Chia0_0_37;

/// Decode a stored record blob: chia layout first (exact fit), legacy layout as the fallback.
pub(crate) fn decode_record(blob: &[u8]) -> Result<BlockRecord, StoreError> {
    let mut cur = Cursor::new(blob);
    let chia_err = match BlockRecord::from_bytes(&mut cur, VERSION) {
        Ok(rec) if cur.position() == blob.len() as u64 => return Ok(rec),
        Ok(_) => Error::new(
            ErrorKind::InvalidData,
            "trailing bytes after chia-layout block record",
        ),
        Err(e) => e,
    };
    // Not a chia-layout blob — pre-#155 stored form. Surface the chia-layout error if the
    // legacy walk fails too: a blob that parses as neither is corrupt, and the primary
    // (current-layout) diagnosis is the useful one.
    decode_legacy_record(blob).map_err(|_| StoreError::Io(chia_err))
}

/// The pre-#155 layout: identical to chia's except the two VDF outputs are length-prefixed
/// byte vectors instead of bare 100-byte values.
fn decode_legacy_record(blob: &[u8]) -> Result<BlockRecord, Error> {
    fn f<T: ChiaSerialize>(c: &mut Cursor<&[u8]>) -> Result<T, Error> {
        T::from_bytes(c, VERSION)
    }
    fn legacy_vdf(c: &mut Cursor<&[u8]>) -> Result<ClassgroupElement, Error> {
        let data = UnsizedBytes::from_bytes(c, VERSION)?;
        let arr: [u8; 100] = data.as_slice().try_into().map_err(|_| {
            Error::new(ErrorKind::InvalidData, "legacy VDF output is not 100 bytes")
        })?;
        Ok(ClassgroupElement {
            data: Bytes100::from(arr),
        })
    }
    let mut c = Cursor::new(blob);
    let record = BlockRecord {
        header_hash: f(&mut c)?,
        prev_hash: f(&mut c)?,
        height: f(&mut c)?,
        weight: f(&mut c)?,
        total_iters: f(&mut c)?,
        signage_point_index: f(&mut c)?,
        challenge_vdf_output: legacy_vdf(&mut c)?,
        infused_challenge_vdf_output: match u8::from_bytes(&mut c, VERSION)? {
            0 => None,
            1 => Some(legacy_vdf(&mut c)?),
            _ => {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "invalid Option tag in legacy-layout block record",
                ));
            }
        },
        reward_infusion_new_challenge: f(&mut c)?,
        challenge_block_info_hash: f(&mut c)?,
        sub_slot_iters: f(&mut c)?,
        pool_puzzle_hash: f(&mut c)?,
        farmer_puzzle_hash: f(&mut c)?,
        required_iters: f(&mut c)?,
        deficit: f(&mut c)?,
        overflow: f(&mut c)?,
        prev_transaction_block_height: f(&mut c)?,
        timestamp: f(&mut c)?,
        prev_transaction_block_hash: f(&mut c)?,
        fees: f(&mut c)?,
        reward_claims_incorporated: f(&mut c)?,
        finished_challenge_slot_hashes: f(&mut c)?,
        finished_infused_challenge_slot_hashes: f(&mut c)?,
        finished_reward_slot_hashes: f(&mut c)?,
        sub_epoch_summary_included: f(&mut c)?,
    };
    if c.position() != blob.len() as u64 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "trailing bytes after legacy-layout block record",
        ));
    }
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Offset of challenge_vdf_output in both layouts: 32 (header_hash) + 32 (prev_hash) +
    // 4 (height) + 16 (weight) + 16 (total_iters) + 1 (signage_point_index).
    const VDF_OFFSET: usize = 101;

    fn unhex(s: &str) -> Vec<u8> {
        assert!(s.len().is_multiple_of(2), "even-length hex");
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }

    /// Real chia mainnet record blobs (see core/tests/block_record_wire.rs for provenance).
    fn goldens() -> Vec<Vec<u8>> {
        include_str!("../../core/tests/fixtures/block_record_mainnet_3000000.txt")
            .lines()
            .filter_map(|l| l.strip_prefix("RECORD "))
            .map(|h| unhex(h.trim()))
            .collect()
    }

    /// Rebuild the pre-#155 blob for a chia-layout record byte-for-byte: the legacy encoder
    /// wrote a u32-BE 0x64 length prefix ahead of each 100-byte VDF output and was otherwise
    /// identical. Pinned here structurally — independent of the shipped legacy decoder — so
    /// the compat path is proven against the layout itself, not against its own inverse.
    fn legacy_blob_of(chia: &[u8]) -> Vec<u8> {
        const PREFIX: [u8; 4] = 100u32.to_be_bytes();
        let mut out = Vec::with_capacity(chia.len() + 8);
        out.extend_from_slice(&chia[..VDF_OFFSET]);
        out.extend_from_slice(&PREFIX);
        out.extend_from_slice(&chia[VDF_OFFSET..VDF_OFFSET + 100]);
        // Option tag for infused_challenge_vdf_output.
        let tag_at = VDF_OFFSET + 100;
        out.push(chia[tag_at]);
        let rest = if chia[tag_at] == 1 {
            out.extend_from_slice(&PREFIX);
            out.extend_from_slice(&chia[tag_at + 1..tag_at + 101]);
            &chia[tag_at + 101..]
        } else {
            &chia[tag_at + 1..]
        };
        out.extend_from_slice(rest);
        out
    }

    #[test]
    fn chia_layout_blobs_decode_directly() {
        for blob in goldens() {
            let rec = decode_record(&blob).expect("chia-layout blob decodes");
            assert_eq!(rec.to_bytes(VERSION).expect("encode"), blob);
        }
    }

    #[test]
    fn legacy_layout_blobs_decode_via_the_fallback() {
        for blob in goldens() {
            let legacy = legacy_blob_of(&blob);
            assert_ne!(legacy, blob, "legacy layout differs");
            let from_legacy = decode_record(&legacy).expect("legacy blob decodes");
            let from_chia = decode_record(&blob).expect("chia blob decodes");
            assert_eq!(from_legacy, from_chia, "both layouts land the same record");
            // A legacy record re-encodes in the chia layout — legacy blobs age out on rewrite.
            assert_eq!(from_legacy.to_bytes(VERSION).expect("encode"), blob);
        }
    }

    #[test]
    fn garbage_blobs_error_on_both_paths() {
        assert!(decode_record(&[]).is_err());
        assert!(decode_record(&[0u8; 7]).is_err());
        // A truncated golden fails both exact-fit walks.
        let blob = goldens().remove(0);
        assert!(decode_record(&blob[..blob.len() - 1]).is_err());
        // Trailing garbage fails exact-fit on both paths.
        let mut extended = blob;
        extended.push(0);
        assert!(decode_record(&extended).is_err());
    }
}
