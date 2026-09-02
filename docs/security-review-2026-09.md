# Security Review — September 2026

A community security report against revision `728ae4a` surfaced 16 findings that
deduplicate to 11 distinct issues. Every claim was independently re-verified by
direct source trace before landing here; several of the report's line numbers
were corrected in the process. This document records the verdict, the trace,
the real exposure for a public mainnet full node, a proposed remediation
strategy (with alternatives where a genuine choice exists), and a red test
demonstrating each confirmed issue.

The red tests live in the tree alongside this document, marked
`#[ignore = "red: ..."]` so the suite stays green while each finding remains
demonstrable: run any of them with `cargo test -- --ignored <name>` and watch
it fail until its fix lands. Each fix should flip its red test to a permanent
regression gate (remove the ignore) in the same commit.

Ranked by real exposure for a public mainnet full node.

---

## 1. Inbound P2P bypasses the configured peer cap — CONFIRMED, HIGH

**Trace.** `p2p/src/sessions/mod.rs` `admit_inbound` enforces
`inbound + outbound >= target_peer_count`, but the full node's inbound
listener never routes through it: `full-node/src/daemon.rs` builds the
`WebsocketServer` directly (the comment at the call site says as much), and
`servers/src/websocket/mod.rs` inserts every accepted peer into the raw
`PeerMap` with no count gate. Pre-upgrade sessions are spawned unbounded — no
semaphore, no idle deadline. `deregister_peer` keeps the map leak-free under
churn but imposes no ceiling.

**Exposure.** Unauthenticated and internet-facing on every public node: an
attacker opens N concurrent inbound sockets and holds N live tasks and
PeerMap entries, unbounded, until descriptor or memory exhaustion.

**Red test.** `p2p/tests/red_security_review.rs::inbound_sessions_respect_the_peer_cap`

**Proposed strategy.** A shared `Arc<Semaphore>` sized from the peer-count
setting, acquired *before* the HTTP/TLS upgrade is served and released on
every teardown path, plus routing the accept through `admit_inbound` so
inbound and outbound draw from the same budget. Add an idle deadline for
pre-upgrade sessions (a socket that never completes the upgrade is torn down).

**Alternatives.**
- Per-IP caps instead of (or in addition to) a global cap — stronger against
  a single-host flood, weaker against a distributed one; the global semaphore
  is the necessary floor either way.
- Enforce only in the daemon rather than the server crate — keeps
  `WebsocketServer` generic, but every future embedder re-inherits the bypass;
  the server-side permit is the class fix.

---

## 2. Inline TLS handshake serializes the accept loops — CONFIRMED, HIGH (P2P)

**Trace.** `servers/src/websocket/mod.rs` awaits `acceptor.accept(stream)`
inside the accept loop body, so the next `listener.accept()` cannot run until
the current handshake completes; a peer that completes TCP and withholds the
ClientHello stalls all new inbound accepts indefinitely (no timeout exists).
The RPC listener in `servers/src/rpc/mod.rs` has the identical pattern —
lower exposure only because RPC binds loopback by default.

**Exposure.** One cheap socket denies all new inbound peers on the public
port. No certificate or protocol message required.

**Red test.** `p2p/tests/red_security_review.rs::a_stalled_handshake_does_not_block_new_accepts`

**Proposed strategy.** Spawn each handshake as its own task under
`tokio::time::timeout`, with a bounded pending-handshake semaphore (global,
modest — e.g. 64) so a handshake flood cannot trade the stall for task
exhaustion. Apply the same shape to both listeners.

**Alternatives.** None worth considering — this is the standard accept-loop
pattern; the only real choice is the timeout value (5–10s matches the
outbound connect timeout).

---

## 3. Client-certificate proof of possession is not verified — CONFIRMED, HIGH when trust is configured

**Trace.** `core/src/ssl.rs` `AllowAny` stubs all three verifier methods:
`verify_client_cert` returns an assertion (intended — the public port accepts
any cert, chia-style), but `verify_tls12_signature` and
`verify_tls13_signature` **also** return `HandshakeSignatureValid::assertion()`
without checking the CertificateVerify signature against the presented
certificate's public key. That signature is the only thing proving the client
*owns* the certificate it presented. With it stubbed, a peer can present a
byte-copy of any other node's (public) certificate and the handshake
succeeds; the peer id becomes `hash(cert_bytes)`
(`servers/src/websocket/mod.rs`), which feeds the trusted-peer tier
(`full-node/src/trust.rs`): the 2,000,000-item subscription and 500,000-item
response caps and priority transaction-queue placement.

This is **not** chia parity: chia's permissive verifier skips *chain*
validation but leaves rustls's handshake-signature verification intact.
Stubbing possession is our deviation.

**Exposure.** An authentication bypass of the `--trusted-peer` cert-hash tier
for any operator who configures it. A stock node with no trusted peers loses
nothing (localhost trust is by IP).

**Red test.** `core/tests/red_client_cert_possession.rs::a_client_without_the_certificates_key_fails_the_handshake`

**Proposed strategy.** Keep optional client certs on the public port, but
delegate the two signature methods to
`rustls::crypto::verify_tls12_signature` / `verify_tls13_signature` with the
provider's supported schemes — possession proven, identity meaningful, no
behavioral change for honest peers.

**Alternatives.**
- `WebPkiClientVerifier` against the private CA for the farmer/harvester
  private roles (they have a CA; the public port cannot use this).
- Drop cert-hash trust entirely and trust by CIDR only — smaller fix surface
  but removes a documented operator feature.

---

## 4. An empty v1 proof of space panics the verifier — CONFIRMED, contained (fails closed)

**Trace.** Peer-supplied `ProofOfSpace.proof` reaches
`proof_of_space/src/verifier.rs` `uncompress_proof`, which builds a
`BitReader` over the raw bytes and indexes `buffer[0]`
(`utils/bit_reader.rs`) — an empty proof panics on index-out-of-bounds (and
underflows `buffer.len()-1`). The filter and size predicates upstream are
satisfiable by grinding, so the panic is reachable from block, unfinished-
block, and declare paths.

**Containment (verified).** The panic fails *closed* everywhere it can fire:
a drain-task panic maps to a fail-closed verdict, a poisoned staging sink
fails the window, and a handler-task panic drops only that connection. This
is a griefing vector (task/connection kills, re-staged windows), not a
corruption or crash vector.

**Red test.** `proof_of_space/tests/red_security_review.rs::an_empty_proof_is_rejected_not_a_panic`

**Proposed strategy.** Bounds-gate at the entry: require the proof length to
cover `64 * k` bits before decompression and return an error; make
`BitReader::slice_to_int` bounds-checked as defense in depth.

**Alternatives.** None — this is a straightforward input-validation gate.

---

## 5. Pool client accepts any TLS certificate and signs unauthenticated pool state — CONFIRMED, MEDIUM (wallet/CLI)

**Trace.** `clients/src/api/pool.rs` sets `danger_accept_invalid_certs(true)`
on the pool HTTP client. `cli/src/wallet_commands.rs` fetches `pool_info`
over that channel, validates only `relative_lock_height <= 1000`, and copies
the unauthenticated `target_puzzle_hash` into a `PoolState` the wallet then
signs and submits on-chain.

**Exposure.** An on-path attacker serving forged `pool_info` redirects future
pool rewards without touching the wallet key. This is the money-theft finding
of the set; it lives in the wallet/CLI, not the node.

**Red test.** `clients/tests/red_pool_tls.rs::the_pool_client_rejects_an_unverified_certificate`

**Proposed strategy.** Remove `danger_accept_invalid_certs`; verify chain and
hostname via webpki (pools serve public HTTPS), and print the pool's target
puzzle hash + lock height for operator confirmation before signing.

**Alternatives.**
- Per-pool certificate pinning — stronger, but operationally brittle for
  public pools that rotate certs; webpki + confirm-before-sign is the
  proportionate default.

---

## 6. RPC bodies are fully buffered before the size cap — CONFIRMED, LOW

**Trace.** `full-node/src/rpc.rs` `read_body` collects the complete body into
a `Vec` in both arms; the 1 MB cap is compared only afterwards. Remote reach
requires an authenticated private-CA client (loopback default), so this is
memory amplification available to an already-authenticated caller.

**Red test.** `full-node/src/rpc.rs` unit `red_oversized_body_is_rejected_while_streaming`

**Proposed strategy.** Enforce the cap while streaming — accumulate frames
and error the moment the running total passes the cap.

**Alternatives.** `http_body_util::Limited` wraps the same behavior at the
body-type level; either is fine, the streaming accumulator avoids a new
wrapper type in the handler signatures.

---

## 7. `Span` lets safe code manufacture undefined behavior — CONFIRMED, real soundness bug, LOW security

**Trace.** `proof_of_space/src/utils/span.rs`: `Span<T>` wraps a raw
`*mut T` with a **safe** public `new`, `Copy`/`Clone`, blanket
`unsafe impl Send/Sync` regardless of `T`, and safe `AsRef`/`AsMut`/`Index`
guarded only by `debug_assert!` (elided in release). Safe downstream code can
construct a dangling span and dereference it with no `unsafe` block.

**Blast radius (measured).** Every `Span::new` call site is inside the
`proof_of_space` crate (~83 sites: decompressor 69, radix_sort 8,
fx_generator 5, encoding 1). No external crate constructs one — the fix does
not ripple past the crate boundary.

**Red test.** Not shipped as a runtime test: demonstrating this one means
executing undefined behavior, which has no honest place in a test suite even
ignored. The fix should land with a `trybuild` compile-fail test asserting
`Span::new` is uncallable from safe code.

**Proposed strategy.** Mark `new`/`cast` `unsafe` with a documented contract
(validity, alignment, aliasing, lifetime), bound `Send`/`Sync` on
`T: Send`/`T: Sync`, and promote the bounds checks to release builds. ~83
mechanical call-site edits, one crate.

**Alternatives.** Replace `Span` with lifetime-bound slices outright —
cleaner end state, but a substantially larger rewrite of the plotter
internals for the same soundness result; reasonable as a later refactor.

---

## 8. Crafted plot header lengths panic the reader — CONFIRMED, LOW (local)

**Trace.** `proof_of_space/src/plots/plot_reader.rs` reads u16 length fields
(`format_desc_len`, `memo_len`, and the v2 variants) and uses them as slice
endpoints into a fixed 320-byte buffer with no bounds check — valid magic
plus `0xffff` panics before the normal parse error.

**Red test.** `proof_of_space/tests/red_security_review.rs::an_oversized_header_length_is_a_parse_error_not_a_panic`

**Proposed strategy.** Checked cursor arithmetic with `slice.get(..)` and
parse errors throughout the header path.

**Alternatives.** None.

---

## 9. SSL private keys are written with ambient permissions — CONFIRMED, LOW (local)

**Trace.** `core/src/ssl.rs` `write_ssl_cert_and_key` opens key files with no
mode — they land at `0666 & ~umask`, commonly world-readable. The full-node
CA path chmods `0600` *after* writing, ignores failure, and skips existing
files; every other caller (node/farmer/harvester keys) gets ambient
permissions.

**Red test.** `core/tests/red_key_perms.rs::private_keys_are_owner_only`

**Proposed strategy.** Open key files with `mode(0o600)` at creation
(`OpenOptionsExt`, unix) inside the shared writer, and repair existing modes
on load.

**Alternatives.** None.

---

## 10. Protocol clients accept any server certificate — split verdict

**P2P WebSocket client: accepted posture.** Node-to-node identity in the
Chia network is the certificate hash, not a webpki chain — peers dial by IP,
the world-public CA proves nothing, and consensus/signature validation
constrains forged data. This matches chia's own outbound posture. Proposed
action: keep, but document the exception at the verifier and scope it to the
P2P client only.

**RPC client: confirmed, low.** The RPC side *has* a real CA (the same
private CA the server verifies clients against), so the RPC client accepting
any server certificate is an unnecessary MITM exposure. Proposed strategy:
seed the RPC client with the private CA (or an explicit pin) and verify
normally; keep the permissive verifier exclusively for the P2P path.

**Red test.** Covered by the same class as the pool-client test; a dedicated
RPC-client variant is trivial to add with the fix.

---

## 11. Simulator binds public plain-HTTP state mutation — CONFIRMED, dev-only

**Trace.** `simulator/src/bin/sim_node.rs` defaults the control listener to
`0.0.0.0:5050`, plain HTTP, full state-mutation dispatch, unbounded bodies.

**Verdict.** Real, but the simulator is a separately-launched developer
binary outside the mainnet node's threat model. Proposed action: default the
bind to `127.0.0.1` (one line) and leave the rest; anything more is scope the
tool doesn't warrant.

---

## Proposed order of work

1. **Findings 1 + 2** — the unauthenticated internet-facing DoS pair on the
   always-on P2P port; one PR, red tests flipped to gates.
2. **Finding 3** — possession verification; small, and an authentication
   bypass for any trust-configured operator.
3. **Findings 4, 6, 8, 9, 11 and the RPC half of 10** — the input-validation
   and hygiene batch; each is small.
4. **Finding 5** — the wallet/pool money path.
5. **Finding 7** — the Span soundness refactor, as its own PR.

Dependency check: none of the above adds or changes a dependency; RustSec is
clean at the reviewed revision.
