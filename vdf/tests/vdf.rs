use dg_xch_core::blockchain::class_group_element::ClassgroupElement;
use dg_xch_core::blockchain::sized_bytes::{Bytes32, Bytes100};
use dg_xch_core::blockchain::unsized_bytes::UnsizedBytes;
use dg_xch_core::blockchain::vdf_info::VdfInfo;
use dg_xch_core::blockchain::vdf_proof::VdfProof;
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_vdf::form::Form;
use dg_xch_vdf::{
    Error, create_discriminant, create_discriminant_bytes, default_classgroup_element, prove,
    validate_vdf_info, validate_vdf_info_result, validate_vdf_with_normalization,
    verify_n_wesolowski, verify_vdf,
};
use num_bigint::{BigInt, Sign};
use num_traits::Num;

fn hex_to_bytes(hex: &str) -> Vec<u8> {
    hex::decode(hex).expect("test vector hex is valid")
}

#[test]
fn create_discriminant_matches_chiavdf_vectors() {
    let vectors = [
        (
            "6c3b9aa767f785b537c0",
            "9a8eaf9c52d9a5f1db648cdf7bcd04b35cb1ac4f421c978fa61fe1344b97d4199dbff700d24e7cfc0b785e4b8b8023dc49f0e90227f74f54234032ac3381879f",
        ),
        (
            "b10da48cea4c09676b8e",
            "b193cdb02f1c2615a257b98933ee0d24157ac5f8c46774d5d635022e6e6bd3f7372898066c2a40fa211d1df8c45cb95c02e36ef878bc67325473d9c0bb34b047",
        ),
        (
            "c51b8a31c98b9fe13065",
            "bb5bd19ae50efe98b5ac56c69453a95e92dc16bb4b2824e73b39b9db0a077fa33fc2e775958af14f675a071bf53f1c22f90ccbd456e2291276951830dba9dcaf",
        ),
        (
            "5de9bc1bb4cb7a9f9cf9",
            "a1e93b8f2e9b0fd3b1325fbe40601f55e2afbdc6161409c0aff8737b7213d7d71cab21ffc83a0b6d5bdeee2fdcbbb34fbc8fc0b439915075afa9ffac8bb1b337",
        ),
        (
            "22cfaefc92e4edb9b0ae",
            "f2a10f70148fb30e4a16c4eda44cc0f9917cb9c2d460926d59a408318472e2cfd597193aa58e1fdccc6ae6a4d85bc9b27f77567ebe94fcedbf530a60ff709fd7",
        ),
    ];

    for (seed_hex, expected_hex) in vectors {
        let seed = hex_to_bytes(seed_hex);
        let expected = hex_to_bytes(expected_hex);
        let mut result = [0u8; 64];

        assert!(create_discriminant(&seed, &mut result));
        assert_eq!(
            result.as_slice(),
            expected.as_slice(),
            "seed {seed_hex} should produce the same discriminant as chiavdf"
        );
        assert_eq!(
            create_discriminant_bytes(&seed, 512).unwrap(),
            expected,
            "owned Vec API should return the same value"
        );
    }
}

#[test]
fn create_discriminant_rejects_invalid_sizes_and_empty_seed() {
    let seed = hex_to_bytes("6c3b9aa767f785b537c0");
    let mut empty_result = [];
    let mut oversized_result = [0u8; 129];

    assert!(!create_discriminant(&seed, &mut empty_result));
    assert!(!create_discriminant(&[], &mut [0u8; 64]));
    assert!(!create_discriminant(&seed, &mut oversized_result));
    assert_eq!(
        create_discriminant_bytes(&seed, 0),
        Err(Error::InvalidDiscriminantSize)
    );
    assert_eq!(
        create_discriminant_bytes(&seed, 1020),
        Err(Error::InvalidDiscriminantSize)
    );
    assert_eq!(
        create_discriminant_bytes(&seed, 1032),
        Err(Error::InvalidDiscriminantSize)
    );
    assert_eq!(create_discriminant_bytes(&[], 512), Err(Error::EmptySeed));
}

#[test]
fn create_discriminant_sets_consensus_bits() {
    let seeds = [
        "a4bb1461ade74ac602e9ae511af68bb254dfe65d61b7faf9fab82d0b4364a30b",
        "1633f29c0ca0597258507bc7d323a8bd485d5f059da56340a2c616081fb05b7f",
        "6aa2451d1469e1213e50f114a49744f96073fedbe53921c8294a303779baa32d",
    ];

    for seed in seeds {
        let discriminant = create_discriminant_bytes(&hex_to_bytes(seed), 1024).unwrap();

        assert_eq!(discriminant.len(), 128);
        assert_ne!(discriminant[0] & 0x80, 0, "top bit must be set");
        assert_eq!(discriminant[127] & 0x07, 0x07, "prime must be 7 mod 8");
    }
}

#[test]
fn prove_and_verify_generator_proof_for_genesis_challenge() {
    let genesis_challenge =
        hex_to_bytes("ccd5bb71183532bff220ba46c268991a3ff07eb358e8255a65c30a2dce0e5fbb");
    let mut default_element = [0u8; 100];
    default_element[0] = 0x08;

    let mut discriminant = [0u8; 128];
    assert!(create_discriminant(&genesis_challenge, &mut discriminant));

    let proof = prove(&genesis_challenge, &default_element, 1024, 231)
        .expect("the default generator proof should be created");

    assert_eq!(proof.len(), 200, "recursion depth 0 proof has two forms");
    assert!(verify_n_wesolowski(
        &discriminant,
        &default_element,
        &proof,
        231,
        0
    ));
    assert!(verify_vdf(
        &genesis_challenge,
        &default_element,
        &proof,
        1024,
        231,
        0
    ));
}

#[test]
fn verifies_chia_known_proof_fixture_from_vdf_txt() {
    let challenge =
        hex_to_bytes("9104c5b5e45d48f374efa0488fe6a617790e9aecb3c9cddec06809b09f45ce9b");
    let mut default_element = [0u8; 100];
    default_element[0] = 0x08;
    let proof = hex_to_bytes(
        "0200553bf0f382fc65a94f20afad5dbce2c1ee8ba3bf93053559ac9960c8fd80ac2222e9b649701a4141a4d8999f0dbfe0c39ea744096598a7528328e5199f0aa30aec8aae8ab5018bf1245329a8272ddff1afbd87ad2eaba1b7fd57bd25edc62e0b010000003f0ffcd0dc307a2aa4678bafba661c77d176ef23afc86e7ea9f4f9eac52b8e1850748019245ecc96547da9b731dc72cded5582a9b0c63e13fd42446c7b28b41d3ded1d0b666d5ddb5b29719e4ebe70969e67e42ddd8591eae60d83dbe619f1250400",
    );

    assert!(verify_vdf(
        &challenge,
        &default_element,
        &proof,
        1024,
        129_499_136,
        0
    ));
    assert!(!verify_vdf(
        &challenge,
        &default_element,
        &proof,
        1024,
        129_499_137,
        0
    ));
}

#[test]
fn prove_can_start_from_previous_output() {
    let challenge = hex_to_bytes("a6c42558174fb1eedc64");
    let mut default_element = [0u8; 100];
    default_element[0] = 0x08;
    let mut discriminant = [0u8; 64];
    assert!(create_discriminant(&challenge, &mut discriminant));

    let first = prove(&challenge, &default_element, 512, 96).unwrap();
    assert!(verify_n_wesolowski(
        &discriminant,
        &default_element,
        &first,
        96,
        0
    ));

    let second = prove(&challenge, &first[..100], 512, 41).unwrap();
    assert!(verify_n_wesolowski(
        &discriminant,
        &first[..100],
        &second,
        41,
        0
    ));
}

#[test]
fn prove_handles_single_and_tiny_iteration_counts() {
    let challenge = [0, 0, 1, 2, 3, 3, 4, 4];
    let mut default_element = [0u8; 100];
    default_element[0] = 0x08;
    let mut discriminant = [0u8; 128];
    assert!(create_discriminant(&challenge, &mut discriminant));

    for iterations in [1, 2] {
        let proof = prove(&challenge, &default_element, 1024, iterations).unwrap();

        assert_eq!(proof.len(), 200);
        assert!(verify_n_wesolowski(
            &discriminant,
            &default_element,
            &proof,
            iterations,
            0
        ));
    }
}

#[test]
fn verifier_rejects_tampered_proof_bytes() {
    let challenge =
        hex_to_bytes("ccd5bb71183532bff220ba46c268991a3ff07eb358e8255a65c30a2dce0e5fbb");
    let mut default_element = [0u8; 100];
    default_element[0] = 0x08;
    let mut discriminant = [0u8; 128];
    assert!(create_discriminant(&challenge, &mut discriminant));

    let mut proof = prove(&challenge, &default_element, 1024, 40).unwrap();
    assert!(verify_n_wesolowski(
        &discriminant,
        &default_element,
        &proof,
        40,
        0
    ));

    proof[2] ^= 0x01;

    assert!(
        !verify_n_wesolowski(&discriminant, &default_element, &proof, 40, 0),
        "changing the serialized output form must invalidate the proof"
    );
}

#[test]
fn verifier_rejects_mismatched_depth_and_proof_length() {
    let challenge =
        hex_to_bytes("ccd5bb71183532bff220ba46c268991a3ff07eb358e8255a65c30a2dce0e5fbb");
    let mut default_element = [0u8; 100];
    default_element[0] = 0x08;
    let mut discriminant = [0u8; 128];
    assert!(create_discriminant(&challenge, &mut discriminant));

    let proof = prove(&challenge, &default_element, 1024, 20).unwrap();
    assert!(!verify_n_wesolowski(
        &discriminant,
        &default_element,
        &proof,
        20,
        1
    ));
    assert!(!verify_n_wesolowski(
        &discriminant,
        &default_element,
        &proof[..199],
        20,
        0
    ));
}

#[test]
fn verifier_rejects_wrong_iteration_count() {
    let challenge =
        hex_to_bytes("ccd5bb71183532bff220ba46c268991a3ff07eb358e8255a65c30a2dce0e5fbb");
    let mut default_element = [0u8; 100];
    default_element[0] = 0x08;
    let mut discriminant = [0u8; 128];
    assert!(create_discriminant(&challenge, &mut discriminant));

    let proof = prove(&challenge, &default_element, 1024, 50).unwrap();

    assert!(!verify_n_wesolowski(
        &discriminant,
        &default_element,
        &proof,
        49,
        0
    ));
}

#[test]
fn form_deserialization_rejects_malformed_and_noncanonical_bytes() {
    let challenge =
        hex_to_bytes("9104c5b5e45d48f374efa0488fe6a617790e9aecb3c9cddec06809b09f45ce9b");
    let discriminant = -BigInt::from_bytes_be(
        Sign::Plus,
        &create_discriminant_bytes(&challenge, 1024).unwrap(),
    );
    let proof = hex_to_bytes(
        "0200553bf0f382fc65a94f20afad5dbce2c1ee8ba3bf93053559ac9960c8fd80ac2222e9b649701a4141a4d8999f0dbfe0c39ea744096598a7528328e5199f0aa30aec8aae8ab5018bf1245329a8272ddff1afbd87ad2eaba1b7fd57bd25edc62e0b010000003f0ffcd0dc307a2aa4678bafba661c77d176ef23afc86e7ea9f4f9eac52b8e1850748019245ecc96547da9b731dc72cded5582a9b0c63e13fd42446c7b28b41d3ded1d0b666d5ddb5b29719e4ebe70969e67e42ddd8591eae60d83dbe619f1250400",
    );
    let canonical = &proof[..100];

    assert_eq!(
        Form::deserialize(&discriminant, &canonical[..99]).unwrap_err(),
        Error::InvalidFormSize
    );

    let mut malformed = [0u8; 100];
    malformed[1] = 0xff;
    assert_eq!(
        Form::deserialize(&discriminant, &malformed).unwrap_err(),
        Error::InvalidCompressedForm
    );

    let mut noncanonical = canonical.to_vec();
    noncanonical[99] ^= 0x01;
    assert!(Form::deserialize(&discriminant, &noncanonical).is_err());
}

#[test]
fn form_deserialization_rejects_inflated_b0_malleability() {
    let discriminant = BigInt::from_str_radix(
        "-146212091130374364448271598629912687111631974722846603227183769906935970876483871782840562162445571052154480975719448767769767557905129461524079902394315542354994269060181795718055043487735056120915916768273200138311940357886024014124174476991145983171370265799623472241486347111977874193600694306566545523111",
        10,
    )
    .unwrap();
    let canonical = hex_to_bytes(
        "0300d8262c430e78e7c06cf60c9b2049968f604f3b506a85bfe4fff319f8176760e06cab8ab45524458bf558101f9b4ce8c23cc1e053263272b808b76c6f26493a113b62ded5707b28d9eedc0503ac2efcd32be670726725be0fa7ea01f0ef3f60250201",
    );

    Form::deserialize(&discriminant, &canonical).unwrap();

    let mut inflated = canonical.clone();
    inflated[99] ^= 0x04;
    assert!(Form::deserialize(&discriminant, &inflated).is_err());

    let mut inflated = canonical;
    inflated[99] ^= 0x08;
    assert!(Form::deserialize(&discriminant, &inflated).is_err());
}

#[test]
fn validates_core_vdf_info_and_proof_types() {
    let challenge =
        hex_to_bytes("ccd5bb71183532bff220ba46c268991a3ff07eb358e8255a65c30a2dce0e5fbb");
    let input = default_classgroup_element();
    let proof_blob = prove(&challenge, input.data.as_ref(), 1024, 36).unwrap();
    let info = VdfInfo {
        challenge: bytes32(&challenge),
        number_of_iterations: 36,
        output: ClassgroupElement {
            data: bytes100(&proof_blob[..100]),
        },
    };
    let proof = VdfProof {
        witness_type: 0,
        witness: UnsizedBytes::new(proof_blob[100..].to_vec()),
        normalized_to_identity: false,
    };

    assert!(validate_vdf_info(&MAINNET, &input, &info, &proof, None));
    assert!(validate_vdf_with_normalization(
        &MAINNET,
        &input,
        &info,
        &proof,
        Some(&info)
    ));
}

#[test]
fn core_adapter_enforces_target_info_and_witness_limit() {
    let challenge =
        hex_to_bytes("ccd5bb71183532bff220ba46c268991a3ff07eb358e8255a65c30a2dce0e5fbb");
    let input = default_classgroup_element();
    let proof_blob = prove(&challenge, input.data.as_ref(), 1024, 20).unwrap();
    let info = VdfInfo {
        challenge: bytes32(&challenge),
        number_of_iterations: 20,
        output: ClassgroupElement {
            data: bytes100(&proof_blob[..100]),
        },
    };
    let proof = VdfProof {
        witness_type: 0,
        witness: UnsizedBytes::new(proof_blob[100..].to_vec()),
        normalized_to_identity: false,
    };
    let wrong_target = VdfInfo {
        number_of_iterations: 21,
        ..info
    };
    let strict_constants = dg_xch_core::consensus::constants::ConsensusConstants {
        max_vdf_witness_size: 0,
        ..MAINNET
    };

    assert_eq!(
        validate_vdf_info_result(&MAINNET, &input, &info, &proof, Some(&wrong_target)),
        Err(Error::TargetVdfMismatch)
    );
    assert_eq!(
        validate_vdf_info_result(&strict_constants, &input, &info, &proof, None),
        Err(Error::WitnessTooLarge {
            witness_type: 0,
            max_vdf_witness_size: 0,
        })
    );
}

#[test]
fn validates_reward_chain_signage_point_from_fullnode_fixture() {
    let info = VdfInfo {
        challenge: bytes32(&hex_to_bytes(
            "e13f6c622a7bad66f67da63c14105977768d72586b5835321508be21de7cb465",
        )),
        number_of_iterations: 885_239,
        output: ClassgroupElement {
            data: bytes100(&hex_to_bytes(
                "030017c1809b7488677f9373ae97fd60b19dd205ed9326f02e8bdb9dbd33ec3cf8ca22b0176c999741e887f4b9b3aabfd272ea9c10fd90cdcc0934cdb6f79ee12007a22eaac7188200e4417bd5c52e24074deeb749cbc86ea514e86af3c1702d15110200",
            )),
        },
    };
    let proof = VdfProof {
        witness_type: 0,
        witness: UnsizedBytes::new(hex_to_bytes(
            "01006ba9baf08084cbf6bb4d7195d96ae968b6ae38e703313919e46a923cc06a95a9abca65ef74aa6419fd76f2d7cd589e0840816399dc2608a94fea48c6bae51100834bf08fc8bccb9dc83af1156dd7be785ac22c0790923f83513f19dd0474f8010302",
        )),
        normalized_to_identity: false,
    };

    assert!(validate_vdf_info(
        &MAINNET,
        &default_classgroup_element(),
        &info,
        &proof,
        None
    ));
}

fn bytes32(bytes: &[u8]) -> Bytes32 {
    Bytes32::from(<[u8; 32]>::try_from(bytes).expect("test vector is 32 bytes"))
}

fn bytes100(bytes: &[u8]) -> Bytes100 {
    Bytes100::from(<[u8; 100]>::try_from(bytes).expect("test vector is 100 bytes"))
}
