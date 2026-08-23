//! # Derived coin-set roots, layout v1
//!
//! A deterministic commitment to the chain's coin set as of a height `H`: a Merkle Mountain
//! Range over every coin creation confirmed at or below `H`, in canonical confirmation order,
//! plus a spent-bitmap commitment over the same leaf sequence (the Grin TXO-MMR pattern:
//! an append-only output MMR paired with a bitmap of which outputs are spent — see
//! grin `doc/mmr.md` and the Grin "bitmap accumulator" in `core/src/core/pmmr`), bound to the
//! height and the main-chain header hash at that height.
//!
//! This module is the NORMATIVE spec of layout v1. An independent implementation that follows
//! the rules below reproduces every root byte-for-byte. Nothing here is consensus: the roots
//! are DERIVED artifacts (SwiftSync trust shape — fail-closed, reproducible by anyone with a
//! synced node).
//!
//! ## Canonical leaf order (v1)
//!
//! Coin creations are appended in ascending `(confirmed_height, coin_id)` order, where
//! `coin_id` ordering is bytewise (unsigned lexicographic over the 32 bytes).
//!
//! Why not intra-block insertion order: none of the store backends preserves it. The SQL
//! schemas key `coin_record` by `coin_name` with no per-block ordinal (see
//! `stores/migrations/*/0001_coin_record.sql`), and the mmap backend's insertion-ordered coin
//! log is an implementation detail its table does not guarantee to expose. Bytewise `coin_id`
//! is the one tie-break every backend can serve identically (SQLite BLOB comparison is
//! memcmp; Postgres BYTEA comparison is bytewise), and any implementation can reproduce it
//! from block data alone by sorting each block's additions by coin id. The tie-break is
//! load-bearing: leaf hashes commit to the leaf index, so a permuted order changes the root
//! (there is a red test for exactly this).
//!
//! Duplicate `coin_id`s cannot occur: a coin id is `sha256(parent, puzzle_hash, amount)` and
//! the store upserts by it; the accumulator rejects an equal-or-descending append as
//! corruption.
//!
//! ## Hashes (all SHA-256, all domain-separated, version tag in the domain string)
//!
//! ```text
//! coin leaf   L_i   = H("dgxch.coinroot.v1.coin-leaf" || LE64(i) || coin_id[32]
//!                       || LE32(confirmed_height) || LE64(timestamp))
//! interior    N     = H("dgxch.coinroot.v1.node" || left[32] || right[32])
//! peak bag          = H("dgxch.coinroot.v1.bag" || peak[32] || acc[32])   (right to left)
//! coin MMR root     = H("dgxch.coinroot.v1.mmr-root" || LE64(n) || bagged[32])
//! bitmap leaf B_j   = H("dgxch.coinroot.v1.bitmap-leaf" || LE64(j) || chunk[128])
//! bitmap root       = H("dgxch.coinroot.v1.bitmap-root" || LE64(n) || bagged[32])
//! empty tree        = H("dgxch.coinroot.v1.empty")
//! combined root     = H("dgxch.coinroot.v1.root" || mmr_root[32] || bitmap_root[32]
//!                       || LE32(height) || header_hash[32])
//! ```
//!
//! - `i` is the 0-based leaf index in canonical order; `n` is the total leaf (coin) count.
//! - `timestamp` is the confirming transaction block's timestamp, exactly as the store
//!   records it per coin (chia `CoinRecord.timestamp` semantics) — carried in the leaf
//!   because SwiftSync-style spend validation of relative time/height conditions needs the
//!   creation metadata.
//! - The MMR is the standard binary-counter construction (FlyClient / Grin lineage): appends
//!   maintain one perfect-subtree peak per set bit of `n`; two equal-order peaks merge into an
//!   interior node. Peaks are bagged right-to-left: `acc` starts as the rightmost (smallest)
//!   peak and folds toward the leftmost.
//! - The spent bitmap has one bit per coin leaf, LSB-first within each byte (bit `i` is byte
//!   `i / 8`, mask `1 << (i % 8)`): bit set iff the coin is spent as of `H`, i.e.
//!   `0 < spent_index <= H`. The bitmap is split into 1024-bit (128-byte) chunks (Grin's
//!   bitmap-accumulator chunk size), the final chunk zero-padded; chunk hashes are the leaves
//!   of a second MMR with the same node/bag rules. `n = 0` commits zero chunks (empty tree).
//!
//! Layout evolution: any change to the byte layout, order, or domain strings is a new version
//! (`v2` domain strings + a new ledger file), never a mutation of v1.

pub mod derive;
pub mod validate;

use dg_xch_core::blockchain::sized_bytes::Bytes32;
use serde::Serialize;
use sha2::{Digest, Sha256};

const DOMAIN_COIN_LEAF: &[u8] = b"dgxch.coinroot.v1.coin-leaf";
const DOMAIN_NODE: &[u8] = b"dgxch.coinroot.v1.node";
const DOMAIN_BAG: &[u8] = b"dgxch.coinroot.v1.bag";
const DOMAIN_MMR_ROOT: &[u8] = b"dgxch.coinroot.v1.mmr-root";
const DOMAIN_BITMAP_LEAF: &[u8] = b"dgxch.coinroot.v1.bitmap-leaf";
const DOMAIN_BITMAP_ROOT: &[u8] = b"dgxch.coinroot.v1.bitmap-root";
const DOMAIN_EMPTY: &[u8] = b"dgxch.coinroot.v1.empty";
const DOMAIN_ROOT: &[u8] = b"dgxch.coinroot.v1.root";

/// Spent-bitmap chunk size in bits (Grin bitmap-accumulator chunking).
pub const BITMAP_CHUNK_BITS: usize = 1024;

#[derive(Debug)]
pub enum RootsError {
    /// An append that is not strictly ascending in `(confirmed_height, coin_id)` — either the
    /// backend returned rows out of canonical order or the same coin twice. Fail-closed: a
    /// root must never be emitted over a misordered stream.
    OutOfOrder {
        prev: (u32, Bytes32),
        next: (u32, Bytes32),
    },
    /// `spent_index` below `confirmed_height` (nonzero) — store corruption; a coin cannot be
    /// spent before it exists (same-block ephemeral spends are equal, which is allowed).
    SpentBeforeCreated {
        coin_id: Bytes32,
        confirmed: u32,
        spent: u32,
    },
    /// A root requested at a height below an already-appended coin — the driver fed the
    /// accumulator past the boundary before emitting it.
    BoundaryBehindAppends { boundary: u32, last_height: u32 },
    /// Store access / decode failure in the derivation layer.
    Store(String),
}

impl std::fmt::Display for RootsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfOrder { prev, next } => write!(
                f,
                "coin stream out of canonical order: ({}, {}) then ({}, {})",
                prev.0, prev.1, next.0, next.1
            ),
            Self::SpentBeforeCreated {
                coin_id,
                confirmed,
                spent,
            } => write!(
                f,
                "coin {coin_id} spent at {spent} before created at {confirmed}"
            ),
            Self::BoundaryBehindAppends {
                boundary,
                last_height,
            } => write!(
                f,
                "boundary {boundary} requested after appending a coin confirmed at {last_height}"
            ),
            Self::Store(msg) => write!(f, "store: {msg}"),
        }
    }
}

impl std::error::Error for RootsError {}

fn sha256_parts(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

fn empty_root() -> [u8; 32] {
    sha256_parts(&[DOMAIN_EMPTY])
}

fn node(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    sha256_parts(&[DOMAIN_NODE, left, right])
}

/// Binary-counter MMR peak set: one perfect-subtree peak per set bit of the leaf count,
/// leftmost (largest) first.
#[derive(Default, Clone)]
struct Peaks {
    // (subtree order, subtree root): orders strictly descend left to right.
    stack: Vec<(u32, [u8; 32])>,
}

impl Peaks {
    fn append(&mut self, leaf: [u8; 32]) {
        self.stack.push((0, leaf));
        while self.stack.len() >= 2 {
            let (ro, rh) = self.stack[self.stack.len() - 1];
            let (lo, lh) = self.stack[self.stack.len() - 2];
            if lo != ro {
                break;
            }
            self.stack.truncate(self.stack.len() - 2);
            self.stack.push((lo + 1, node(&lh, &rh)));
        }
    }

    /// Bag peaks right-to-left, then bind the leaf count under `domain`.
    fn root(&self, domain: &[u8], leaf_count: u64) -> [u8; 32] {
        let Some((_, first)) = self.stack.last() else {
            return empty_root();
        };
        let mut acc = *first;
        for (_, peak) in self.stack.iter().rev().skip(1) {
            acc = sha256_parts(&[DOMAIN_BAG, peak, &acc]);
        }
        sha256_parts(&[domain, &leaf_count.to_le_bytes(), &acc])
    }
}

/// One derived boundary root: the v1 commitment plus its inputs and tallies.
#[derive(Serialize, Clone, Debug)]
pub struct RootV1 {
    pub height: u32,
    pub header_hash: Bytes32,
    pub coin_count: u64,
    pub spent_count: u64,
    pub mmr_root: Bytes32,
    pub spent_bitmap_root: Bytes32,
    pub root_v1: Bytes32,
}

/// The v1 coin-set accumulator: streaming appends in canonical order, roots emittable at any
/// boundary at or above the last appended coin's height (multi-boundary single-pass driver).
#[derive(Default)]
pub struct CoinSetAccumulator {
    peaks: Peaks,
    // Immutable insert-time spend pointer per leaf (0 = unspent); the as-of-`H` bitmap is
    // recomputed per boundary as `0 < spent_at[i] <= H`.
    spent_at: Vec<u32>,
    last: Option<(u32, Bytes32)>,
}

impl CoinSetAccumulator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn len(&self) -> u64 {
        self.spent_at.len() as u64
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.spent_at.is_empty()
    }

    /// Append the next coin creation in canonical `(confirmed_height, coin_id)` order.
    ///
    /// `spent_index` is the coin's spend height as the store records it (0 = unspent); the
    /// boundary predicate is applied at [`Self::root_at`] time, so one pass serves every
    /// boundary.
    ///
    /// # Errors
    /// [`RootsError::OutOfOrder`] on a non-ascending append (misordered backend stream or
    /// duplicate coin); [`RootsError::SpentBeforeCreated`] on a spend pointer below the
    /// creation height.
    pub fn append(
        &mut self,
        coin_id: Bytes32,
        confirmed_height: u32,
        timestamp: u64,
        spent_index: u32,
    ) -> Result<(), RootsError> {
        if let Some(prev) = self.last
            && (confirmed_height, &*coin_id) <= (prev.0, &*prev.1)
        {
            return Err(RootsError::OutOfOrder {
                prev,
                next: (confirmed_height, coin_id),
            });
        }
        if spent_index != 0 && spent_index < confirmed_height {
            return Err(RootsError::SpentBeforeCreated {
                coin_id,
                confirmed: confirmed_height,
                spent: spent_index,
            });
        }
        let index = self.spent_at.len() as u64;
        let leaf = sha256_parts(&[
            DOMAIN_COIN_LEAF,
            &index.to_le_bytes(),
            &*coin_id,
            &confirmed_height.to_le_bytes(),
            &timestamp.to_le_bytes(),
        ]);
        self.peaks.append(leaf);
        self.spent_at.push(spent_index);
        self.last = Some((confirmed_height, coin_id));
        Ok(())
    }

    /// Emit the v1 root at boundary `height` over everything appended so far. The caller must
    /// have appended exactly the coins with `confirmed_height <= height` (the single-pass
    /// driver's contract); appending anything above the boundary first is rejected.
    ///
    /// # Errors
    /// [`RootsError::BoundaryBehindAppends`] if a coin above `height` was already appended.
    pub fn root_at(&self, height: u32, header_hash: Bytes32) -> Result<RootV1, RootsError> {
        if let Some((last_height, _)) = self.last
            && last_height > height
        {
            return Err(RootsError::BoundaryBehindAppends {
                boundary: height,
                last_height,
            });
        }
        let n = self.len();
        let mmr_root = self.peaks.root(DOMAIN_MMR_ROOT, n);
        let (spent_bitmap_root, spent_count) = self.bitmap_root(height);
        let root_v1 = sha256_parts(&[
            DOMAIN_ROOT,
            &mmr_root,
            &spent_bitmap_root,
            &height.to_le_bytes(),
            &*header_hash,
        ]);
        Ok(RootV1 {
            height,
            header_hash,
            coin_count: n,
            spent_count,
            mmr_root: Bytes32::from(mmr_root),
            spent_bitmap_root: Bytes32::from(spent_bitmap_root),
            root_v1: Bytes32::from(root_v1),
        })
    }

    // The as-of-`height` spent-bitmap commitment: chunked bitmap MMR + spent tally.
    fn bitmap_root(&self, height: u32) -> ([u8; 32], u64) {
        let mut peaks = Peaks::default();
        let mut spent_count = 0u64;
        let mut chunk = [0u8; BITMAP_CHUNK_BITS / 8];
        let mut chunk_index = 0u64;
        let mut bit = 0usize;
        for &spent in &self.spent_at {
            if spent != 0 && spent <= height {
                chunk[bit / 8] |= 1 << (bit % 8);
                spent_count += 1;
            }
            bit += 1;
            if bit == BITMAP_CHUNK_BITS {
                peaks.append(sha256_parts(&[
                    DOMAIN_BITMAP_LEAF,
                    &chunk_index.to_le_bytes(),
                    &chunk,
                ]));
                chunk = [0u8; BITMAP_CHUNK_BITS / 8];
                chunk_index += 1;
                bit = 0;
            }
        }
        if bit != 0 {
            peaks.append(sha256_parts(&[
                DOMAIN_BITMAP_LEAF,
                &chunk_index.to_le_bytes(),
                &chunk,
            ]));
        }
        (peaks.root(DOMAIN_BITMAP_ROOT, self.len()), spent_count)
    }
}
