use crate::error::SimError;
use dg_xch_core::blockchain::class_group_element::ClassgroupElement;
use dg_xch_core::blockchain::sized_bytes::{Bytes32, Bytes100};
use dg_xch_core::blockchain::unsized_bytes::UnsizedBytes;
use dg_xch_core::blockchain::vdf_info::VdfInfo;
use dg_xch_core::blockchain::vdf_proof::VdfProof;
use dg_xch_core::traits::SizedBytes;
use dg_xch_vdf::proof::prove_result;

/// Serialized size of a class group element, and so of each half of a one-wesolowski proof. Fixed
/// by `FORM_SIZE`, independent of the discriminant actually in use.
const FORM_BYTES: usize = 100;

/// Run one VDF and return what a block needs to carry it.
///
/// `discriminant_size_bits` is what makes a simulated chain cheap: the real prover and the real
/// verifier run unchanged, over a group small enough that a proof takes microseconds instead of
/// the seconds a mainnet-sized discriminant costs.
pub fn prove_vdf(
    challenge: Bytes32,
    input: &ClassgroupElement,
    iterations: u64,
    discriminant_size_bits: u64,
) -> Result<(VdfInfo, VdfProof), SimError> {
    let bits = usize::try_from(discriminant_size_bits).map_err(|_| {
        SimError::Invariant(format!("discriminant {discriminant_size_bits} too wide"))
    })?;
    let bytes = prove_result(&challenge.bytes(), &input.data.bytes(), bits, iterations)?;
    if bytes.len() < FORM_BYTES {
        return Err(SimError::Invariant(format!(
            "vdf returned {} bytes, need at least {FORM_BYTES}",
            bytes.len()
        )));
    }
    let (output, witness) = bytes.split_at(FORM_BYTES);
    Ok((
        VdfInfo {
            challenge,
            number_of_iterations: iterations,
            output: ClassgroupElement {
                data: Bytes100::parse(output).map_err(|e| {
                    SimError::Invariant(format!("vdf output is not a class group element: {e:?}"))
                })?,
            },
        },
        VdfProof {
            witness_type: 0,
            witness: UnsizedBytes::new(witness.to_vec()),
            normalized_to_identity: false,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dg_xch_core::consensus::constants::{ConsensusConstants, SIMULATOR};
    use dg_xch_core::consensus::overrides::{ConsensusOverrides, apply_overrides};
    use dg_xch_vdf::validate_vdf_info;
    use num_bigint::BigInt;

    fn constants_at(discriminant_size_bits: i64) -> ConsensusConstants {
        apply_overrides(
            SIMULATOR,
            &ConsensusOverrides {
                discriminant_size_bits: Some(BigInt::from(discriminant_size_bits)),
                ..Default::default()
            },
        )
    }

    #[test]
    fn a_tiny_discriminant_proof_validates_through_the_real_verifier() {
        // The premise the fast tier rests on: shrinking the discriminant does not put the chain on
        // a different code path, it just makes the same prover and verifier cheap.
        for bits in [16i64, 32, 64, 512] {
            let constants = constants_at(bits);
            let challenge = Bytes32::from([7u8; 32]);
            let input = ClassgroupElement::get_default_element();
            let (info, proof) = prove_vdf(challenge, &input, 64, constants.discriminant_size_bits)
                .unwrap_or_else(|e| panic!("prove at {bits} bits: {e}"));
            assert_eq!(info.number_of_iterations, 64);
            assert!(
                validate_vdf_info(&constants, &input, &info, &proof, None),
                "a {bits} bit proof was rejected by the real verifier"
            );
        }
    }

    #[test]
    fn a_proof_is_rejected_under_a_different_iteration_count() {
        let constants = constants_at(16);
        let challenge = Bytes32::from([9u8; 32]);
        let input = ClassgroupElement::get_default_element();
        let (mut info, proof) = prove_vdf(challenge, &input, 32, 16).expect("prove");
        info.number_of_iterations = 33;
        assert!(!validate_vdf_info(&constants, &input, &info, &proof, None));
    }

    #[test]
    fn a_tampered_witness_is_rejected() {
        let constants = constants_at(16);
        let challenge = Bytes32::from([5u8; 32]);
        let input = ClassgroupElement::get_default_element();
        let (info, mut proof) = prove_vdf(challenge, &input, 32, 16).expect("prove");
        let mut witness = proof.witness.as_slice().to_vec();
        witness[0] ^= 0xFF;
        proof.witness = UnsizedBytes::new(witness);
        assert!(!validate_vdf_info(&constants, &input, &info, &proof, None));
    }

    #[test]
    fn proving_is_deterministic() {
        let challenge = Bytes32::from([1u8; 32]);
        let input = ClassgroupElement::get_default_element();
        let a = prove_vdf(challenge, &input, 48, 16).expect("prove");
        let b = prove_vdf(challenge, &input, 48, 16).expect("prove");
        assert_eq!(a.0.output, b.0.output);
        assert_eq!(a.1.witness.as_slice(), b.1.witness.as_slice());
    }

    #[test]
    fn chaining_a_proof_onto_the_previous_output_validates() {
        // How a timelord actually runs: each infusion continues from the last output rather than
        // restarting from the identity element.
        let constants = constants_at(16);
        let challenge = Bytes32::from([3u8; 32]);
        let first_input = ClassgroupElement::get_default_element();
        let (first, first_proof) = prove_vdf(challenge, &first_input, 32, 16).expect("prove");
        assert!(validate_vdf_info(
            &constants,
            &first_input,
            &first,
            &first_proof,
            None
        ));

        let (second, second_proof) = prove_vdf(challenge, &first.output, 32, 16).expect("prove");
        assert!(validate_vdf_info(
            &constants,
            &first.output,
            &second,
            &second_proof,
            None
        ));
        assert_ne!(first.output, second.output);
    }
}
