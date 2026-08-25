use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_pos::pos2::aes_hash::{AES_G_ROUNDS, AesHash};
use std::time::Instant;

fn plot_id() -> Bytes32 {
    let mut bytes = [0u8; 32];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = (i * 11 + 5) as u8;
    }
    Bytes32::from(bytes)
}

/// Throughput of the plot hash, for comparison against the reference implementation. Ignored by
/// default: it is a measurement, not an assertion.
#[test]
#[ignore = "benchmark"]
fn aes_hash_throughput() {
    let hasher = AesHash::new(&plot_id(), 28);
    let n: u64 = 3_000_000;

    let mut sink = 0u32;
    let start = Instant::now();
    for i in 0..n {
        sink ^= hasher.g_x(i as u32, AES_G_ROUNDS);
    }
    let secs = start.elapsed().as_secs_f64();
    println!(
        "rust                   g_x  {:10.0} ops/s  {:8.1} ns/op",
        n as f64 / secs,
        secs * 1e9 / n as f64
    );

    let mut sink2 = 0u32;
    let start = Instant::now();
    for i in 0..n {
        sink2 ^= hasher.pairing(i, i.wrapping_mul(3), 0)[0];
    }
    let secs = start.elapsed().as_secs_f64();
    println!(
        "rust                   pair {:10.0} ops/s  {:8.1} ns/op",
        n as f64 / secs,
        secs * 1e9 / n as f64
    );
    let xs: Vec<u32> = (0..n as u32).collect();
    let mut out = vec![0u32; xs.len()];
    let start = Instant::now();
    hasher.g_x_batch(&xs, &mut out, AES_G_ROUNDS);
    let secs = start.elapsed().as_secs_f64();
    println!(
        "rust batched           g_x  {:10.0} ops/s  {:8.1} ns/op",
        n as f64 / secs,
        secs * 1e9 / n as f64
    );
    assert!(sink != 1 || sink2 != 1 || out[0] == 1);
}
