use crate::stats::{Domain, derive_rng};
use rand_chacha::rand_core::Rng;

/// Block timestamps for a simulated chain.
///
/// Elapsed time accumulates as `f64` and is quantized to whole seconds only in [`Self::emit`],
/// which carries the sub-second remainder forward so a long run does not drift by the rounding
/// error of every block it produced. Emissions are anchored to the previously emitted timestamp
/// and are strictly increasing, which is what the consensus rules require of transaction blocks.
#[derive(Debug, Clone, PartialEq)]
pub struct TimestampEmitter {
    anchor: u64,
    offset: f64,
}

impl TimestampEmitter {
    #[must_use]
    pub fn new(genesis_timestamp: u64) -> Self {
        Self {
            anchor: genesis_timestamp,
            offset: 0.0,
        }
    }

    /// Accumulate `seconds` of chain time without emitting.
    pub fn advance(&mut self, seconds: f64) {
        self.offset += seconds;
    }

    /// The last emitted timestamp.
    #[must_use]
    pub fn anchor(&self) -> u64 {
        self.anchor
    }

    /// Emit the next timestamp, re-anchoring to it. A step of zero whole seconds is emitted as one
    /// second and borrowed from the accumulator, so the debt is repaid by a later block rather than
    /// silently inflating the chain's clock.
    pub fn emit(&mut self) -> u64 {
        let whole = self.offset.floor();
        let step = if whole >= 1.0 { whole as u64 } else { 1 };
        self.offset -= step as f64;
        self.anchor += step;
        self.anchor
    }
}

/// The seed for the `fork_ordinal`-th reorg of a run. Always distinct from `run_seed`: a fork
/// seeded identically to the mainline replays it block for block and never diverges.
#[must_use]
pub fn reorg_seed(run_seed: u64, fork_ordinal: u64) -> u64 {
    let mut rng = derive_rng(run_seed, Domain::Reorg, fork_ordinal);
    let mut seed = rng.next_u64();
    while seed == run_seed {
        seed = rng.next_u64();
    }
    seed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emitted_timestamps_are_strictly_increasing() {
        let mut e = TimestampEmitter::new(1_000);
        let mut last = 1_000;
        for _ in 0..100 {
            e.advance(0.01);
            let ts = e.emit();
            assert!(ts > last, "{ts} !> {last}");
            last = ts;
        }
    }

    #[test]
    fn quantizing_only_at_emission_avoids_drift() {
        let mut e = TimestampEmitter::new(0);
        for _ in 0..1_000 {
            e.advance(18.6);
            e.emit();
        }
        // Rounding at every block would lose up to 0.6s each, roughly 600s over this run.
        assert_eq!(e.anchor(), 18_600);
    }

    #[test]
    fn many_small_advances_match_one_large_advance() {
        let mut split = TimestampEmitter::new(500);
        for _ in 0..64 {
            split.advance(0.25);
        }
        let mut whole = TimestampEmitter::new(500);
        whole.advance(16.0);
        assert_eq!(split.emit(), whole.emit());
    }

    #[test]
    fn a_borrowed_second_is_repaid() {
        let mut e = TimestampEmitter::new(0);
        // Four blocks arrive inside one second, then the chain idles.
        for _ in 0..4 {
            e.advance(0.25);
            e.emit();
        }
        assert_eq!(e.anchor(), 4);
        e.advance(4.0);
        assert_eq!(e.emit(), 5);
    }

    #[test]
    fn a_reorg_seed_never_equals_the_mainline_seed() {
        for run_seed in 0..256u64 {
            for fork_ordinal in 0..8u64 {
                assert_ne!(reorg_seed(run_seed, fork_ordinal), run_seed);
            }
        }
    }

    #[test]
    fn reorg_seeds_are_deterministic_and_distinct_per_fork() {
        assert_eq!(reorg_seed(7, 0), reorg_seed(7, 0));
        assert_ne!(reorg_seed(7, 0), reorg_seed(7, 1));
        assert_ne!(reorg_seed(7, 0), reorg_seed(8, 0));
    }
}
