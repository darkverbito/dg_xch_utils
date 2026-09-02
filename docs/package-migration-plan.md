# Package Migration Plan

This migration is deferred until the PR 49-57 remediation is merged. It must preserve public APIs and runtime behavior at each step.

## Target Ownership

- `cli` owns executable command dispatch, including full-node startup and coin-root derivation.
- `servers` owns RPC and protocol handler routing.
- `node` owns chain state, synchronization, peer policy, and validation.
- `stores` owns persistence implementations and storage traits.
- `p2p` owns transport, sessions, peer connections, and wire-level limits.

## Sequence

1. Add full-node and coin-root subcommands to `cli` without removing the existing binaries.
2. Move reusable root derivation code into the closest existing library package and keep only command parsing in `cli`.
3. Move handler registration and routing from `p2p/src/handlers.rs` into `servers`, leaving transport-facing adapters in `p2p`.
4. Switch integration tests to the new command and routing entry points.
5. Deprecate the standalone `full-node` and `roots` binaries for one release.
6. Remove the redundant crates after downstream users have migrated.

## Acceptance Criteria

- Workspace dependency direction follows the target ownership without cycles.
- Existing configuration files and command options remain compatible during the deprecation release.
- Protocol, synchronization, persistence, and CLI integration tests pass through the new entry points.
- Published packages contain no path dependencies on removed workspace crates.
