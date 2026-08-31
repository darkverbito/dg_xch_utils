//! Fixed-width stack integers for the class-group GCD hot loops.
//!
//! chiavdf's remaining structural speed technique: the Lehmer loops' big-integer updates
//! (`x·w1 + y·w2` with word-size `w`) run on bounded operands (≤ ~1088 bits), so they fit in a
//! fixed `[u64; LIMBS]` on the stack — no allocation, no `BigInt` bookkeeping. Only the rare
//! exact-division fallback converts back to `BigInt`. Every op is differentially property-tested
//! against `num_bigint` below.

use num_bigint::{BigInt, Sign};

// Width is const-generic: the GCD loops use the narrow SwGcd (20 limbs — operands <= ~1088 bits,
// and small Copy structs keep the Lehmer loops fast); the composition assembly uses the wide
// SwWide (34 limbs — a1^2 and v2*c1 reach ~2048 bits).

// Möller–Granlund 2-by-1 division: one reciprocal per divisor, then every 128÷64 step is
// multiplies and single-word corrections. The straightforward u128 quotient at these sites
// compiles to a software builtin on aarch64 (no wide divide) — measured at 3% of whole-node
// cycles on the Pi-4, on top of the divide latency itself. Const so per-divisor constants can
// be folded at compile time.
#[inline]
pub(crate) const fn recip_2by1(d: u64) -> u64 {
    debug_assert!(d >> 63 == 1, "reciprocal needs a normalized divisor");
    // The one u128 divide left: once per divisor, amortized across every limb step.
    ((u128::MAX / (d as u128)) - (1u128 << 64)) as u64
}

/// Exact `(u1·B + u0) / d` with `u1 < d` and `d` normalized — identical output to the u128
/// quotient it replaces.
#[inline]
pub(crate) const fn div_2by1(u1: u64, u0: u64, d: u64, v: u64) -> (u64, u64) {
    debug_assert!(d >> 63 == 1);
    debug_assert!(u1 < d);
    let q = (v as u128) * (u1 as u128) + (((u1 as u128) << 64) | (u0 as u128));
    let mut q1 = ((q >> 64) as u64).wrapping_add(1);
    let q0 = q as u64;
    let mut r = u0.wrapping_sub(q1.wrapping_mul(d));
    if r > q0 {
        q1 = q1.wrapping_sub(1);
        r = r.wrapping_add(d);
    }
    if r >= d {
        q1 = q1.wrapping_add(1);
        r -= d;
    }
    (q1, r)
}

/// A signed fixed-width integer: sign + little-endian magnitude with an explicit length.
#[derive(Clone, Copy, Debug)]
pub struct Sw<const N: usize> {
    pub neg: bool,
    len: usize,
    d: [u64; N],
}

pub type SwGcd = Sw<20>;
#[allow(dead_code)] // consumed by the assembly conversion staging
pub type SwWide = Sw<34>;

impl<const N: usize> Sw<N> {
    #[must_use]
    pub fn zero() -> Self {
        Self {
            neg: false,
            len: 0,
            d: [0; N],
        }
    }

    #[must_use]
    pub fn from_bigint(v: &BigInt) -> Self {
        let digits = v.magnitude().to_u64_digits();
        assert!(digits.len() <= N, "value exceeds fixed limb width");
        let mut d = [0u64; N];
        d[..digits.len()].copy_from_slice(&digits);
        Self {
            neg: v.sign() == Sign::Minus,
            len: digits.len(),
            d,
        }
    }

    #[must_use]
    pub fn to_bigint(self) -> BigInt {
        let mut bytes = Vec::with_capacity(self.len * 8);
        for i in 0..self.len {
            bytes.extend_from_slice(&self.d[i].to_le_bytes());
        }
        let mag = num_bigint::BigUint::from_bytes_le(&bytes);
        if self.neg && self.len > 0 {
            -BigInt::from(mag)
        } else {
            BigInt::from(mag)
        }
    }

    #[must_use]
    pub fn one() -> Self {
        let mut s = Self::zero();
        s.d[0] = 1;
        s.len = 1;
        s
    }

    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.len == 0
    }

    #[must_use]
    pub fn is_one(&self) -> bool {
        !self.neg && self.len == 1 && self.d[0] == 1
    }

    #[must_use]
    pub fn is_negative(&self) -> bool {
        self.neg && self.len > 0
    }

    pub fn negate(&mut self) {
        if self.len > 0 {
            self.neg = !self.neg;
        }
    }

    /// Bit length of the magnitude (0 for zero), matching `mpz_sizeinbase(x, 2)`'s ≥ 1 convention
    /// at the caller.
    #[must_use]
    pub fn bit_len(&self) -> i64 {
        if self.len == 0 {
            0
        } else {
            (self.len as i64 - 1) * 64 + (64 - self.d[self.len - 1].leading_zeros() as i64)
        }
    }

    /// The low 64 bits of `magnitude >> shift`, reinterpreted as `i64` (callers pick `shift` so the
    /// result is ~63 bits and non-negative) — the limb analog of `u64_low_word(&(x >> shift))`.
    #[must_use]
    pub fn extract_word(&self, shift: i64) -> i128 {
        let w = if shift <= 0 {
            self.d.first().copied().unwrap_or(0)
        } else {
            let s = shift as usize;
            let idx = s / 64;
            let off = s % 64;
            let lo = self.d.get(idx).copied().unwrap_or(0);
            if off == 0 {
                lo
            } else {
                let hi = self.d.get(idx + 1).copied().unwrap_or(0);
                (lo >> off) | (hi << (64 - off))
            }
        };
        i64::from_ne_bytes(w.to_ne_bytes()) as i128
    }

    /// Magnitude comparison.
    #[must_use]
    pub fn cmp_mag(&self, other: &Self) -> core::cmp::Ordering {
        if self.len != other.len {
            return self.len.cmp(&other.len);
        }
        for i in (0..self.len).rev() {
            if self.d[i] != other.d[i] {
                return self.d[i].cmp(&other.d[i]);
            }
        }
        core::cmp::Ordering::Equal
    }

    fn trim(&mut self) {
        while self.len > 0 && self.d[self.len - 1] == 0 {
            self.len -= 1;
        }
        if self.len == 0 {
            self.neg = false;
        }
    }

    /// Signed addition of magnitudes-with-signs.
    fn add_signed(a: &Self, b: &Self) -> Self {
        if a.is_zero() {
            return *b;
        }
        if b.is_zero() {
            return *a;
        }
        if a.neg == b.neg {
            // Same sign: magnitude add.
            let mut out = Self::zero();
            let n = a.len.max(b.len);
            let mut carry: u128 = 0;
            for i in 0..n {
                let s = u128::from(a.d[i]) + u128::from(b.d[i]) + carry;
                out.d[i] = s as u64;
                carry = s >> 64;
            }
            let mut len = n;
            if carry != 0 {
                assert!(len < N, "limb overflow in add");
                out.d[len] = carry as u64;
                len += 1;
            }
            out.len = len;
            out.neg = a.neg;
            out
        } else {
            // Opposite signs: subtract smaller magnitude from larger.
            let (big, small, neg) = match a.cmp_mag(b) {
                core::cmp::Ordering::Less => (b, a, b.neg),
                core::cmp::Ordering::Greater => (a, b, a.neg),
                core::cmp::Ordering::Equal => return Self::zero(),
            };
            let mut out = Self::zero();
            let mut borrow: i128 = 0;
            for i in 0..big.len {
                let s = i128::from(big.d[i]) - i128::from(small.d[i]) - borrow;
                if s < 0 {
                    out.d[i] = (s + (1i128 << 64)) as u64;
                    borrow = 1;
                } else {
                    out.d[i] = s as u64;
                    borrow = 0;
                }
            }
            out.len = big.len;
            out.neg = neg;
            out.trim();
            out
        }
    }

    /// Re-width to `M` limbs (asserts the value fits) — the narrow-GCD ↔ wide-assembly bridge.
    #[must_use]
    pub fn resize<const M: usize>(&self) -> Sw<M> {
        assert!(self.len <= M, "value exceeds target limb width");
        let mut d = [0u64; M];
        d[..self.len].copy_from_slice(&self.d[..self.len]);
        Sw {
            neg: self.neg,
            len: self.len,
            d,
        }
    }

    /// Signed addition.
    #[must_use]
    pub fn add(&self, other: &Self) -> Self {
        Self::add_signed(self, other)
    }

    /// Signed subtraction.
    #[must_use]
    pub fn sub(&self, other: &Self) -> Self {
        let mut n = *other;
        n.negate();
        Self::add_signed(self, &n)
    }

    /// Full multi-limb schoolbook multiply (signed). (Wired into the composition assembly next;
    /// property-tested below.)
    #[must_use]
    #[allow(dead_code)]
    pub fn mul(&self, other: &Self) -> Self {
        let mut out = Self::zero();
        if self.len == 0 || other.len == 0 {
            return out;
        }
        assert!(self.len + other.len <= N, "limb overflow in mul");
        for i in 0..self.len {
            out.d[i + other.len] = Self::addmul1(&other.d, other.len, self.d[i], &mut out.d[i..]);
        }
        out.len = self.len + other.len;
        out.neg = self.neg != other.neg;
        out.trim();
        out
    }

    /// Magnitude divide-and-remainder (Knuth Algorithm D; single-word fast path). Ignores signs.
    #[allow(dead_code)]
    fn divrem_mag(&self, v: &Self) -> (Self, Self) {
        assert!(v.len > 0, "division by zero");
        if self.cmp_mag(v) == core::cmp::Ordering::Less {
            let mut r = *self;
            r.neg = false;
            return (Self::zero(), r);
        }
        if v.len == 1 {
            // Single-word divisor: normalize once, then reciprocal steps. (A·2^s)/(d·2^s)
            // equals A/d with the remainder scaled by 2^s; the pre-shift spill seeds the
            // running remainder and is < 2^s ≤ the normalized divisor.
            let s = v.d[0].leading_zeros();
            let dn = v.d[0] << s;
            let vr = recip_2by1(dn);
            let mut q = Self::zero();
            let mut rem: u64 = if s == 0 {
                0
            } else {
                self.d[self.len - 1] >> (64 - s)
            };
            for i in (0..self.len).rev() {
                let lo = if i == 0 { 0 } else { self.d[i - 1] };
                let cur = if s == 0 {
                    self.d[i]
                } else {
                    (self.d[i] << s) | (lo >> (64 - s))
                };
                let (qi, r) = div_2by1(rem, cur, dn, vr);
                q.d[i] = qi;
                rem = r;
            }
            q.len = self.len;
            q.trim();
            let mut r = Self::zero();
            if rem >> s != 0 {
                r.d[0] = rem >> s;
                r.len = 1;
            }
            return (q, r);
        }
        // Knuth D: normalize so the divisor's top limb has its high bit set.
        let s = v.d[v.len - 1].leading_zeros() as usize;
        let n = v.len;
        let m = self.len - n;
        // un: normalized dividend with one extra limb; vn: normalized divisor.
        debug_assert!(N < 64);
        let mut un = [0u64; 65];
        let mut vn = [0u64; 64];
        if s == 0 {
            un[..self.len].copy_from_slice(&self.d[..self.len]);
            vn[..n].copy_from_slice(&v.d[..n]);
        } else {
            for i in (1..self.len).rev() {
                un[i] = (self.d[i] << s) | (self.d[i - 1] >> (64 - s));
            }
            un[0] = self.d[0] << s;
            un[self.len] = self.d[self.len - 1] >> (64 - s);
            for i in (1..n).rev() {
                vn[i] = (v.d[i] << s) | (v.d[i - 1] >> (64 - s));
            }
            vn[0] = v.d[0] << s;
        }
        let vtop = u128::from(vn[n - 1]);
        let vnext = u128::from(vn[n - 2]);
        let vrecip = recip_2by1(vn[n - 1]);
        let mut q = Self::zero();
        for j in (0..=m).rev() {
            // Reciprocal estimate when the strict u1 < d precondition holds; the rare
            // top-limb-equal case keeps the u128 quotient so the correction loop sees
            // byte-identical inputs either way.
            let (mut qhat, mut rhat) = if un[j + n] >= vn[n - 1] {
                let num = (u128::from(un[j + n]) << 64) | u128::from(un[j + n - 1]);
                (num / vtop, num % vtop)
            } else {
                let (qh, rh) = div_2by1(un[j + n], un[j + n - 1], vn[n - 1], vrecip);
                (u128::from(qh), u128::from(rh))
            };
            while qhat >> 64 != 0 || qhat * vnext > ((rhat << 64) | u128::from(un[j + n - 2])) {
                qhat -= 1;
                rhat += vtop;
                if rhat >> 64 != 0 {
                    break;
                }
            }
            // Multiply-subtract qhat·vn from un[j..=j+n].
            let mut borrow: i128 = 0;
            let mut carry: u128 = 0;
            for i in 0..n {
                let p = qhat * u128::from(vn[i]) + carry;
                carry = p >> 64;
                let sub = i128::from(un[j + i]) - i128::from(p as u64) - borrow;
                if sub < 0 {
                    un[j + i] = (sub + (1i128 << 64)) as u64;
                    borrow = 1;
                } else {
                    un[j + i] = sub as u64;
                    borrow = 0;
                }
            }
            let sub = i128::from(un[j + n]) - i128::from(carry as u64) - borrow;
            if sub < 0 {
                // qhat was one too large: add back.
                un[j + n] = (sub + (1i128 << 64)) as u64;
                qhat -= 1;
                let mut c: u128 = 0;
                for i in 0..n {
                    let a = u128::from(un[j + i]) + u128::from(vn[i]) + c;
                    un[j + i] = a as u64;
                    c = a >> 64;
                }
                un[j + n] = (u128::from(un[j + n]) + c) as u64;
            } else {
                un[j + n] = sub as u64;
            }
            q.d[j] = qhat as u64;
        }
        q.len = m + 1;
        q.trim();
        // Denormalize the remainder.
        let mut r = Self::zero();
        if s == 0 {
            r.d[..n].copy_from_slice(&un[..n]);
        } else {
            for i in 0..n - 1 {
                r.d[i] = (un[i] >> s) | (un[i + 1] << (64 - s));
            }
            r.d[n - 1] = un[n - 1] >> s;
        }
        r.len = n;
        r.trim();
        (q, r)
    }

    /// Floor division with remainder (signs like `Integer::div_mod_floor`): `r` has the divisor's
    /// sign, `self = q·v + r`.
    #[must_use]
    #[allow(dead_code)]
    pub fn div_mod_floor(&self, v: &Self) -> (Self, Self) {
        let (mut q, mut r) = self.divrem_mag(v);
        let sneg = self.is_negative();
        let vneg = v.is_negative();
        if sneg != vneg {
            q.negate();
            if !r.is_zero() {
                // Floor adjustment: q -= 1; r = |v| - r, with the divisor's sign.
                q = Self::add_signed(&q, &{
                    let mut m1 = Self::one();
                    m1.negate();
                    m1
                });
                let mut vv = *v;
                vv.neg = false;
                r.negate();
                r = Self::add_signed(&vv, &r);
            }
        }
        if vneg && !r.is_zero() {
            r.neg = true;
        }
        (q, r)
    }

    /// Exact division (caller guarantees divisibility) — signs multiply.
    #[must_use]
    #[allow(dead_code)]
    pub fn div_exact(&self, v: &Self) -> Self {
        let (mut q, r) = self.divrem_mag(v);
        debug_assert!(r.is_zero(), "div_exact on non-divisible input");
        let _ = r;
        q.neg = self.neg != v.neg;
        if q.len == 0 {
            q.neg = false;
        }
        q
    }

    /// Exact right shift by one bit (caller guarantees the value is even; floor == exact there,
    /// so the magnitude shift is sign-correct).
    #[must_use]
    pub fn shr1_exact(&self) -> Self {
        debug_assert!(
            self.len == 0 || self.d[0] & 1 == 0,
            "shr1_exact on odd value"
        );
        let mut out = *self;
        for i in 0..self.len {
            let hi = if i + 1 < self.len { self.d[i + 1] } else { 0 };
            out.d[i] = (self.d[i] >> 1) | (hi << 63);
        }
        out.trim();
        out
    }

    /// Left shift by one bit.
    #[must_use]
    #[allow(dead_code)]
    pub fn shl1(&self) -> Self {
        let mut out = *self;
        let mut carry = 0u64;
        for i in 0..self.len {
            let nc = self.d[i] >> 63;
            out.d[i] = (self.d[i] << 1) | carry;
            carry = nc;
        }
        if carry != 0 {
            assert!(self.len < N, "limb overflow in shl1");
            out.d[self.len] = carry;
            out.len = self.len + 1;
        }
        out
    }

    /// `self·w1 + other·w2` with word coefficients — the Lehmer matrix-application primitive and
    /// ~77% of class-group squaring (both Lehmer loops apply their 2×2 matrix through it).
    ///
    /// Single fused pass (chiavdf's fixed-limb technique), shaped for the hardware carry units:
    /// the sign-magnitude three-pass dance (two `mul_word_mag` + `add_signed` with a magnitude
    /// compare) collapses into one sweep of pure u64 `overflowing_add`/`overflowing_sub` chains —
    /// exactly the adc/sbb structure hand assembly would use, which LLVM lowers to it. Same
    /// effective signs run the fused add; opposite signs the fused subtract in two's complement
    /// with one complement pass when the result is negative.
    #[must_use]
    pub fn linear2(&self, w1: i128, other: &Self, w2: i128) -> Self {
        let a1 = w1.unsigned_abs() as u64;
        let a2 = w2.unsigned_abs() as u64;
        // Effective sign of each contribution: operand sign XOR coefficient sign.
        let s1 = (w1 < 0) != self.neg;
        let s2 = (w2 < 0) != other.neg;
        let n = self.len.max(other.len);
        debug_assert!(n + 2 <= N, "limb overflow in linear2");
        let mut out = Self::zero();
        if s1 == s2 {
            let (c_lo, c_hi) = Self::addmul2(&self.d, &other.d, n, a1, a2, &mut out.d);
            out.d[n] = c_lo;
            out.d[n + 1] = c_hi;
            out.neg = s1;
        } else {
            // Fused submul_2: out = ±(X·a1 − Y·a2) in two's complement — three short chains
            // (P-accumulate, Q-accumulate, borrow), the sbb shape. A set final borrow means the
            // true value is negative: complement to magnitude, sign flips to s2's contribution.
            let (mut cp, mut cq, mut borrow) =
                Self::submul2(&self.d, &other.d, n, a1, a2, &mut out.d);
            // Drain the carries: two more digits of P − Q − borrow.
            for i in n..n + 2 {
                let (d, b1) = cp.overflowing_sub(cq);
                let (d, b2) = d.overflowing_sub(borrow);
                out.d[i] = d;
                borrow = u64::from(b1) + u64::from(b2);
                cp = 0;
                cq = 0;
            }
            if borrow != 0 {
                // Negative two's complement: take the magnitude (invert + increment).
                let mut inc: u64 = 1;
                for i in 0..n + 2 {
                    let (v, o) = (!out.d[i]).overflowing_add(inc);
                    out.d[i] = v;
                    inc = u64::from(o);
                }
                out.neg = s2;
            } else {
                out.neg = s1;
            }
        }
        out.len = n + 2;
        out.trim();
        if out.len == 0 {
            out.neg = false;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_bigint::BigInt;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    fn sw_from_limbs(limbs: &[u64]) -> Sw<8> {
        let mut v = BigInt::from(0u8);
        for l in limbs.iter().rev() {
            v = (v << 64) + l;
        }
        Sw::<8>::from_bigint(&v)
    }

    // The division is consensus-adjacent (the Lehmer quotient and every reduction walks through
    // it), so its gate is a differential against bigint division rather than fixed vectors:
    // random shapes plus the patterns that historically break Knuth D — the qhat overestimate
    // (add-back branch), all-ones limbs, minimal normalized divisors, and single-word divisors.
    #[test]
    fn divrem_mag_matches_bigint_division() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut cases: Vec<(Vec<u64>, Vec<u64>)> = vec![
            // qhat = B-1 overestimate / add-back territory
            (vec![0, 0, 1 << 63, u64::MAX - 1], vec![1, 1 << 63]),
            (vec![0, u64::MAX, u64::MAX], vec![u64::MAX, 1 << 63]),
            // all-ones dividend, minimal normalized divisor
            (vec![u64::MAX; 6], vec![0, 1 << 63]),
            (vec![u64::MAX; 6], vec![1 << 63]),
            // single-word divisors incl. 1 and MAX
            (vec![u64::MAX, u64::MAX, u64::MAX], vec![1]),
            (vec![123, 456, 789], vec![u64::MAX]),
            // dividend < divisor
            (vec![7], vec![0, 1]),
            // exact multiples
            (vec![0, 0, 0, 1 << 63], vec![0, 1 << 63]),
        ];
        for _ in 0..4000 {
            let dl = 1 + (rng.next() as usize) % 6;
            let vl = 1 + (rng.next() as usize) % dl.max(1).min(4);
            let mut d: Vec<u64> = (0..dl).map(|_| rng.next()).collect();
            let mut v: Vec<u64> = (0..vl).map(|_| rng.next()).collect();
            // Bias toward carry-heavy limbs.
            if rng.next() % 3 == 0 {
                for x in &mut d {
                    *x |= 0xFFFF_FFFF_0000_0000;
                }
            }
            if rng.next() % 3 == 0 {
                for x in &mut v {
                    *x |= 0xFFFF_FFFF_FFFF_0000;
                }
            }
            if v.iter().all(|&x| x == 0) {
                v[0] = 1;
            }
            cases.push((d, v));
        }
        for (dl, vl) in cases {
            let a = sw_from_limbs(&dl);
            let b = sw_from_limbs(&vl);
            if b.len == 0 {
                continue;
            }
            let (q, r) = a.divrem_mag(&b);
            let (ab, bb) = (a.to_bigint(), b.to_bigint());
            assert_eq!(
                q.to_bigint(),
                &ab / &bb,
                "quotient diverged for {dl:x?} / {vl:x?}"
            );
            assert_eq!(
                r.to_bigint(),
                &ab % &bb,
                "remainder diverged for {dl:x?} / {vl:x?}"
            );
        }
    }
}

impl<const N: usize> Sw<N> {
    /// Fused `out[..n] = X·a1 + Y·a2` with the 65-bit (lo, hi) carry — kernel on aarch64,
    /// portable elsewhere. Both stay compiled on aarch64: the portable body is the kernel's
    /// oracle in the differential and its baseline in the bench.
    #[inline]
    fn addmul2(
        x: &[u64; N],
        y: &[u64; N],
        n: usize,
        a1: u64,
        a2: u64,
        out: &mut [u64; N],
    ) -> (u64, u64) {
        // The aarch64 kernel measured 0.92-1.00x of this portable loop (the compiler
        // already emits optimal chains for the A72); production stays portable and the
        // assembly lives on only through its differential and bench. See
        // docs/algorithmic-finality.md.
        Self::addmul2_portable(x, y, n, a1, a2, out)
    }

    #[allow(dead_code)]
    fn addmul2_portable(
        x: &[u64; N],
        y: &[u64; N],
        n: usize,
        a1: u64,
        a2: u64,
        out: &mut [u64; N],
    ) -> (u64, u64) {
        let mut c_lo: u64 = 0;
        let mut c_hi: u64 = 0;
        for i in 0..n {
            let p1 = u128::from(x[i]) * u128::from(a1);
            let p2 = u128::from(y[i]) * u128::from(a2);
            let (s, o1) = (p1 as u64).overflowing_add(p2 as u64);
            let (s, o2) = s.overflowing_add(c_lo);
            out[i] = s;
            let (h, oh1) = ((p1 >> 64) as u64).overflowing_add((p2 >> 64) as u64);
            let (h, oh2) = h.overflowing_add(c_hi + u64::from(o1) + u64::from(o2));
            c_lo = h;
            c_hi = u64::from(oh1) + u64::from(oh2);
        }
        (c_lo, c_hi)
    }

    /// Fused `out[..n] = X·a1 − Y·a2` with separate P/Q carries and a running borrow.
    #[inline]
    fn submul2(
        x: &[u64; N],
        y: &[u64; N],
        n: usize,
        a1: u64,
        a2: u64,
        out: &mut [u64; N],
    ) -> (u64, u64, u64) {
        Self::submul2_portable(x, y, n, a1, a2, out)
    }

    #[allow(dead_code)]
    fn submul2_portable(
        x: &[u64; N],
        y: &[u64; N],
        n: usize,
        a1: u64,
        a2: u64,
        out: &mut [u64; N],
    ) -> (u64, u64, u64) {
        let mut cp: u64 = 0;
        let mut cq: u64 = 0;
        let mut borrow: u64 = 0;
        for i in 0..n {
            let p = u128::from(x[i]) * u128::from(a1);
            let q = u128::from(y[i]) * u128::from(a2);
            let (pl, pc) = (p as u64).overflowing_add(cp);
            cp = (p >> 64) as u64 + u64::from(pc);
            let (ql, qc) = (q as u64).overflowing_add(cq);
            cq = (q >> 64) as u64 + u64::from(qc);
            let (d, b1) = pl.overflowing_sub(ql);
            let (d, b2) = d.overflowing_sub(borrow);
            out[i] = d;
            borrow = u64::from(b1) + u64::from(b2);
        }
        (cp, cq, borrow)
    }

    /// One schoolbook row: `out[..n] += a·y[..n]`, returning the carry limb.
    #[inline]
    fn addmul1(y: &[u64; N], n: usize, a: u64, out: &mut [u64]) -> u64 {
        Self::addmul1_portable(y, n, a, out)
    }

    #[allow(dead_code)]
    fn addmul1_portable(y: &[u64; N], n: usize, a: u64, out: &mut [u64]) -> u64 {
        let mut carry: u128 = 0;
        let a = u128::from(a);
        for j in 0..n {
            let p = a * u128::from(y[j]) + u128::from(out[j]) + carry;
            out[j] = p as u64;
            carry = p >> 64;
        }
        carry as u64
    }
}

#[cfg(test)]
mod kernel_tests {
    use super::*;
    use num_bigint::BigInt;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    #[cfg(target_arch = "aarch64")]
    fn addmul2_kernel(
        x: &[u64; 34],
        y: &[u64; 34],
        n: usize,
        a1: u64,
        a2: u64,
        out: &mut [u64; 34],
    ) -> (u64, u64) {
        // SAFETY: fixed 34-limb arrays, n <= 30 in every caller here.
        unsafe {
            crate::limbs_a72::addmul2_rows(x.as_ptr(), y.as_ptr(), n, a1, a2, out.as_mut_ptr())
        }
    }

    #[cfg(target_arch = "aarch64")]
    fn submul2_kernel(
        x: &[u64; 34],
        y: &[u64; 34],
        n: usize,
        a1: u64,
        a2: u64,
        out: &mut [u64; 34],
    ) -> (u64, u64, u64) {
        // SAFETY: as above.
        unsafe {
            crate::limbs_a72::submul2_rows(x.as_ptr(), y.as_ptr(), n, a1, a2, out.as_mut_ptr())
        }
    }

    #[cfg(target_arch = "aarch64")]
    fn addmul1_kernel(y: &[u64; 34], n: usize, a: u64, out: &mut [u64]) -> u64 {
        // SAFETY: as above.
        unsafe { crate::limbs_a72::addmul1_row(y.as_ptr(), n, a, out.as_mut_ptr()) }
    }

    fn sw_from_limbs(limbs: &[u64]) -> Sw<34> {
        let mut v = BigInt::from(0u8);
        for l in limbs.iter().rev() {
            v = (v << 64) + l;
        }
        Sw::<34>::from_bigint(&v)
    }

    // The refactor gate: linear2 and mul against bigint arithmetic, so cutting the row loops
    // out into dispatchable kernels provably changed nothing on any target — and on aarch64
    // this same oracle proves the assembly end-to-end.
    #[test]
    fn linear2_and_mul_match_bigint() {
        let mut rng = Rng(0xA5A5_5A5A_DEAD_BEEF);
        let mut cases = 0u64;
        for round in 0..6_000u64 {
            let xl = 1 + (rng.next() as usize) % 12;
            let yl = 1 + (rng.next() as usize) % 12;
            let mut xd: Vec<u64> = (0..xl).map(|_| rng.next()).collect();
            let mut yd: Vec<u64> = (0..yl).map(|_| rng.next()).collect();
            // Carry-dense shapes every third round.
            if round % 3 == 0 {
                for v in &mut xd {
                    *v |= 0xFFFF_FFFF_FF00_0000;
                }
                for v in &mut yd {
                    *v = v.wrapping_mul(0xFF00_0000_0000_0001) | 1;
                }
            }
            let x = sw_from_limbs(&xd);
            let y = sw_from_limbs(&yd);
            let (xb, yb) = (x.to_bigint(), y.to_bigint());

            // linear2 across all four sign quadrants, including max-magnitude coefficients.
            for (w1, w2) in [
                (rng.next() as i64 as i128, rng.next() as i64 as i128),
                (i128::from(i64::MAX), i128::from(i64::MAX)),
                (i128::from(i64::MAX), -i128::from(i64::MAX)),
                (-1, 1),
            ] {
                let got = x.linear2(w1, &y, w2).to_bigint();
                let want = &xb * w1 + &yb * w2;
                assert_eq!(
                    got, want,
                    "linear2 diverged: x={xd:x?} y={yd:x?} w1={w1} w2={w2}"
                );
                cases += 1;
            }

            let got = x.mul(&y).to_bigint();
            assert_eq!(got, &xb * &yb, "mul diverged: x={xd:x?} y={yd:x?}");
            cases += 1;
        }
        eprintln!("  linear2/mul vs bigint: {cases} cases agreed");
    }

    // Row-level kernel differential: the assembly against the portable loop it replaces, on
    // the exact accumulator discipline (65-bit add carry; P/Q/borrow chains; addmul_1 carry).
    #[cfg(target_arch = "aarch64")]
    #[test]
    fn kernels_match_portable_rows() {
        let mut rng = Rng(0x0123_4567_89AB_CDEF);
        let shapes: Vec<Vec<u64>> = vec![
            vec![u64::MAX; 12],
            vec![0; 12],
            vec![u64::MAX, 0, u64::MAX, 0, u64::MAX, 0, u64::MAX, 0],
            (0..12).map(|i| 1u64 << (63 - i * 5)).collect(),
        ];
        let coeffs = [0u64, 1, 2, u64::MAX, u64::MAX - 1, 1 << 63];
        let mut cases = 0u64;
        let mut check = |xd: &[u64], yd: &[u64], a1: u64, a2: u64| {
            let n = xd.len().min(yd.len()).min(30);
            let mut x = [0u64; 34];
            let mut y = [0u64; 34];
            x[..xd.len().min(34)].copy_from_slice(&xd[..xd.len().min(34)]);
            y[..yd.len().min(34)].copy_from_slice(&yd[..yd.len().min(34)]);
            let mut out_k = [0u64; 34];
            let mut out_p = [0u64; 34];
            let k = addmul2_kernel(&x, &y, n, a1, a2, &mut out_k);
            let p = Sw::<34>::addmul2_portable(&x, &y, n, a1, a2, &mut out_p);
            assert_eq!(
                (k, out_k),
                (p, out_p),
                "addmul2 diverged n={n} a1={a1:x} a2={a2:x}"
            );
            let mut out_k = [0u64; 34];
            let mut out_p = [0u64; 34];
            let k = submul2_kernel(&x, &y, n, a1, a2, &mut out_k);
            let p = Sw::<34>::submul2_portable(&x, &y, n, a1, a2, &mut out_p);
            assert_eq!(
                (k, out_k),
                (p, out_p),
                "submul2 diverged n={n} a1={a1:x} a2={a2:x}"
            );
            let mut out_k = [7u64; 34];
            let mut out_p = [7u64; 34];
            let k = addmul1_kernel(&y, n, a1, &mut out_k[..]);
            let p = Sw::<34>::addmul1_portable(&y, n, a1, &mut out_p[..]);
            assert_eq!((k, out_k), (p, out_p), "addmul1 diverged n={n} a={a1:x}");
            cases += 3;
        };
        for xs in &shapes {
            for ys in &shapes {
                for &a1 in &coeffs {
                    for &a2 in &coeffs {
                        check(xs, ys, a1, a2);
                    }
                }
            }
        }
        for _ in 0..4_000 {
            let n = 1 + (rng.next() as usize) % 20;
            let xd: Vec<u64> = (0..n).map(|_| rng.next()).collect();
            let yd: Vec<u64> = (0..n).map(|_| rng.next()).collect();
            check(&xd, &yd, rng.next(), rng.next());
        }
        eprintln!("  kernel-vs-portable rows: {cases} cases agreed");
    }

    // The quantifier: portable vs kernel per-row wall time in the same binary. This is how
    // the kernels prove their worth on-target in seconds — no chain sync involved.
    #[cfg(target_arch = "aarch64")]
    #[test]
    #[ignore = "manual kernel quantification"]
    fn kernel_bench_rows() {
        use std::time::Instant;
        let mut rng = Rng(0x5EED_5EED_5EED_5EED);
        // The Lehmer walk's operand distribution: most rows are 4-16 limbs.
        for n in [4usize, 8, 16, 30] {
            let x: [u64; 34] = core::array::from_fn(|_| rng.next());
            let y: [u64; 34] = core::array::from_fn(|_| rng.next());
            let (a1, a2) = (rng.next() | 1, rng.next() | 1);
            let iters = 2_000_000u64 / n as u64;
            let mut sink = 0u64;
            let t = Instant::now();
            for _ in 0..iters {
                let mut out = [0u64; 34];
                let (lo, hi) = Sw::<34>::addmul2_portable(&x, &y, n, a1, a2, &mut out);
                sink = sink.wrapping_add(lo ^ hi ^ out[n / 2]);
            }
            let portable = t.elapsed();
            let t = Instant::now();
            for _ in 0..iters {
                let mut out = [0u64; 34];
                let (lo, hi) = addmul2_kernel(&x, &y, n, a1, a2, &mut out);
                sink = sink.wrapping_add(lo ^ hi ^ out[n / 2]);
            }
            let kernel = t.elapsed();
            eprintln!(
                "  addmul2 n={n:>2}: portable {:>6.1?}ns/row, kernel {:>6.1?}ns/row, kernel/portable = {:.2}x  (sink {sink})",
                portable.as_nanos() as f64 / iters as f64,
                kernel.as_nanos() as f64 / iters as f64,
                kernel.as_secs_f64() / portable.as_secs_f64(),
            );
        }
    }
}
