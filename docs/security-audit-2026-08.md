# dg_xch_utils full-node — security audit (PR #51 HEAD `3bfebc4`)

Auditor: Sally (application & enterprise security). Date: 2026-08-26.
Scope base: worktree `dgxch-idxphase`, branch `tip-follow-fixes` @ `3bfebc4` (PR #51 HEAD, the
#49 foundation + #51 additions, audited whole). Read-only on #51; this report + the red test live
on the separate `pr-52-security` branch.

## Fit
`in-domain` for the application-security surface (deserialization/DoS, injection, access control,
TLS/authn, secrets, panics-as-DoS, `unsafe` memory safety). Two boundaries respected: **CLVM /
consensus correctness** (cost-accounting math, VDF/PoS/BLS *validity* rules) routes to **chino**;
**async / wire-protocol correctness** and the RPC TLS *trust-model fix* route to **james / Grant**.
Findings below are traced to a sink or demonstrated with a test; anything I could not ground is
marked [UNVERIFIED] with the exact proof required.

## Threat-model contract
- **Asset / data class:** a mainnet consensus full node — chain state (coin/block store), the
  mempool, peer topology, and node availability. No wallet keys or user PII in scope.
- **Trust boundaries:** (1) untrusted P2P peers ↔ node (unauthenticated, internet-facing, port
  8444); (2) RPC client ↔ node (mTLS, port 8555, *intended* to be operator-only); (3) node ↔
  SQL/mmap store; (4) Prometheus scraper ↔ /metrics (9100).
- **Actors / surface:** anonymous network attacker who can (a) open a Chia-protocol websocket and
  send crafted messages, (b) reach the RPC/metrics ports. STRIDE emphasis: **Denial of service**
  (remote panic / OOM / CPU) and **Elevation/Spoofing** (auth bypass) on the crossings.
- **Deployment:** runs in-cluster (MKE) and on bare-metal/Pi; RPC default bind `0.0.0.0:8555`,
  metrics default `0.0.0.0:9100`. Blast radius of a node compromise: node availability + local
  mempool/topology disclosure; not funds or keys.
- **Scope in:** wire deserialization, rate-limit/admission, block/tx validation panic surface,
  store SQL + mmap, RPC authn/authz + TLS, `unsafe`, secrets. **Out:** the correctness of CLVM cost
  math / BLS pairing / VDF rules (chino/knuth), and consensus-affecting fixes.

---

## Findings (worst-first)

### [MEDIUM · CVSS v4.0 6.9 · `AV:N/AC:L/AT:N/PR:N/UI:N/VC:L/VI:L/VA:L/SC:N/SI:N/SA:N`] F1 — RPC mTLS is unauthenticated by default: client-cert verifier trusts the world-public Chia CA
- **Class:** [CITED: CWE-321 Use of Hard-coded Cryptographic Key] + [CITED: CWE-295 Improper
  Certificate Validation]; OWASP [A07:2025 Authentication Failures] / [A02:2025 Security
  Misconfiguration — insecure default].
- **Source → sink trace** [DERIVED]:
  1. `full-node/src/main.rs:24` — `--rpc` defaults to `0.0.0.0:8555` (all interfaces).
  2. `full-node/src/daemon.rs:4387` → `spawn_rpc_server()` (`daemon.rs:3207`) — the RPC listener
     is started unconditionally on node run.
  3. `full-node/src/rpc.rs:1353-1363` `build_rpc_tls_context()` — when `PRIVATE_CA_CRT` /
     `PRIVATE_CA_KEY` env vars are unset (**the default**), `ca_crt`/`ca_key` fall back to the
     embedded `CHIA_CA_CRT` / `CHIA_CA_KEY`.
  4. `core/src/constants.rs:163-190` — `CHIA_CA_KEY` is a full RSA **private** key committed in
     source (and published in chia-blockchain). Confirmed by scanner, below.
  5. `full-node/src/rpc.rs:1370-1384` — the client-cert verifier's `RootCertStore` is that same
     `ca_crt`; `WebPkiClientVerifier` requires the client to present a cert that **chains to it**.
  6. **Sink:** any handler in the RPC route table (`full-node/src/rpc.rs:1563-1598`) —
     `/push_tx`, `/get_all_mempool_items`, `/get_mempool_items_by_coin_name`, `/get_connections`,
     `/get_coin_records_by_*`, block queries — runs for a caller who has satisfied client-auth.
- **Why it is a bypass:** `CHIA_CA_KEY` is world-public. Anyone can mint a client cert that chains
  to it with the repo's own `core/src/ssl.rs::generate_ca_signed_cert_data`. mTLS client-auth that
  trusts a CA whose private key is public provides **zero authentication** — the "legitimate chia
  client" and an anonymous attacker are byte-indistinguishable.
- **Proof-of-exploit** [DERIVED]:
  - Red test `full-node/tests/rpc_http.rs::tls_public_chia_ca_client_is_an_auth_bypass` — an
    attacker cert signed by the public `CHIA_CA_KEY` connects to the real `RpcServer` built by the
    default `build_rpc_tls_context()` and reaches `/healthz`. The test asserts the connection is
    **refused**; on #51 HEAD it is **accepted**, so the test FAILS:
    `test tls_public_chia_ca_client_is_an_auth_bypass ... FAILED` (builder-0, 2026-08-26).
  - Positive control (pre-existing, still passing): `tls_raw_client_with_valid_cert_succeeds` and
    `tls_e2e_chia_client_four_endpoints` — the same public-CA cert round-trips `get_blockchain_state`,
    `get_block`, `get_coin_records_by_names`, and **`push_tx`**. The exploit is already demonstrated
    by the tree's own passing tests; F1 is their security re-framing.
  - Scanner [DERIVED]: `trivy fs --scanners vuln,secret` →
    `core/src/constants.rs:163-190  HIGH: AsymmetricPrivateKey (private-key)`.
- **Severity rationale (not inflated):** exploitability is maximal (network, no privileges, no
  interaction, trivial), but impact is **bounded** — the RPC surface is read-mostly *public* chain
  data plus `push_tx` (a capability already available to anyone via the public P2P network) and
  peer/mempool disclosure (`get_connections`, mempool listing — topology/targeting + pending-tx
  visibility). No secrets, no node control, no funds. Hence Medium, not High. The insidious part is
  the *false assurance*: an operator who sees "mTLS, client cert REQUIRED" will reasonably bind the
  RPC to `0.0.0.0` believing it authenticated. (CVSS 6.9 is my computed estimate from the vector;
  the vector string is the authoritative artifact.)
- **Remediation (routed to Grant — TLS trust-model decision, not fixed in #52):**
  1. Do **not** fall back to the public Chia CA for RPC **client-auth**. Require an explicit
     per-install private CA (`PRIVATE_CA_CRT`/`PRIVATE_CA_KEY`); if absent, either generate a
     unique private CA on first run (chia's `private_ca` posture) or **refuse to start client-auth**
     and fail closed. The public CA remains correct for the *P2P* listener (8444) only.
  2. Default `--rpc` to `127.0.0.1:8555` (loopback); make `0.0.0.0` an explicit opt-in.
  3. Root-cause the class [SSDF RV.3]: the P2P listener and the RPC listener share one CA-selection
     helper; split them so a public-CA trust anchor can never back an interface that is meant to
     authenticate. Add a startup assertion + a regression test (this red test) that the RPC verifier
     root is never the public CA.

---

## Hardening / defense-in-depth (not independently exploitable)

- **H1 — Prometheus `/metrics` unauthenticated on `0.0.0.0:9100` by default**
  (`full-node/src/main.rs:43`). [CITED: CWE-200 Exposure of Sensitive Information] / A02:2025.
  Leaks node internals (heights, peer counts, store/telemetry gauges) to anyone on the network.
  Common practice, but pair with a NetworkPolicy / loopback bind. Low.
- **H2 — `DG_XCH_RPC_ALLOW_ANY_CLIENT_CERT=1` disables client-cert verification entirely**
  (`full-node/src/rpc.rs:1376-1384`, `AllowAny`). Off by default and operator-opt-in, so a
  documented risk toggle rather than a vuln — but it compounds F1 (with F1 unfixed, the "secure"
  path is already effectively open). Note in ops docs; consider removing once F1 is fixed.
- **H3 — `tarpaulin-report.html` (10 MB) committed to the repo** embeds the public CA key twice
  (trivy flagged both) and bloats the tree. Repo hygiene / A02; remove from VCS and `.gitignore`
  it. Low.

---

## Coverage / tools run (areas audited — legible map)

**[DERIVED] scanners:** `trivy fs --scanners vuln,secret --severity HIGH,CRITICAL .` →
`gui/package-lock.json` 0 vulns; 1 secret (the public Chia CA key, F1 context) + 2 in the coverage
HTML (H3). No dependency CVEs surfaced (no `Cargo.lock` committed → SCA is source-tree only; a
locked SCA pass in CI is a coverage gap to close). `gitleaks`/`semgrep` not installed on the host.

**Wire / deserialization — no finding.** `serialize/src/lib.rs`: `String`/`Vec<T>`/`HashMap`
decoders start empty and grow (no pre-alloc from an untrusted length; #180 sibling check clean),
`String` has a remaining-bytes guard before alloc, primitives/arrays are fixed-size with
remaining-bytes guards, `parse_vec_limited` caps item count + O(1)-skips fixed-size tails
(CHIA-4203), `decode_size` bounds atoms to `MAX_DECODE_SIZE` and the CLVM parser is **iterative**
(explicit `op_buf` stack — no recursion) with a `remaining < blob_size` guard before every alloc.
Message framing capped at 64 MiB (`servers/src/websocket/mod.rs:337`); the envelope decode keeps
`data` as raw bytes and the expensive inner decode happens **after** the rate limiter charges
(`core/src/protocols/mod.rs:940,978,1024`), so per-message parse cost is bounded and gated.
`UnsizedBytes`/`record_compat` decode via safe `from_bytes` with exact-fit gating.

**Rate limiting / admission — no finding.** RATE_LIMITS_V3 window logic
(`core/src/protocols/rate_limits_v3.rs`) is count-checked (`recv_acquire` rejects at `>= w`),
saturating on release, `ConfigureWindowSizes` rejects a peer trying to bound our unlimited types,
lock-poison-tolerant; unknown message types disconnect + short-ban before dispatch
(`core/src/protocols/mod.rs:990`); address book is capacity-bounded + deduped + self-filtered
(`p2p/src/address_manager.rs`).

**Block/tx validation panic surface — no finding.** Traced every candidate index/`unwrap`/subtract
on peer data: `engine.rs:2307` (`finished_sub_slots[0]`) guarded by a prior `find_map` (non-empty);
`engine.rs:1560` (`removed - added`) guarded by `removed < added`; `slots.rs:810`
(`sps[index]`) guarded by `index < num_sps_sub_slot` at `slots.rs:628`; `header.rs:203,319`
(`queue[0]`) guarded by `len()==1`; the residual `expect(...)` are test-only or one-time
metric/lock init. Malicious-generator DoS (OOM export + owned-`SExp` `Drop` stack overflow) was the
pre-existing DIVERGENCE-51 vuln — already fixed (arena-native, incrementally cost-bounded condition
parse) with **all 10** vectors active in `core/tests/malicious_generators.rs` (0 `#[ignore]`).

**Store layer — no finding.** Postgres + SQLite coin/block queries are parameterized; the `format!`
sites (`stores/src/postgres/coin.rs:118,253,300,307`, sqlite siblings) interpolate only
`$n` placeholder strings and clause fragments chosen from **fixed literals** (`spent_clause`,
`height_filter`) — every user value goes through `.bind()`. No SQL injection. mmap offsets are
internally derived (`record_off`/`record_count`), reads go through bounds-checked `read_at`.

**BLS FFI `unsafe` — no finding (memory safety).** `more_ops.rs` / `bls_ops.rs` point parses take
fixed-size `&[u8;48]`/`&[u8;96]`; call sites convert untrusted atoms via `try_into` or a
`len()==48` guard (`more_ops.rs:1166`) — no OOB slice. SAFETY invariants documented; points
subgroup-checked. (Crypto *validity* correctness = knuth's call, not audited here.)

**RPC — F1 above; otherwise coin queries are `LIMIT`/`max_items`-bounded, body capped at
`MAX_RPC_BODY_BYTES` (413 over), TLS floored at 1.3, error envelope leaks no stack traces.**

## Residual risk & [UNVERIFIED]
- **Owned `SExp` `Drop` recursion** (`core/src/clvm/sexp.rs` `PairBuf::Owned(Arc<SExp>)`): dropping
  a deeply-nested owned tree recurses and can overflow the native stack. In the node this is **not**
  reachable from untrusted input — the untrusted generator/spend path is arena-native (SerializedProgram
  → arena VM), and DIVERGENCE-51 specifically removed the owned-`SExp` materialization that once made
  it reachable. Marked [UNVERIFIED]/theoretical. *Proof required to promote it:* find a peer-reachable
  path that calls `Program::from_bytes`/`sexp_from_bytes` on an unbounded peer blob, then a test that
  decodes `ff`×N + `80` and drops it, observing SIGABRT/stack overflow.
- **SCA coverage gap:** no committed `Cargo.lock` → no reproducible dependency-CVE gate. Add a
  locked `cargo audit`/`trivy` step in CI (SSDF PW.7 / A03:2025).

---

## F1 remediation — SHIPPED in this PR (config option, CNI-compatible)

Grant reviewed F1 and directed a CNI-parity fix exposed as a config option. Implemented and verified
on this branch:

- **New `--rpc-tls <cni|local>` flag** (`full-node/src/main.rs`, `full-node/src/config.rs`
  `RpcTlsMode`), default **`cni`**:
  - **`cni`** — CNI-compatible mutual TLS. The RPC client-cert verifier is rooted at a **per-install
    private CA** (chia `private_ssl_ca`, `chia/rpc/rpc_server.py:179-182`), taken from
    `PRIVATE_CA_CRT`/`PRIVATE_CA_KEY` if set, else loaded from — or generated once and persisted into
    — `<--ssl-dir>/ca/private_ca.{crt,key}` (key chmod 600). The served cert is signed by it. The
    world-public Chia CA is **rejected outright** as a client-auth anchor (explicit guard in
    `build_cni_rpc_tls`), so F1 cannot recur even if someone points `PRIVATE_CA_CRT` at it.
  - **`local`** — for operators running privately who "don't want these certs in the mix": server-only
    TLS with an ephemeral in-memory cert and **no client-cert requirement**, permitted on a
    **loopback `--rpc` bind only** — `build_local_rpc_tls` returns an error (fail closed) on a routable
    address, so an unauthenticated RPC can never be network-exposed.
- **Removed** the public-CA fallback and the `DG_XCH_RPC_ALLOW_ANY_CLIENT_CERT` accept-anything env
  toggle (H2) — both folded into the two explicit, safe modes.
- The world-public `CHIA_CA_*` remains the correct anchor for the **P2P** listener (8444) only.

**Verification** [DERIVED] (builder-0, `cargo test -p full-node`): `rpc_http` 17/17 green, including
`tls_public_chia_ca_client_is_an_auth_bypass` (the F1 attacker cert is now REFUSED — the ex-red test,
now a regression guard), `tls_private_ca_client_is_accepted` + `tls_e2e_chia_client_four_endpoints`
(private-CA mTLS round-trips every endpoint incl. `push_tx`), `tls_local_mode_allows_no_client_cert_on_loopback`,
and `tls_local_mode_refuses_non_loopback_bind` (fail-closed). `integration` 1/1 green (full daemon
boot + RPC over Local-mode TLS). `cargo clippy -p full-node` clean.

**Operator migration note:** nodes now start in `cni` mode by default and will auto-generate a private
CA under `<--ssl-dir>/ca` on first run; RPC tooling must present a cert signed by that private CA
(distribute `private_ca.crt`, sign client certs with the key) — the old public-CA client certs are
rejected (by design). A purely local node can instead run `--rpc 127.0.0.1:8555 --rpc-tls local`.

---

# pr-52 follow-on hardening (Grant-directed)

## Local-default RPC TLS (non-breaking)
`--rpc-tls` defaults to **`local`** (cni is the explicit opt-in), so existing public-CA RPC clients
are not broken. Non-breaking on nodes that pass `--rpc 0.0.0.0`: `--rpc` bind default flips to
`127.0.0.1:8555`, and `RpcTlsMode::resolve_bind` **downgrades** a routable local-mode bind to
loopback (port preserved) with a loud WARN — chosen over a hard-fail because it (a) never
network-exposes an unauthenticated RPC and (b) never crashes a previously-working node. `cni` stays
the CNI-parity authenticated path. Commit `e621576`. Tests: `local_mode_downgrades_routable_bind_to_loopback`
+ the full `rpc_http` suite green.

## H1 — metrics loopback default + /debug/heap gate (commit `f97dbf7`)
`--metrics` default `0.0.0.0:9100` → `127.0.0.1:9100`; new `--debug-endpoints` (off) gates the
`/debug/heap` jemalloc dump (can leak in-memory data + writes a file) — 404 unless enabled. CWE-200.

## H3 — coverage artifact removed (commit `4f8e6e8`)
`tarpaulin-report.html` (10 MB, embedded the public CA key twice) removed from VCS; coverage outputs
`.gitignore`d.

## U2 — Cargo.lock + cargo audit gate (commit `c01cac3`)
`Cargo.lock` committed (734 deps); a `Dependency Audit` CI job runs `cargo audit` (RustSec), failing
on any NEW advisory. `.cargo/audit.toml` documents the reviewed baseline. Beyond ~19
unmaintained/unsound INFORMATIONAL advisories in GUI (gtk-rs/glib via Tauri), i18n (unic-*), and
build-macro transitive deps, three REAL advisories surfaced — all risk-accepted with reachability
justifications (below), none reachable from the node's untrusted attack surface:

### [LOW · dep] F2 — `rsa` Marvin timing attack
- [CITED: RUSTSEC-2026-0195] · [CITED: CWE-208 Observable Timing Discrepancy]
- `rsa` (`core/src/ssl.rs`) is used ONLY for offline, startup-time CA/leaf cert GENERATION and
  PKCS#1v1.5 SIGNING — no attacker-facing RSA decryption/signing oracle whose timing is measurable
  (TLS crypto is rustls/ring, not `rsa`) [DERIVED: `grep rsa:: → ssl.rs only`]. No upstream fix.
  Action: bump `rsa` and drop the ignore when a fix ships.

### [LOW · dep] F3 — `protobuf` uncontrolled-recursion crash
- [CITED: RUSTSEC-2024-0437] · [CITED: CWE-674 Uncontrolled Recursion]
- Transitive `prometheus → portfu → dg_xch_core` [DERIVED: `cargo tree -i`]. Used only to ENCODE the
  `/metrics` exposition format; the vulnerable protobuf DECODE path is never fed untrusted input.
  Fix (protobuf ≥3.7.2) needs a `prometheus` major bump (upstream). Action: modernize the metrics dep.

### [MEDIUM · dep, not reachable] F4 — `quick-xml` quadratic-parse DoS
- [CITED: RUSTSEC-2026-0194, base 7.5] · [CITED: CWE-1333 Inefficient Regular/Parse Complexity]
- Transitive `pprof → inferno → full-node` [DERIVED: `cargo tree -i`]. Used only to WRITE the
  flamegraph SVG; the vulnerable XML PARSE path is never fed untrusted input. Fix (quick-xml ≥0.41)
  needs an `inferno`/`pprof` bump (upstream). Action: modernize the profiler dep.

## U1 — owned `SExp` recursive `Drop` stack overflow (LATENT; routed to chino)
- Class: [CITED: CWE-674 Uncontrolled Recursion] (remote-DoS shape). **Latent** — NOT reachable from
  untrusted node input today: the block/tx path is arena-native (`SerializedProgram` → arena VM) and
  DIVERGENCE-51 removed owned-`SExp` materialization. Rated informational-until-reachable.
- Proof [DERIVED]: `core/tests/sexp_drop_stack_safety.rs::deep_owned_sexp_drops_without_stack_overflow`
  builds a 500k-deep `PairBuf::Owned(Arc<SExp>)` chain and drops it on a 1 MiB stack → **SIGABRT
  "stack overflow, aborting"** on pr-52 HEAD (verified builder-0). `#[ignore]`d as the reproduction +
  regression guard (commits `ff56a35`, `75e547a`).
- **Parity verdict — DIVERGENCE from chia/clvm_rs, confirmed by source AND the iriga oracle:**
  - Source: clvm_rs represents CLVM values as `NodePtr` indices into a flat arena
    (`Allocator { u8_vec, atom_vec, pair_vec }`); our `core/src/clvm/arena.rs` is the cited port of
    clvm_rs 0.17.7. There is no deep owned recursive value — deserialize/tree-hash are iterative and
    teardown is a flat `Vec` free (O(1) stack). The owned `SExp`/`PairBuf::Owned(Arc<SExp>)` is a
    dg_xch-specific SECOND representation whose `Drop` recurses — the divergence.
  - iriga oracle [DERIVED]: on `iriga/chia-mainnet-0` (official chia, `chia-enhanced-node`), chia's
    `chia_rs.Program.from_bytes` deserialized the SAME valid 500k-deep structure, ran `get_tree_hash`,
    and dropped it — all without a stack overflow ("PROBE_COMPLETE: chia handled the deep CLVM without
    SIGABRT"); the node pod stayed `2/2 Running`. The probe ran in a throwaway subprocess, never the
    live node process or chain state.
- **Fix routed to chino** (CLVM-core): the naive `impl Drop for SExp` does NOT compile — `SExp`
  carries `const NULL_SEXP`/`ONE_SEXP` used as `&'static`, and a `Drop` type cannot be const-promoted,
  cascading into `from_bool`, `SExpIter`, and `Program::new_const` (E0509/E0515/E0716). A correct
  iterative teardown needs a CLVM-core change (const→static + `Program` rework, or a bounded parse
  depth matching clvm_rs) — outside a security-clean change. The `#[ignore]`d red test turns green
  when it lands.

## Fleet manifests for Grant's GitOps follow-up (NOT changed here)
Default changes don't touch nodes that set these flags explicitly, so these keep working — but they
now warrant follow-up:

**Pass `--rpc 0.0.0.0:8555` (RPC on all interfaces).** Under the new `local` default these keep an
authenticated posture ONLY if switched to `--rpc-tls cni` (else the RPC downgrades to loopback with a
WARN and cross-pod RPC clients lose access). Add `--rpc-tls cni` (+ distribute the private CA) where
network RPC is needed:
- `applications/mke/dg-xch-node/statefulset.yaml:41`
- `applications/mke/dg-xch-node-pg/statefulset.yaml:59`
- `applications/mke/dg-xch-node-pg/statefulset-b.yaml:50`
- `applications/mke/dg-xch-node-pg/statefulset-epyc.yaml:87`
- `applications/mke/dg-xch-node-pg/statefulset-mm.yaml:46`

**Pass `--metrics=0.0.0.0:9100` (metrics on all interfaces).** Unaffected by the default flip; for
defense-in-depth pair with a pod-IP bind + NetworkPolicy (and keep `--debug-endpoints` OFF):
- `applications/mke/dg-xch-node/statefulset.yaml:64`
- `applications/mke/dg-xch-node-pg/statefulset.yaml:71`
- `applications/mke/dg-xch-node-pg/statefulset-b.yaml:62`
- `applications/mke/dg-xch-node-pg/statefulset-epyc.yaml:98`
- `applications/mke/dg-xch-node-pg/statefulset-mm.yaml:58`
