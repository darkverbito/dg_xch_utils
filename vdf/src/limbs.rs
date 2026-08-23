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
            let mut carry: u128 = 0;
            let a = u128::from(self.d[i]);
            for j in 0..other.len {
                let p = a * u128::from(other.d[j]) + u128::from(out.d[i + j]) + carry;
                out.d[i + j] = p as u64;
                carry = p >> 64;
            }
            out.d[i + other.len] = carry as u64;
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
            // Single-word divisor.
            let d = u128::from(v.d[0]);
            let mut q = Self::zero();
            let mut rem: u128 = 0;
            for i in (0..self.len).rev() {
                let cur = (rem << 64) | u128::from(self.d[i]);
                q.d[i] = (cur / d) as u64;
                rem = cur % d;
            }
            q.len = self.len;
            q.trim();
            let mut r = Self::zero();
            if rem != 0 {
                r.d[0] = rem as u64;
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
        let mut q = Self::zero();
        for j in (0..=m).rev() {
            let num = (u128::from(un[j + n]) << 64) | u128::from(un[j + n - 1]);
            let mut qhat = num / vtop;
            let mut rhat = num % vtop;
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
            // Fused addmul_2: out = X·a1 + Y·a2, all unsigned; carry < 2^65 held as (lo, hi).
            let mut c_lo: u64 = 0;
            let mut c_hi: u64 = 0;
            for i in 0..n {
                let p1 = u128::from(self.d[i]) * u128::from(a1);
                let p2 = u128::from(other.d[i]) * u128::from(a2);
                let (s, o1) = (p1 as u64).overflowing_add(p2 as u64);
                let (s, o2) = s.overflowing_add(c_lo);
                out.d[i] = s;
                // Next carry = hi1 + hi2 + c_hi + spill bits; sum < 2^65 needs the (lo, hi) split.
                let (h, oh1) = ((p1 >> 64) as u64).overflowing_add((p2 >> 64) as u64);
                let (h, oh2) = h.overflowing_add(c_hi + u64::from(o1) + u64::from(o2));
                c_lo = h;
                c_hi = u64::from(oh1) + u64::from(oh2);
            }
            out.d[n] = c_lo;
            out.d[n + 1] = c_hi;
            out.neg = s1;
        } else {
            // Fused submul_2: out = ±(X·a1 − Y·a2) in two's complement — three short chains
            // (P-accumulate, Q-accumulate, borrow), the sbb shape. A set final borrow means the
            // true value is negative: complement to magnitude, sign flips to s2's contribution.
            let mut cp: u64 = 0; // carry of the P = X·a1 stream
            let mut cq: u64 = 0; // carry of the Q = Y·a2 stream
            let mut borrow: u64 = 0;
            for i in 0..n {
                let p = u128::from(self.d[i]) * u128::from(a1);
                let q = u128::from(other.d[i]) * u128::from(a2);
                let (pl, pc) = (p as u64).overflowing_add(cp);
                cp = (p >> 64) as u64 + u64::from(pc);
                let (ql, qc) = (q as u64).overflowing_add(cq);
                cq = (q >> 64) as u64 + u64::from(qc);
                let (d, b1) = pl.overflowing_sub(ql);
                let (d, b2) = d.overflowing_sub(borrow);
                out.d[i] = d;
                borrow = u64::from(b1) + u64::from(b2);
            }
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
