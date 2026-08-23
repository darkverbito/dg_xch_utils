use crate::blockchain::vdf_info::VdfInfo;
use crate::blockchain::vdf_proof::VdfProof;
use dg_xch_macros::ChiaSerial;
use serde::{Deserialize, Serialize};

/// A signage point, mirroring chia `chia/consensus/signage_point.py::SignagePoint`. All four VDF/proof
/// fields are `Option` because chia represents the sub-slot-start signage point (signage-point index 0)
/// as `SignagePoint(None, None, None, None)` — the point at the very start of a sub slot has no signage
/// VDFs of its own; it is validated against the sub-slot boundary (the cc/rc challenge). Indices 1..64
/// carry the real signage-chain VDFs.
///
/// WIRE NOTE: this is an internal store/RPC type, NOT a standalone network message. The p2p gossip uses
/// `NewSignagePointOrEndOfSubSlot` (indices/hashes) and the pull uses `RespondSignagePoint` (which carries
/// non-optional `VdfInfo`/`VdfProof` directly). Its `ChiaSerialize` encoding (now with a 1-byte presence
/// tag per field, exactly like chia's `parse_optional`) is only exercised inside `SignagePointOrEOS`
/// (the RPC `get_signage_point_or_eos` response, which travels as serde JSON in practice, `null` per
/// absent field). Chia's `SignagePoint` is likewise never sent as a top-level protocol payload.
#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct SignagePoint {
    pub cc_vdf: Option<VdfInfo>,
    pub cc_proof: Option<VdfProof>,
    pub rc_vdf: Option<VdfInfo>,
    pub rc_proof: Option<VdfProof>,
}

impl SignagePoint {
    /// The sub-slot-start signage point (signage-point index 0) — chia `SignagePoint(None, None, None,
    /// None)`. Carries no signage VDFs; a declare/candidate at index 0 is validated/built against the
    /// sub-slot boundary instead.
    #[must_use]
    pub fn sub_slot_start() -> Self {
        Self {
            cc_vdf: None,
            cc_proof: None,
            rc_vdf: None,
            rc_proof: None,
        }
    }

    /// True if this is the sub-slot-start signage point (all VDFs absent).
    #[must_use]
    pub fn is_sub_slot_start(&self) -> bool {
        self.cc_vdf.is_none()
            && self.cc_proof.is_none()
            && self.rc_vdf.is_none()
            && self.rc_proof.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockchain::class_group_element::ClassgroupElement;
    use crate::blockchain::sized_bytes::Bytes32;
    use crate::blockchain::unsized_bytes::UnsizedBytes;
    use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
    use std::io::Cursor;

    fn full_sp() -> SignagePoint {
        let vdf = VdfInfo {
            challenge: Bytes32::from([7; 32]),
            number_of_iterations: 42,
            output: ClassgroupElement::get_default_element(),
        };
        let proof = VdfProof {
            witness_type: 0,
            witness: UnsizedBytes::default(),
            normalized_to_identity: false,
        };
        SignagePoint {
            cc_vdf: Some(vdf),
            cc_proof: Some(proof.clone()),
            rc_vdf: Some(vdf),
            rc_proof: Some(proof),
        }
    }

    fn round_trip(sp: &SignagePoint) -> SignagePoint {
        let v = ChiaProtocolVersion::default();
        let bytes = sp.to_bytes(v).expect("encode");
        SignagePoint::from_bytes(&mut Cursor::new(bytes.as_slice()), v).expect("decode")
    }

    #[test]
    fn full_signage_point_round_trips() {
        let sp = full_sp();
        assert_eq!(round_trip(&sp), sp);
    }

    #[test]
    fn sub_slot_start_signage_point_round_trips() {
        // chia SignagePoint(None, None, None, None) — the index-0 SP. Each absent field is a single 0x00
        // presence tag (chia parse_optional), so the whole SP encodes to exactly four zero bytes.
        let sp = SignagePoint::sub_slot_start();
        assert!(sp.is_sub_slot_start());
        let v = ChiaProtocolVersion::default();
        let bytes = sp.to_bytes(v).expect("encode");
        assert_eq!(
            bytes,
            vec![0u8; 4],
            "all-None SP is four presence-tag zero bytes"
        );
        assert_eq!(round_trip(&sp), sp);
    }

    #[test]
    fn garbage_presence_tag_errs_not_panics() {
        // A non-0/1 presence byte is a malformed optional (chia raises ValueError); the decoder must
        // return Err, never panic.
        let v = ChiaProtocolVersion::default();
        let garbage = [2u8, 0, 0, 0];
        assert!(SignagePoint::from_bytes(&mut Cursor::new(&garbage[..]), v).is_err());
    }

    #[test]
    fn truncated_input_errs_not_panics() {
        // A "Some" cc_vdf tag with no following bytes must error, not panic.
        let v = ChiaProtocolVersion::default();
        let truncated = [1u8];
        assert!(SignagePoint::from_bytes(&mut Cursor::new(&truncated[..]), v).is_err());
    }
}
