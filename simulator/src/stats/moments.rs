//! Online moments and the reduction used to combine them.
//!
//! Floating point addition is not associative, so an aggregate that folds results in whatever order
//! workers happen to finish is not reproducible. Leaves are therefore combined by [`tree_reduce`] in
//! index order with a shape fixed by the leaf count alone, which is what makes a campaign's answer
//! independent of how many workers ran it.

/// Mean and variance accumulated in one pass, after Welford. `merge` is the pairwise form from
/// Chan, Golub, and LeVeque.
#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct Welford {
    n: u64,
    mean: f64,
    m2: f64,
}

impl Welford {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A single-sample accumulator: one run's contribution, before any combining.
    #[must_use]
    pub fn of(x: f64) -> Self {
        Self {
            n: 1,
            mean: x,
            m2: 0.0,
        }
    }

    pub fn push(&mut self, x: f64) {
        self.n += 1;
        let delta = x - self.mean;
        self.mean += delta / self.n as f64;
        self.m2 += delta * (x - self.mean);
    }

    #[must_use]
    pub fn merge(a: &Self, b: &Self) -> Self {
        if a.n == 0 {
            return *b;
        }
        if b.n == 0 {
            return *a;
        }
        let n = a.n + b.n;
        let delta = b.mean - a.mean;
        let na = a.n as f64;
        let nb = b.n as f64;
        Self {
            n,
            mean: a.mean + delta * (nb / n as f64),
            m2: a.m2 + b.m2 + delta * delta * (na * nb / n as f64),
        }
    }

    #[must_use]
    pub fn n(&self) -> u64 {
        self.n
    }

    #[must_use]
    pub fn mean(&self) -> f64 {
        self.mean
    }

    /// Sample variance, `None` for fewer than two samples.
    #[must_use]
    pub fn variance(&self) -> Option<f64> {
        (self.n >= 2).then(|| self.m2 / (self.n - 1) as f64)
    }

    #[must_use]
    pub fn std_dev(&self) -> Option<f64> {
        self.variance().map(f64::sqrt)
    }

    /// Standard error of the mean.
    #[must_use]
    pub fn std_error(&self) -> Option<f64> {
        self.std_dev().map(|s| s / (self.n as f64).sqrt())
    }
}

/// Combine leaves pairwise in index order. The recursion splits at the midpoint, so the shape
/// depends only on `leaves.len()` and the result is bit-identical across runs and worker counts.
///
/// Leaves must be per-run, not per-worker: pre-merging a worker's runs into one leaf makes the
/// shape depend on the work distribution and reintroduces the drift this avoids.
#[must_use]
pub fn tree_reduce(leaves: &[Welford]) -> Welford {
    match leaves.len() {
        0 => Welford::new(),
        1 => leaves[0],
        n => {
            let (left, right) = leaves.split_at(n / 2);
            Welford::merge(&tree_reduce(left), &tree_reduce(right))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn naive(xs: &[f64]) -> (f64, f64) {
        let n = xs.len() as f64;
        let mean = xs.iter().sum::<f64>() / n;
        let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0);
        (mean, var)
    }

    fn samples() -> Vec<f64> {
        (0..1_000).map(|i| 18.75 + (i % 37) as f64 * 0.5).collect()
    }

    #[test]
    fn moments_match_a_two_pass_computation() {
        let xs = samples();
        let mut w = Welford::new();
        for x in &xs {
            w.push(*x);
        }
        let (mean, var) = naive(&xs);
        assert_eq!(w.n(), xs.len() as u64);
        assert!((w.mean() - mean).abs() < 1e-9, "{} vs {mean}", w.mean());
        let got = w.variance().expect("more than one sample");
        assert!((got - var).abs() < 1e-9, "{got} vs {var}");
    }

    #[test]
    fn merging_halves_matches_pushing_the_whole() {
        let xs = samples();
        let (left, right) = xs.split_at(xs.len() / 2);
        let fold = |chunk: &[f64]| {
            let mut w = Welford::new();
            for x in chunk {
                w.push(*x);
            }
            w
        };
        let merged = Welford::merge(&fold(left), &fold(right));
        let whole = fold(&xs);
        assert_eq!(merged.n(), whole.n());
        assert!((merged.mean() - whole.mean()).abs() < 1e-9);
        let (a, b) = (
            merged.variance().expect("n >= 2"),
            whole.variance().expect("n >= 2"),
        );
        assert!((a - b).abs() < 1e-9, "{a} vs {b}");
    }

    #[test]
    fn the_reduction_is_bit_identical_for_the_same_leaves() {
        let leaves: Vec<Welford> = samples().into_iter().map(Welford::of).collect();
        let a = tree_reduce(&leaves);
        let b = tree_reduce(&leaves);
        assert_eq!(a, b);
        assert_eq!(a.mean().to_bits(), b.mean().to_bits());
    }

    #[test]
    fn the_reduction_does_not_depend_on_worker_count() {
        // Workers claim runs round robin and fill their results back by run index. Whatever the
        // worker count, the leaf slice is the same and so is every bit of the answer.
        let xs = samples();
        let reference = tree_reduce(&xs.iter().copied().map(Welford::of).collect::<Vec<_>>());
        for workers in [1usize, 2, 3, 7, 16] {
            let mut leaves = vec![Welford::new(); xs.len()];
            for worker in 0..workers {
                for (i, x) in xs.iter().enumerate() {
                    if i % workers == worker {
                        leaves[i] = Welford::of(*x);
                    }
                }
            }
            let got = tree_reduce(&leaves);
            assert_eq!(
                got.mean().to_bits(),
                reference.mean().to_bits(),
                "{workers} workers drifted"
            );
            assert_eq!(got, reference, "{workers} workers drifted");
        }
    }

    #[test]
    fn variance_and_error_need_two_samples() {
        let mut w = Welford::new();
        assert_eq!(w.n(), 0);
        assert!(w.variance().is_none());
        w.push(4.0);
        assert!(w.variance().is_none());
        assert!(w.std_error().is_none());
        w.push(6.0);
        assert_eq!(w.mean(), 5.0);
        assert_eq!(w.variance(), Some(2.0));
        assert_eq!(w.std_error(), Some(1.0));
    }

    #[test]
    fn empty_and_single_leaf_reductions_are_well_defined() {
        assert_eq!(tree_reduce(&[]).n(), 0);
        let one = Welford::of(3.5);
        assert_eq!(tree_reduce(&[one]), one);
    }
}
