# Portfu 2 Remediation Plan

This work starts after the PR 49-57 remediation is merged and must finish before the updated crates are published.

## Sequence

1. Split `full-node/src/daemon.rs` into behavior-preserving service modules with explicit ownership for startup, synchronization, peer management, wallet requests, and shutdown.
2. Split the RPC and P2P handler modules by protocol area, without changing message routing or error behavior.
3. Pin every workspace package to the same Portfu 2 revision and remove all Portfu 1.x dependencies.
4. Replace direct Tokio task spawning with Portfu task ownership and cancellation.
5. Replace local health, metrics, and cache lifecycle code where Portfu 2 provides the required behavior.
6. Migrate RPC listener lifecycle to Portfu while retaining authentication, TLS, and response compatibility.
7. Migrate P2P listener and peer lifecycle only after equivalent connection limits, timeouts, bans, and graceful shutdown are covered by integration tests.
8. Publish or select a stable Portfu 2 crate release, then replace git dependencies with versioned dependencies before publishing this workspace.

## Acceptance Criteria

- One Portfu major version and source is present in `Cargo.lock`.
- Every long-lived task has a Portfu owner, cancellation path, and shutdown test.
- RPC and P2P interoperability tests pass without listener or lifecycle regressions.
- No published crate depends on a mutable git branch or workspace-only path.
