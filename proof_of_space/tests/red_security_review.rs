//! Red demonstrations for docs/security-review-2026-09.md (findings 4 and 8). Each test asserts
//! the SAFE behavior and fails until its finding's fix lands; run with
//! `cargo test -p dg_xch_pos --test red_security_review -- --ignored`. When a fix lands, remove
//! the ignore and keep the test as the regression gate.

use dg_xch_pos::constants::HEADER_MAGIC;
use dg_xch_pos::plots::plot_reader::read_plot_header;
use dg_xch_pos::verifier::uncompress_proof;
use std::io::Write;

// Finding 4: a peer-supplied proof shorter than the k-derived minimum must be rejected by the
// verifier, not panic the calling task on an out-of-bounds bit read.
#[test]
#[ignore = "red: finding 4 in docs/security-review-2026-09.md — an empty proof panics uncompress_proof"]
fn an_empty_proof_is_rejected_not_a_panic() {
    let outcome = std::panic::catch_unwind(|| uncompress_proof(&[], 32));
    assert!(
        outcome.is_ok(),
        "an empty proof vector must produce an error or empty result, never a panic"
    );
}

// Finding 8: header length fields are attacker-controlled input; an oversized value must be a
// parse error, never a slice panic.
#[test]
#[ignore = "red: finding 8 in docs/security-review-2026-09.md — an oversized header length panics the reader"]
fn an_oversized_header_length_is_a_parse_error_not_a_panic() {
    let mut buffer = [0u8; 320];
    buffer[0..19].copy_from_slice(&HEADER_MAGIC);
    // id (32) + k (1) are zeros; the format-description length claims 0xffff bytes of a
    // 320-byte header.
    buffer[52] = 0xff;
    buffer[53] = 0xff;
    let dir = std::env::temp_dir().join(format!(
        "red_plot_header_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("crafted.plot");
    std::fs::File::create(&path)
        .unwrap()
        .write_all(&buffer)
        .unwrap();
    let mut file = std::fs::File::open(&path).unwrap();
    let outcome = std::panic::catch_unwind(move || read_plot_header(&mut file));
    let _ = std::fs::remove_dir_all(&dir);
    match outcome {
        Ok(parsed) => assert!(
            parsed.is_err(),
            "an impossible length field must fail the parse"
        ),
        Err(_) => panic!("a crafted header length must be a parse error, never a panic"),
    }
}
