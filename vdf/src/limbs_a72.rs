//! aarch64 kernels for the limb loops the profile put at the top of the Pi-4's stack —
//! kept as a measured NEGATIVE result, not used in production.
//!
//! The hypothesis was that the compiler's scheduling of the portable u128 carry chains left
//! a gap on the Cortex-A72 (identical source runs ~40 µs per squaring op there against 26 on
//! a Xeon). Measured on the idle Pi-4, these hand-held `mul`/`umulh`/`adcs` loops run
//! 0.92–1.00× of the portable rows: LLVM was already at the floor, and the A72/Xeon gap is
//! the multiplier's pipelining, not instruction order. See docs/algorithmic-finality.md §1c.
//!
//! The kernels stay behind their gates: `kernels_match_portable_rows` pins byte-identical
//! outputs, `kernel_bench_rows` re-measures the verdict on any ARM core in seconds — no sync
//! required. Non-aarch64 targets never compile this module; production dispatch is portable
//! everywhere.

/// One fused row of `out = X·a1 + Y·a2`: processes `n` limbs from `x`/`y` into `out`,
/// carrying a 65-bit accumulator as (lo, hi). Returns the final (c_lo, c_hi) exactly as the
/// portable loop does.
///
/// # Safety
/// `x` and `y` must be readable for `n` limbs; `out` writable for `n` limbs.
#[cfg(target_arch = "aarch64")]
#[allow(dead_code)]
pub(crate) unsafe fn addmul2_rows(
    x: *const u64,
    y: *const u64,
    n: usize,
    a1: u64,
    a2: u64,
    out: *mut u64,
) -> (u64, u64) {
    let mut c_lo: u64 = 0;
    let mut c_hi: u64 = 0;
    if n == 0 {
        return (0, 0);
    }
    unsafe {
        core::arch::asm!(
            "2:",
            "ldr {xi}, [{x}], #8",
            "ldr {yi}, [{y}], #8",
            "mul {pl}, {xi}, {a1}",
            "umulh {ph}, {xi}, {a1}",
            "mul {ql}, {yi}, {a2}",
            "umulh {qh}, {yi}, {a2}",
            // acc = (c_lo, c_hi, 0) + (pl, ph) + (ql, qh); emit acc0, carry = (acc1, acc2)
            "adds {c_lo}, {c_lo}, {pl}",
            "adcs {c_hi}, {c_hi}, {ph}",
            "cset {t0}, cs",
            "adds {c_lo}, {c_lo}, {ql}",
            "adcs {c_hi}, {c_hi}, {qh}",
            "cinc {t0}, {t0}, cs",
            "str {c_lo}, [{out}], #8",
            "mov {c_lo}, {c_hi}",
            "mov {c_hi}, {t0}",
            "subs {n}, {n}, #1",
            "b.ne 2b",
            x = inout(reg) x => _,
            y = inout(reg) y => _,
            out = inout(reg) out => _,
            n = inout(reg) n => _,
            a1 = in(reg) a1,
            a2 = in(reg) a2,
            c_lo = inout(reg) c_lo,
            c_hi = inout(reg) c_hi,
            xi = out(reg) _,
            yi = out(reg) _,
            pl = out(reg) _,
            ph = out(reg) _,
            ql = out(reg) _,
            qh = out(reg) _,
            t0 = out(reg) _,
            options(nostack),
        );
    }
    (c_lo, c_hi)
}

/// One fused row of `out = X·a1 − Y·a2` in the portable loop's exact discipline: separate
/// carry chains for the P and Q streams and a running borrow. Returns (cp, cq, borrow).
///
/// # Safety
/// `x` and `y` must be readable for `n` limbs; `out` writable for `n` limbs.
#[cfg(target_arch = "aarch64")]
#[allow(dead_code)]
pub(crate) unsafe fn submul2_rows(
    x: *const u64,
    y: *const u64,
    n: usize,
    a1: u64,
    a2: u64,
    out: *mut u64,
) -> (u64, u64, u64) {
    let mut cp: u64 = 0;
    let mut cq: u64 = 0;
    let mut borrow: u64 = 0;
    if n == 0 {
        return (0, 0, 0);
    }
    unsafe {
        core::arch::asm!(
            "2:",
            "ldr {xi}, [{x}], #8",
            "ldr {yi}, [{y}], #8",
            "mul {pl}, {xi}, {a1}",
            "umulh {ph}, {xi}, {a1}",
            "mul {ql}, {yi}, {a2}",
            "umulh {qh}, {yi}, {a2}",
            // P stream: pl += cp, cp' = ph + carry
            "adds {pl}, {pl}, {cp}",
            "cinc {cp}, {ph}, cs",
            // Q stream: ql += cq, cq' = qh + carry
            "adds {ql}, {ql}, {cq}",
            "cinc {cq}, {qh}, cs",
            // d = pl - ql - borrow; borrow' = b1 + b2 (each 0/1)
            "subs {pl}, {pl}, {ql}",
            "cset {t0}, cc",
            "subs {pl}, {pl}, {borrow}",
            "cinc {t0}, {t0}, cc",
            "mov {borrow}, {t0}",
            "str {pl}, [{out}], #8",
            "subs {n}, {n}, #1",
            "b.ne 2b",
            x = inout(reg) x => _,
            y = inout(reg) y => _,
            out = inout(reg) out => _,
            n = inout(reg) n => _,
            a1 = in(reg) a1,
            a2 = in(reg) a2,
            cp = inout(reg) cp,
            cq = inout(reg) cq,
            borrow = inout(reg) borrow,
            xi = out(reg) _,
            yi = out(reg) _,
            pl = out(reg) _,
            ph = out(reg) _,
            ql = out(reg) _,
            qh = out(reg) _,
            t0 = out(reg) _,
            options(nostack),
        );
    }
    (cp, cq, borrow)
}

/// One schoolbook row: `out[..n] += a · y[..n]`, returning the final carry — the addmul_1
/// shape `Sw::mul` iterates.
///
/// # Safety
/// `y` must be readable and `out` readable+writable for `n` limbs.
#[cfg(target_arch = "aarch64")]
#[allow(dead_code)]
pub(crate) unsafe fn addmul1_row(y: *const u64, n: usize, a: u64, out: *mut u64) -> u64 {
    let mut carry: u64 = 0;
    if n == 0 {
        return 0;
    }
    unsafe {
        core::arch::asm!(
            "2:",
            "ldr {yi}, [{y}], #8",
            "ldr {oi}, [{out}]",
            "mul {pl}, {yi}, {a}",
            "umulh {ph}, {yi}, {a}",
            "adds {pl}, {pl}, {oi}",
            "cinc {ph}, {ph}, cs",
            "adds {pl}, {pl}, {carry}",
            "cinc {carry}, {ph}, cs",
            "str {pl}, [{out}], #8",
            "subs {n}, {n}, #1",
            "b.ne 2b",
            y = inout(reg) y => _,
            out = inout(reg) out => _,
            n = inout(reg) n => _,
            a = in(reg) a,
            carry = inout(reg) carry,
            yi = out(reg) _,
            oi = out(reg) _,
            pl = out(reg) _,
            ph = out(reg) _,
            options(nostack),
        );
    }
    carry
}
