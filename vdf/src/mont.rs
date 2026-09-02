/// One Montgomery context per candidate modulus. `N` limbs, little-endian.
pub(crate) struct Mont<const N: usize> {
    n: [u64; N],
    /// `-n^{-1} mod 2^64`.
    n0inv: u64,
    /// `R^2 mod n` with `R = 2^(64N)`, for converting into Montgomery form.
    rr: [u64; N],
}

#[inline]
fn addcarry(a: u64, b: u64, carry: u64) -> (u64, u64) {
    let s = u128::from(a) + u128::from(b) + u128::from(carry);
    (s as u64, (s >> 64) as u64)
}

#[inline]
fn subborrow(a: u64, b: u64, borrow: u64) -> (u64, u64) {
    let d = u128::from(a)
        .wrapping_sub(u128::from(b))
        .wrapping_sub(u128::from(borrow));
    (d as u64, (d >> 127) as u64)
}

fn geq<const N: usize>(a: &[u64; N], b: &[u64; N]) -> bool {
    for i in (0..N).rev() {
        if a[i] != b[i] {
            return a[i] > b[i];
        }
    }
    true
}

fn sub_n<const N: usize>(a: &mut [u64; N], b: &[u64; N]) {
    let mut borrow = 0u64;
    for i in 0..N {
        let (d, br) = subborrow(a[i], b[i], borrow);
        a[i] = d;
        borrow = br;
    }
}

impl<const N: usize> Mont<N> {
    /// `n` odd, high limb non-zero fitting `N` limbs.
    pub(crate) fn new(n: [u64; N]) -> Self {
        debug_assert!(n[0] & 1 == 1, "Montgomery needs an odd modulus");
        // Newton: x_{k+1} = x_k (2 - n0 x_k) doubles correct low bits each step.
        let n0 = n[0];
        let mut inv: u64 = 1;
        for _ in 0..6 {
            inv = inv.wrapping_mul(2u64.wrapping_sub(n0.wrapping_mul(inv)));
        }
        let n0inv = inv.wrapping_neg();
        // R^2 mod n by 2·64·N doublings of R mod n's seed (start from 1, shift 128N times).
        let mut rr = [0u64; N];
        rr[0] = 1;
        // reduce 1 into range (already), then double 128N times mod n
        for _ in 0..(128 * N) {
            // rr <<= 1 (with carry-out), conditional subtract
            let mut carry = 0u64;
            for limb in rr.iter_mut() {
                let new_carry = *limb >> 63;
                *limb = (*limb << 1) | carry;
                carry = new_carry;
            }
            if carry == 1 || geq(&rr, &n) {
                sub_n(&mut rr, &n);
            }
        }
        Self { n, n0inv, rr }
    }

    /// CIOS multiply: returns `a·b·R^{-1} mod n`.
    fn mul(&self, a: &[u64; N], b: &[u64; N]) -> [u64; N] {
        let mut t = [0u64; N];
        let mut t_extra: u64 = 0; // limb N of the running accumulator
        for &bi in b.iter() {
            // t += a * bi
            let mut carry = 0u64;
            for j in 0..N {
                let prod = u128::from(a[j]) * u128::from(bi) + u128::from(t[j]) + u128::from(carry);
                t[j] = prod as u64;
                carry = (prod >> 64) as u64;
            }
            let (te, ce) = addcarry(t_extra, carry, 0);
            t_extra = te;
            let mut carry2 = ce; // can only be 0 or 1, folded below

            // m = t0 · n0inv; t += m · n; t >>= 64
            let m = t[0].wrapping_mul(self.n0inv);
            let prod = u128::from(m) * u128::from(self.n[0]) + u128::from(t[0]);
            let mut carry3 = (prod >> 64) as u64;
            for j in 1..N {
                let prod =
                    u128::from(m) * u128::from(self.n[j]) + u128::from(t[j]) + u128::from(carry3);
                t[j - 1] = prod as u64;
                carry3 = (prod >> 64) as u64;
            }
            let (last, c4) = addcarry(t_extra, carry3, 0);
            t[N - 1] = last;
            t_extra = carry2 + c4;
            carry2 = 0;
            let _ = carry2;
        }
        if t_extra != 0 || geq(&t, &self.n) {
            sub_n(&mut t, &self.n);
        }
        t
    }

    fn to_mont(&self, a: &[u64; N]) -> [u64; N] {
        self.mul(a, &self.rr)
    }

    fn out_of_mont(&self, a: &[u64; N]) -> [u64; N] {
        let mut one = [0u64; N];
        one[0] = 1;
        self.mul(a, &one)
    }

    /// `2^e mod n` for a multi-limb exponent (little-endian limbs, `e_bits` significant bits),
    /// square-and-multiply MSB-first.
    pub(crate) fn pow2(&self, e: &[u64], e_bits: u64) -> [u64; N] {
        let mut two = [0u64; N];
        two[0] = 2;
        let two_m = self.to_mont(&two);
        let mut one = [0u64; N];
        one[0] = 1;
        let mut acc = self.to_mont(&one);
        for i in (0..e_bits).rev() {
            acc = self.mul(&acc, &acc);
            if (e[(i / 64) as usize] >> (i % 64)) & 1 == 1 {
                acc = self.mul(&acc, &two_m);
            }
        }
        self.out_of_mont(&acc)
    }
}

/// Strong Miller–Rabin to base 2 on fixed limbs. `None` when the candidate does not fit `N`
/// limbs — the caller falls back to the bigint implementation of the same test.
pub(crate) fn miller_rabin_base2_fixed<const N: usize>(n: &num_bigint::BigUint) -> Option<bool> {
    if n.bits() > (64 * N) as u64 || *n < num_bigint::BigUint::from(3u8) {
        return None;
    }
    let mut limbs = [0u64; N];
    for (i, d) in n.iter_u64_digits().enumerate() {
        limbs[i] = d;
    }
    if limbs[0] & 1 == 0 {
        return Some(false);
    }
    let ctx = Mont::<N>::new(limbs);

    // n - 1 = d · 2^s
    let mut d = limbs;
    d[0] -= 1; // n odd, no borrow
    let mut s = 0u32;
    while !d.iter().all(|&l| l == 0) && d[0] & 1 == 0 {
        // d >>= 1
        let mut carry = 0u64;
        for i in (0..N).rev() {
            let new_carry = d[i] & 1;
            d[i] = (d[i] >> 1) | (carry << 63);
            carry = new_carry;
        }
        s += 1;
    }
    let d_bits = {
        let mut bits = 0u64;
        for i in (0..N).rev() {
            if d[i] != 0 {
                bits = (i as u64) * 64 + (64 - d[i].leading_zeros() as u64);
                break;
            }
        }
        bits
    };

    let mut x = ctx.pow2(&d, d_bits);
    // n - 1 as limbs
    let mut n_minus_one = limbs;
    n_minus_one[0] -= 1;
    let one_is = |v: &[u64; N]| v[0] == 1 && v[1..].iter().all(|&l| l == 0);
    if one_is(&x) || x == n_minus_one {
        return Some(true);
    }
    for _ in 1..s {
        let xm = ctx.to_mont(&x);
        let sq = ctx.mul(&xm, &xm);
        x = ctx.out_of_mont(&sq);
        if x == n_minus_one {
            return Some(true);
        }
    }
    Some(false)
}
