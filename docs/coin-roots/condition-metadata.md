# Condition-metadata audit — order-independent (SwiftSync-class) validation

**The make-or-break analysis for state-commitment metadata.**
`roots/src/lib.rs` (the phase-1 v1 leaf spec this extends), `docs/coin-roots/README.md`
(phase-1 derived-roots trust shape).

Status: analysis + spec. The v2 leaf and the aggregate-validation core it feeds are
implemented and proved in deliverables 2–3.

## 0. The question this answers

SwiftSync validates blocks in **any order** by replacing the moment-to-moment UTXO set with
a commutative aggregate: hash-**add** every coin creation, hash-**remove** every spend; the
adds and removes cancel regardless of processing order (Somsen 2025;
<https://gist.github.com/RubenSomsen/a61a37d14182ccd78760e477c78133cd>). The aggregate is
*fail-closed*: a wrong hint or wrong metadata breaks the final cancellation, so it can never
cause acceptance of invalid state.

Order-independence holds **only if every consensus rule a spend must satisfy can be checked
without consulting mutable state that changes as other blocks are processed.** A block's
own generator/roots/aggsig/cost check is already self-contained. The open question — the one
that decides whether SwiftSync works on chia at all — is the **condition set**: does any
condition require data about the *spent coin* that is (a) not carried by the spend and (b)
not derivable from the block being validated?

The known family reads the spent coin's **creation height / timestamp**
(`ASSERT_HEIGHT_RELATIVE` et al.). This document enumerates the **entire** chia condition
opcode set, classifies each, defines the fold for the metadata-dependent ones, and issues a
**GO / NO-GO**.

## 1. Two independent mechanisms (the core finding, stated first)

Every consensus-relevant condition falls into exactly one of four buckets. Only bucket **C**
needs the SwiftSync metadata fold; bucket **D** is order-independent for a *different* reason
that must not be conflated with the fold:

| Bucket | Dependency | Order-independence mechanism |
|---|---|---|
| **A. Spend-local** | data carried by the spend itself (parent, puzzle_hash, amount, coin_id, the condition args) | trivially self-contained |
| **B. Block-local** | the current block's height/timestamp and its own add/remove set (fees, absolute time-locks) | self-contained per block |
| **C. Cross-block creation-metadata** | the **spent coin's creation height / timestamp** | **the metadata fold** (§4): creator commits it, hints supply it, a wrong value breaks cancellation |
| **D. Intra-block scoping** | another spend/coin **in the same block** (announcements, concurrent, messages, ephemeral) | **block-atomic validation**: the block is the validation unit, so the scope is resolved entirely inside one block; order *across* blocks is irrelevant |

The load-bearing distinction: **the fold handles cross-block metadata; block-atomicity
handles intra-block scoping.** They are different mechanisms. The fold does *not* make
announcements order-independent — atomic per-block validation does. Conflating them is the
trap; separating them is why the audit is GO. **No condition reads global mutable state whose
value depends on the order blocks are processed in.** That is the whole property.

## 2. Coin-identity self-authentication (why the accumulator element needs no lookup)

A chia coin id is `sha256(parent_coin_info || puzzle_hash || amount)` and a spend carries all
three: `parent_coin_info` (the coin it references), the puzzle **reveal** (which must hash to
`puzzle_hash`), and `amount`. So both the creation element and the spend element are
computable **entirely from the block being validated** — no UTXO lookup, no ordering, no
state. This is strictly better than Bitcoin, where the spend references an outpoint and the
spent `scriptPubKey`/`amount`/`height` must be fetched. (Design doc §2; verified against
`core/src/blockchain/coin.rs` coin-id construction and `condition_utils.rs::
created_outputs_for_conditions`, which builds each new coin's `parent_coin_info` from the
spending coin's id.)

Consequence: the *identity* half of every add/remove is self-authenticating. The only thing a
spend does **not** carry about the coin it spends is that coin's **creation metadata** — its
confirmed height and the confirming block's timestamp. That is bucket C, and it is the entire
residual surface.

## 3. Full condition enumeration + classification

Opcode set taken from `core/src/blockchain/condition_opcode.rs` (dg_xch `ConditionOpcode`),
verified byte-for-byte against chia-blockchain `chia/types/condition_opcodes.py` (all 33
opcodes + `REMARK` + the `Unknown`/soft-fork-forward sentinel). Argument shapes and the
metadata each reads are taken from `core/src/blockchain/condition_with_args.rs`
(`from_opcode_with_args`) and the parsed per-spend surface `core/src/blockchain/spend.rs`
(`struct Spend`), which mirrors chia_rs `SpendConditions`
(`chia-consensus/src/gen/conditions.rs`). Semantics cross-checked against chialisp
`chia/wallet/puzzles/condition_codes.clib` (referenced by the opcode file) and the chia
`_tests/core/full_node/test_conditions.py` corpus (cited inline in
`condition_with_args.rs::agg_sig_infinity_pubkey`).

Legend for **needs-metadata?**: **no** = bucket A/B/D (self-contained in spend, block, or the
same block); **YES** = bucket C (reads the spent coin's creation height or timestamp).

| # | Condition (opcode) | What it checks | Reads about spent coin's *creation*? | Bucket | Treatment |
|---|---|---|---|---|---|
| 1 | `REMARK` | always true, args ignored | no | — | no-op |
| 43 | `AGG_SIG_PARENT` | BLS sig over `parent_id‖msg‖genesis` | no | A | none (spend + genesis const) |
| 44 | `AGG_SIG_PUZZLE` | sig over `puzzle_hash‖msg‖genesis` | no | A | none |
| 45 | `AGG_SIG_AMOUNT` | sig over `amount‖msg‖genesis` | no | A | none |
| 46 | `AGG_SIG_PUZZLE_AMOUNT` | sig over `puzzle_hash‖amount‖…` | no | A | none |
| 47 | `AGG_SIG_PARENT_AMOUNT` | sig over `parent‖amount‖…` | no | A | none |
| 48 | `AGG_SIG_PARENT_PUZZLE` | sig over `parent‖puzzle_hash‖…` | no | A | none |
| 49 | `AGG_SIG_UNSAFE` | sig over `msg‖genesis` | no | A | none |
| 50 | `AGG_SIG_ME` | sig over `coin_id‖genesis‖msg` | no | A | none (coin_id from spend) |
| 51 | `CREATE_COIN` | emits a new coin | no | A | this **is** the accumulator ADD |
| 52 | `RESERVE_FEE` | `Σ in − Σ out − reserved ≥ 0` | no | B | block-local (amounts carried by spends) |
| 60 | `CREATE_COIN_ANNOUNCEMENT` | emit announcement | no | D | block-atomic |
| 61 | `ASSERT_COIN_ANNOUNCEMENT` | match a CREATE in **same block** | no | D | block-atomic |
| 62 | `CREATE_PUZZLE_ANNOUNCEMENT` | emit announcement | no | D | block-atomic |
| 63 | `ASSERT_PUZZLE_ANNOUNCEMENT` | match a CREATE in **same block** | no | D | block-atomic |
| 64 | `ASSERT_CONCURRENT_SPEND` | named coin id spent in **same block** | no | D | block-atomic |
| 65 | `ASSERT_CONCURRENT_PUZZLE` | named puzzle spent in **same block** | no | D | block-atomic |
| 66 | `SEND_MESSAGE` (CHIP-25) | inter-spend message, **same block** | no | D | block-atomic |
| 67 | `RECEIVE_MESSAGE` (CHIP-25) | inter-spend message, **same block** | no | D | block-atomic |
| 70 | `ASSERT_MY_COIN_ID` | `coin_id == arg` | no | A | none (coin_id from spend) |
| 71 | `ASSERT_MY_PARENT_ID` | `parent == arg` | no | A | none |
| 72 | `ASSERT_MY_PUZZLEHASH` | `puzzle_hash == arg` | no | A | none |
| 73 | `ASSERT_MY_AMOUNT` | `amount == arg` | no | A | none |
| 74 | `ASSERT_MY_BIRTH_SECONDS` | `created_ts == arg` | **YES (ts)** | **C** | **fold** |
| 75 | `ASSERT_MY_BIRTH_HEIGHT` | `created_height == arg` | **YES (h)** | **C** | **fold** |
| 76 | `ASSERT_EPHEMERAL` | coin created earlier in **same block** | no¹ | D | block-atomic |
| 80 | `ASSERT_SECONDS_RELATIVE` | `now_ts − created_ts ≥ arg` | **YES (ts)** | **C** | **fold** |
| 81 | `ASSERT_SECONDS_ABSOLUTE` | `now_ts ≥ arg` | no | B | block-local |
| 82 | `ASSERT_HEIGHT_RELATIVE` | `now_h − created_height ≥ arg` | **YES (h)** | **C** | **fold** |
| 83 | `ASSERT_HEIGHT_ABSOLUTE` | `now_h ≥ arg` | no | B | block-local |
| 84 | `ASSERT_BEFORE_SECONDS_RELATIVE` | `now_ts < created_ts + arg` | **YES (ts)** | **C** | **fold** |
| 85 | `ASSERT_BEFORE_SECONDS_ABSOLUTE` | `now_ts < arg` | no | B | block-local |
| 86 | `ASSERT_BEFORE_HEIGHT_RELATIVE` | `now_h < created_height + arg` | **YES (h)** | **C** | **fold** |
| 87 | `ASSERT_BEFORE_HEIGHT_ABSOLUTE` | `now_h < arg` | no | B | block-local |
| 90 | `SOFTFORK` | reserves cost; extension point | no | B | block-local (cost only) |
| 0 | `Unknown` | unrecognised → reject / soft-fork-forward | no | — | no state |

¹ `ASSERT_EPHEMERAL`: an ephemeral coin's `created_height` **equals the current block
height** by definition (created and spent in the same block), so its metadata is derivable
from the block itself — no hint needed. It is bucket **D**, resolved by finding the coin's
creator among this block's other spends.

### The metadata-dependent family is exactly six opcodes over exactly two scalars

- **created_height** (`u32`): `ASSERT_MY_BIRTH_HEIGHT` (75), `ASSERT_HEIGHT_RELATIVE` (82),
  `ASSERT_BEFORE_HEIGHT_RELATIVE` (86).
- **created_timestamp** (`u64`): `ASSERT_MY_BIRTH_SECONDS` (74),
  `ASSERT_SECONDS_RELATIVE` (80), `ASSERT_BEFORE_SECONDS_RELATIVE` (84).

`core/src/blockchain/spend.rs::Spend` already isolates exactly this surface as the six
`Option` fields `birth_height`, `birth_seconds`, `height_relative`, `seconds_relative`,
`before_height_relative`, `before_seconds_relative` — the parsed condition set carries nothing
else that reads creation metadata. This is the same `(confirmed_height, timestamp)` pair the
phase-1 v1 leaf already commits (`roots/src/lib.rs`, coin-leaf, and the comment at the
`timestamp` field: *"carried in the leaf because SwiftSync-style spend validation of relative
time/height conditions needs the creation metadata (the known wrinkle)"*).

The relative time-lock reference point in chia is the coin's confirmed block vs. the
**previous transaction block** of the spending block; both `now_h/now_ts` (block-local) and
`created_height/created_ts` (metadata) are the only inputs — no other coin's state is read.
Cross-checked against chia_rs `chia-consensus/src/gen/conditions.rs`
(`assert_height_relative`, `assert_seconds_relative`, `assert_before_*` fields of
`SpendConditions`) and chia-blockchain `condition_opcodes.py` (verified locally). Values are
parsed with saturating conversions (`condition_with_args.rs`,
`saturating_u32/u64_from_bigint`) — parity to keep.

## 4. The fold (SwiftSync's BIP-68 treatment, applied to bucket C)

SwiftSync's BIP-68 treatment for Bitcoin: the spent output's block height is needed to check
a relative time-lock but is not in the spend; SwiftSync folds it into the aggregate element
committed at **creation** and hands the value to the spender via hints, so a wrong value fails
the final cancellation (Somsen gist, "relative timelocks" / BIP-68 section). Chia's fold is
the direct analogue, and the metadata is a superset-free two-scalar tuple.

### v2 coin leaf (extends phase-1 v1; a new version, never a mutation of v1)

Phase-1's v1 leaf is **order-dependent** — it binds `LE64(i)`, the leaf's index in canonical
`(confirmed_height, coin_id)` order (`roots/src/lib.rs`, coin-leaf). That is correct for an
ordered MMR but fatal for a commutative add/remove aggregate: an add and its later remove must
produce the **identical** element to cancel, and the spender does not know the coin's eventual
canonical index. **The v2 fold leaf drops the index** and binds only self-authenticating coin
data plus the folded metadata:

```text
coin fold leaf  F = H("dgxch.coinroot.v2.fold-leaf"
                      || coin_id[32]
                      || LE32(created_height)
                      || LE64(created_timestamp))
```

- `coin_id` self-authenticates identity (§2); `created_height`/`created_timestamp` are the
  bucket-C metadata. No index, no position, no block-scoped data → **`F` is identical whether
  produced at creation (from the block) or at spend (from the hint).**
- **Creation** (accumulator ADD): the block being validated supplies `coin_id` (computed) and
  the creating block's `(height, timestamp)` (this block) → `F` is fully derived, no hint.
- **Spend** (accumulator REMOVE): the spender computes `coin_id` from the spend and reads
  `(created_height, created_timestamp)` from the **hint** for that coin, then (i) validates
  every bucket-C condition against those two scalars and (ii) removes `F`.
- **Wrong metadata breaks cancellation:** if the hint gives `created_height' ≠ created_height`,
  the removed `F'` differs from the added `F` in ~128 bits, so the coin's add is never
  cancelled and the aggregate does not reconcile to the phase-1 root → sync fails, fail-closed.
  The very same scalar that was validated against the condition is the scalar that must match
  to cancel, so **a metadata value good enough to pass the fold is provably the true creation
  metadata** — the condition check and the cancellation check are welded to the same bytes.

### Where the metadata comes from for pre-range coins

For a from-genesis range every spent coin was created earlier **in the same range**, so every
REMOVE has a matching in-range ADD — the fold is fully internal, hints only mark survivors.
For a bounded range that starts at a snapshot (the tip-first / assumeutxo use), spends of
coins created **before** the range are seeded as pre-loaded ADDs from the range-start UTXO
set, each carrying its `(created_height, created_timestamp)` — i.e. the start-boundary phase-1
commitment already binds exactly these two scalars per coin, so the seed is verifiable against
the phase-1 root at the range-start SES boundary. No new metadata surface.

## 5. Fail-closed argument (why wrong inputs cannot pass)

1. **Wrong survivor hint** (coin marked spent that survives, or vice-versa): the mismatched
   ADD/REMOVE count leaves a non-cancelling residual → root mismatch.
2. **Wrong metadata** (§4): the REMOVE element diverges from the ADD element → residual.
3. **Missing coin** (a real creation dropped, or a spend of a coin never created): an
   unmatched REMOVE (spend with no ADD) or an orphan ADD → residual. Coin-id
   self-authentication (§2) means a forged coin cannot borrow a real coin's identity without
   also matching its `(parent, puzzle_hash, amount)` preimage.
4. **Authoritative commitment is the phase-1 SHA-256 MMR + spent-bitmap root**, not the
   commutative aggregate. The aggregate is the order-independence engine and the cheap
   fail-closed pre-check; the surviving multiset it produces is canonicalised and run through
   the phase-1 ordered MMR (`roots::CoinSetAccumulator`) for the binding root. Soundness of the
   final artifact therefore rests on the already-shipped, collision-resistant phase-1 hashing,
   not on the aggregate's weaker algebra.

## 6. GO / NO-GO

**GO.** Every consensus-relevant condition in chia's full opcode set is order-independent
under the two-mechanism treatment:

- Buckets **A** (spend-local) and **B** (block-local) are self-contained with no state and no
  metadata.
- Bucket **C** is exactly six opcodes reading exactly two scalars per coin
  (`created_height: u32`, `created_timestamp: u64`); the v2 fold leaf makes both
  self-authenticating and fail-closed.
- Bucket **D** (nine intra-block opcodes: announcements 60–63, concurrent 64–65, messages
  66–67, ephemeral 76) is order-independent **across blocks** because chia validates a block
  atomically — the scope never crosses a block boundary.

**No condition fundamentally resists the fold.** There is no opcode that reads mutable global
state whose value depends on block-processing order. The design's central assumption holds on
stock chia semantics.

### Residual / caveats to carry into deliverables 2–3

1. **v1 is order-dependent by design; v2 must drop the leaf index.** The single most likely
   implementation error is reusing the v1 coin-leaf (with `LE64(i)`) in the commutative
   aggregate — it cannot cancel. v2 leaf per §4; guard with the red-first shuffled-order proof.
2. **Bucket D is proven order-independent by block-atomicity, not by the fold.** The core must
   validate each block's announcement/concurrent/message/ephemeral scoping **within** that
   block before folding its deltas — never defer an intra-block assert to a global pass. A
   corpus with a heavy CHIP-25 / concurrent-spend block should be included beyond era-a
   (era-a is CAT flood; CATs lean on announcements, so era-a already exercises D — confirm the
   era-a block set actually contains 60–67 usage, else add an era that does).
3. **Saturating parse semantics** (`saturating_u32/u64_from_bigint`) and the AGG_SIG-infinity
   soft-fork-5 rejection (`condition_with_args.rs::agg_sig_infinity_pubkey`) are chia-parity
   behaviours the validation core consumes via the existing `core` public API — do **not**
   re-derive them (a consensus campaign owns `core/src/consensus`; consume, never edit).
4. **Ephemeral + relative-timelock interaction:** a coin created and spent in the same block
   with `ASSERT_HEIGHT_RELATIVE > 0` (or seconds) must fail (age 0 < arg). This falls out of
   the fold automatically because the ephemeral coin's folded `created_height == now_h`; add a
   targeted red case so a future refactor cannot silently exempt ephemeral coins.
5. **Relative reference point** is the *previous transaction block*, not the current block, for
   the `now_h/now_ts` side. That is block-local (bucket B) data the block carries, but the core
   must source it from the block's foliage/transaction-block info, not from wall-clock — parity
   with chia_rs `conditions.rs`.

This clears deliverable 1: the metadata treatment is **sufficient**, so the approach proceeds
to the aggregate-validation core (deliverable 2) and the era-a red-first proof (deliverable 3).
```
