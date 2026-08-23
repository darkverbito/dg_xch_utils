// Clippy: the type-annotation Integer::from wrappers are deliberate (rug operator output types).
#![allow(clippy::useless_conversion)]
//! GMP-backed class-group form arithmetic (rug/mpz), the candidate replacement for the
//! fixed-limb `SwWide`/`SwGcd` NUDUPL in `form.rs`. Same algorithm, GMP's mpn kernels underneath.
//! Production paths stay on `form.rs` until the A/B bench (`square_bench::gmp_vs_limbs`) proves
//! this faster AND the corpus replays gate a switch.

use crate::error::{Error, Result};
use crate::form::Form;
use num_bigint::{BigInt, Sign};
use rug::Integer;
use rug::ops::{DivRounding, RemRounding, RemRoundingAssign};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GForm {
    pub a: Integer,
    pub b: Integer,
    pub c: Integer,
}

pub fn to_rug(v: &BigInt) -> Integer {
    let (sign, bytes) = v.to_bytes_be();
    let mut g = Integer::from_digits(&bytes, rug::integer::Order::MsfBe);
    if sign == Sign::Minus {
        g = -g;
    }
    g
}

pub fn to_bigint(v: &Integer) -> BigInt {
    let bytes = v.to_digits::<u8>(rug::integer::Order::MsfBe);
    let mag = BigInt::from_bytes_be(Sign::Plus, &bytes);
    if v.is_negative() { -mag } else { mag }
}

impl GForm {
    #[must_use]
    pub fn from_form(f: &Form) -> Self {
        Self {
            a: to_rug(&f.a),
            b: to_rug(&f.b),
            c: to_rug(&f.c),
        }
    }

    #[must_use]
    pub fn to_form(&self) -> Form {
        Form {
            a: to_bigint(&self.a),
            b: to_bigint(&self.b),
            c: to_bigint(&self.c),
        }
    }

    // Mirror of form.rs normalize: bring b into (-a, a].
    fn normalize(&mut self) {
        if self.b > self.a || self.b <= -self.a.clone() {
            // r = floor((a - b) / 2a)
            let two_a = Integer::from(&self.a << 1);
            let r = Integer::from(&self.a - &self.b).div_floor(two_a.clone());
            // b' = b + 2ra ; c' = a r^2 + b r + c
            let ra = Integer::from(&r * &self.a);
            self.c += Integer::from(&ra * &r) + Integer::from(&self.b * &r);
            self.b += Integer::from(&ra << 1);
        }
    }

    // Mirror of form.rs reduce_impl loop.
    pub fn reduce(&mut self) {
        self.normalize();
        while self.a > self.c || (self.a == self.c && self.b.is_negative()) {
            // s = floor((c + b) / 2c)
            let two_c = Integer::from(&self.c << 1);
            let s = Integer::from(&self.c + &self.b).div_floor(two_c.clone());
            // (a, b, c) <- (c, -b + 2sc, c s^2 - b s + a)
            let sc = Integer::from(&s * &self.c);
            let new_a = self.c.clone();
            let new_c = Integer::from(&sc * &s) - Integer::from(&self.b * &s) + &self.a;
            self.b = Integer::from(&sc << 1) - &self.b;
            self.a = new_a;
            self.c = new_c;
        }
        self.normalize();
    }

    /// NUDUPL squaring (chiavdf nudupl over GMP): identical structure to `Form::square_with`.
    ///
    /// # Errors
    /// Returns [`Error::InvalidForm`] if the form's invariants do not hold (non-invertible b mod a).
    pub fn square_with(&self, l: &Integer) -> Result<Self> {
        // (g, v) with g = gcd(b, a), v*b ≡ g (mod a).
        let (g, v, _) = Integer::from(&self.b)
            .extended_gcd(Integer::from(&self.a), Integer::new())
            .into();
        let g: Integer = g;
        let v: Integer = v;

        let mut a1 = self.a.clone();
        let mut c1 = self.c.clone();
        if g != 1u32 {
            a1 = Integer::from(&a1 / &g);
            c1 *= &g;
        }
        // k = -v*c mod a1
        let mut k = Integer::from(-Integer::from(&v * &c1));
        k = k.rem_floor(a1.clone());

        let (ca, cb, cc);
        if a1 < *l {
            let t = Integer::from(&a1 * &k);
            ca = Integer::from(&a1 * &a1);
            cb = Integer::from(&t << 1) + &self.b;
            let num = Integer::from(&self.b + &t) * &k + &c1;
            cc = Integer::from(&num / &a1);
        } else {
            // Partial euclidean reduction of (a1, k) down to ~|D|^(1/4), cofactors tracked.
            let (r2, r1, co2, co1) = xgcd_partial(a1.clone(), k.clone(), l);
            let _ = r2;
            let t = Integer::from(&a1 * &r1);
            let m2 = (Integer::from(&self.b * &r1) - Integer::from(&c1 * &co1)) / &a1;
            let r1r1 = Integer::from(&r1 * &r1);
            let co1m2 = Integer::from(&co1 * &m2);
            let mut ca_ = if co1.is_negative() {
                r1r1 - co1m2
            } else {
                co1m2 - r1r1
            };
            let b_num: Integer =
                Integer::from((Integer::from(&t - Integer::from(&ca_ * &co2)) << 1) / &co1)
                    - &self.b;
            let cb_: Integer = b_num.rem_floor(Integer::from(&ca_ << 1));
            let d = Integer::from(&self.b * &self.b) - (Integer::from(&self.a * &self.c) << 2);
            let mut cc_: Integer =
                Integer::from(Integer::from(Integer::from(&cb_ * &cb_) - d) / &ca_) >> 2;
            if ca_.is_negative() {
                ca_ = -ca_;
                cc_ = -cc_;
            }
            ca = ca_;
            cb = cb_;
            cc = cc_;
        }
        if ca == 0u32 {
            return Err(Error::InvalidForm);
        }
        let mut out = Self {
            a: ca,
            b: cb,
            c: cc,
        };
        out.reduce();
        Ok(out)
    }
}

// Partial extended gcd (chiavdf xgcd_partial): reduce (r2, r1) by euclidean steps until |r1| < L,
// tracking cofactors (co2, co1) with r_i = co_i' * a + ... (the NUDUPL cofactor convention:
// co2, co1 start at -1? chiavdf starts co2 = 0, co1 = -1 with sign-folding).
fn xgcd_partial(
    mut r2: Integer,
    mut r1: Integer,
    l: &Integer,
) -> (Integer, Integer, Integer, Integer) {
    let mut co2 = Integer::new();
    let mut co1 = Integer::from(-1);
    while r1 != 0u32 && r1.clone().abs() >= *l {
        let q = Integer::from(&r2 / &r1);
        let r = Integer::from(&r2 - Integer::from(&q * &r1));
        r2 = std::mem::replace(&mut r1, r);
        let c = Integer::from(&co2 - Integer::from(&q * &co1));
        co2 = std::mem::replace(&mut co1, c);
    }
    (r2, r1, co2, co1)
}

/// Preallocated scratch for the allocation-free NUDUPL path — the chiavdf pattern: every
/// intermediate lives in a reused mpz; the hot loop performs no heap allocation once capacities
/// warm up (rug's `assign` on an existing Integer reuses its limbs).
pub struct GScratch {
    g: Integer,
    v: Integer,
    a1: Integer,
    c1: Integer,
    k: Integer,
    t: Integer,
    m2: Integer,
    r2: Integer,
    r1: Integer,
    co2: Integer,
    co1: Integer,
    q: Integer,
    tmp: Integer,
    tmp2: Integer,
    ca: Integer,
    cb: Integer,
    cc: Integer,
    d: Integer,
}

impl Default for GScratch {
    fn default() -> Self {
        let z = || Integer::with_capacity(2048);
        Self {
            g: z(),
            v: z(),
            a1: z(),
            c1: z(),
            k: z(),
            t: z(),
            m2: z(),
            r2: z(),
            r1: z(),
            co2: z(),
            co1: z(),
            q: z(),
            tmp: z(),
            tmp2: z(),
            ca: z(),
            cb: z(),
            cc: z(),
            d: z(),
        }
    }
}

impl GForm {
    /// NUDUPL squaring with preallocated scratch — semantics identical to
    /// [`GForm::square_with`], allocation-free steady state. `self` is updated in place.
    ///
    /// # Errors
    /// Returns [`Error::InvalidForm`] on a degenerate form.
    pub fn square_with_scratch(&mut self, l: &Integer, s: &mut GScratch) -> Result<()> {
        use rug::Assign;
        // (g, v): g = gcd(b, a), v*b ≡ g (mod a)
        // extended_gcd_mut: self<-gcd, other<-cofactor of OLD self, rop<-cofactor of old other.
        s.g.assign(&self.b);
        s.tmp.assign(&self.a);
        s.v.assign(0);
        s.g.extended_gcd_mut(&mut s.tmp, &mut s.v);
        std::mem::swap(&mut s.v, &mut s.tmp); // v = cofactor of b

        s.a1.assign(&self.a);
        s.c1.assign(&self.c);
        if s.g != 1u32 {
            s.a1.div_exact_mut(&s.g);
            s.c1 *= &s.g;
        }
        // k = (-v*c1) mod a1
        s.k.assign(&s.v * &s.c1);
        s.k = -s.k.clone();
        s.k.rem_floor_assign(&s.a1);

        if s.a1 < *l {
            s.t.assign(&s.a1 * &s.k);
            s.ca.assign(&s.a1 * &s.a1);
            s.cb.assign(&s.t);
            s.cb <<= 1;
            s.cb += &self.b;
            s.tmp.assign(&self.b);
            s.tmp += &s.t;
            s.tmp *= &s.k;
            s.tmp += &s.c1;
            s.tmp.div_exact_mut(&s.a1);
            s.cc.assign(&s.tmp);
        } else {
            // Partial euclidean reduction with in-place cofactors.
            s.r2.assign(&s.a1);
            s.r1.assign(&s.k);
            s.co2.assign(0);
            s.co1.assign(-1);
            while s.r1 != 0u32 && {
                s.tmp.assign(&s.r1);
                s.tmp.abs_mut();
                s.tmp >= *l
            } {
                s.q.assign(&s.r2 / &s.r1);
                s.tmp.assign(&s.q * &s.r1);
                s.r2 -= &s.tmp;
                std::mem::swap(&mut s.r2, &mut s.r1);
                s.tmp.assign(&s.q * &s.co1);
                s.co2 -= &s.tmp;
                std::mem::swap(&mut s.co2, &mut s.co1);
            }
            s.t.assign(&s.a1 * &s.r1);
            s.m2.assign(&self.b * &s.r1);
            s.tmp.assign(&s.c1 * &s.co1);
            s.m2 -= &s.tmp;
            s.m2.div_exact_mut(&s.a1);
            s.ca.assign(&s.co1 * &s.m2);
            s.tmp.assign(&s.r1 * &s.r1);
            if s.co1.is_negative() {
                // ca = r1*r1 - co1*m2
                s.tmp2.assign(&s.ca);
                s.ca.assign(&s.tmp);
                s.ca -= &s.tmp2;
            } else {
                s.ca -= &s.tmp;
            }
            // cb = ((t - ca*co2) << 1) / co1 - b, then mod 2ca
            s.cb.assign(&s.ca * &s.co2);
            s.tmp.assign(&s.t);
            s.tmp -= &s.cb;
            s.tmp <<= 1;
            s.tmp.div_exact_mut(&s.co1);
            s.tmp -= &self.b;
            s.tmp2.assign(&s.ca);
            s.tmp2 <<= 1;
            s.tmp.rem_floor_assign(&s.tmp2);
            s.cb.assign(&s.tmp);
            // cc = ((cb^2 - D) / ca) >> 2
            s.d.assign(&self.b * &self.b);
            s.tmp.assign(&self.a * &self.c);
            s.tmp <<= 2;
            s.d -= &s.tmp;
            s.cc.assign(&s.cb * &s.cb);
            s.cc -= &s.d;
            s.cc.div_exact_mut(&s.ca);
            s.cc >>= 2;
            if s.ca.is_negative() {
                s.ca = -s.ca.clone();
                s.cc = -s.cc.clone();
            }
        }
        if s.ca == 0u32 {
            return Err(Error::InvalidForm);
        }
        std::mem::swap(&mut self.a, &mut s.ca);
        std::mem::swap(&mut self.b, &mut s.cb);
        std::mem::swap(&mut self.c, &mut s.cc);
        self.reduce();
        Ok(())
    }
}
