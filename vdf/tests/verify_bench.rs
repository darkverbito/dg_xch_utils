// A/B microbenchmark for the hash_prime / discriminant-derivation path (wall-clock
// per-op, before/after on the same machine).
//
//   cargo test --release -p dg_xch_vdf --test verify_bench -- --ignored --nocapture
//
// Two probes:
//  * repeated-challenge verify: the depth-0 chia vdf.txt fixture verified N times with the SAME
//    challenge — models the measured genesis-era recurrence (5,098 derivations / 1,094 unique
//    challenges over mainnet blocks 0..=1023; hottest challenge derived 110x).
//  * distinct-seed discriminants: create_discriminant_bytes over N unique seeds — the uncached
//    hash_prime cost itself (reps / primality-path changes show up here).

use dg_xch_vdf::{create_discriminant_bytes, verify_vdf};
use std::time::Instant;

fn hex_to_bytes(hex: &str) -> Vec<u8> {
    hex::decode(hex).expect("valid hex")
}

#[test]
#[ignore = "manual A/B benchmark"]
fn bench_repeated_challenge_verify_and_distinct_seed_discriminants() {
    let challenge =
        hex_to_bytes("9104c5b5e45d48f374efa0488fe6a617790e9aecb3c9cddec06809b09f45ce9b");
    let mut default_element = [0u8; 100];
    default_element[0] = 0x08;
    let proof = hex_to_bytes(
        "0200553bf0f382fc65a94f20afad5dbce2c1ee8ba3bf93053559ac9960c8fd80ac2222e9b649701a4141a4d8999f0dbfe0c39ea744096598a7528328e5199f0aa30aec8aae8ab5018bf1245329a8272ddff1afbd87ad2eaba1b7fd57bd25edc62e0b010000003f0ffcd0dc307a2aa4678bafba661c77d176ef23afc86e7ea9f4f9eac52b8e1850748019245ecc96547da9b731dc72cded5582a9b0c63e13fd42446c7b28b41d3ded1d0b666d5ddb5b29719e4ebe70969e67e42ddd8591eae60d83dbe619f1250400",
    );

    // Warm-up (first derivation populates any cache; also faults in the fixture path).
    assert!(verify_vdf(
        &challenge,
        &default_element,
        &proof,
        1024,
        129_499_136,
        0
    ));

    const VERIFY_REPS: u32 = 20;
    let start = Instant::now();
    for _ in 0..VERIFY_REPS {
        assert!(verify_vdf(
            &challenge,
            &default_element,
            &proof,
            1024,
            129_499_136,
            0
        ));
    }
    let per_verify = start.elapsed() / VERIFY_REPS;
    println!("repeated-challenge verify: {per_verify:?}/op over {VERIFY_REPS} reps");

    const SEED_REPS: u32 = 50;
    let start = Instant::now();
    for i in 0..SEED_REPS {
        let mut seed = [0u8; 32];
        seed[0] = 0xA5;
        seed[28..32].copy_from_slice(&i.to_be_bytes());
        let d = create_discriminant_bytes(&seed, 1024).expect("derivation succeeds");
        assert_eq!(d.len(), 128);
    }
    let per_disc = start.elapsed() / SEED_REPS;
    println!("distinct-seed 1024-bit discriminant: {per_disc:?}/op over {SEED_REPS} seeds");
}
