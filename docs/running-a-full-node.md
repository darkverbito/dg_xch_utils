# Running a dg_xch Full Node

## Build

Install stable Rust, `cmake`, a C compiler, and the system zstd development package. Then build the node:

```bash
cargo build --release -p full-node --features sqlite,coin-index,hint
```

The binary is written to `target/release/full-node`.

## Start

```bash
mkdir -p "$HOME/dg-xch-data"
./target/release/full-node \
  --listen 0.0.0.0:8444 \
  --rpc 127.0.0.1:8555 \
  --db "sqlite://$HOME/dg-xch-data/chain.db" \
  --network mainnet \
  --metrics 127.0.0.1:9100
```

Use one or more `--peer host:port` options or an `--introducer host:port` to establish outbound connections. Use `--advertise ip:port` only when the listener is reachable from the public network.

## RPC TLS

The default `--rpc-tls local` mode requires the RPC listener to use a loopback address. Use `--rpc-tls private-ca --ssl-dir <directory>` for authenticated remote RPC. The directory must contain `ca/private_ca.crt` and `ca/private_ca.key`; public network certificates are not accepted as RPC client-authentication roots.

## Peer Settings

The full-node command exposes every `P2pSettings` value:

- `--target-outbound` and `--target-peer-count`
- `--host-pool-capacity`, `--address-lower`, and `--address-upper`
- `--connect-timeout-secs`, `--handshake-timeout-secs`, and `--retry-timeout-secs`
- `--heartbeat-secs`, `--pong-deadline-secs`, and `--recent-peer-threshold-secs`
- `--jitter-floor`

Invalid combinations are rejected during startup. In particular, outbound peers cannot exceed total peers, address bounds must fit within the host pool, durations must be nonzero, and jitter must be between `0.0` and `1.0`.

## Storage

Supported database URLs are:

- `sqlite://<path>` for the embedded default
- `postgres://<connection-string>` with the `postgres` feature
- `mmap://<directory>` with the `mmap` feature

The mmap directory must be writable by the node process.

## Sync Modes

- The default mode validates a weight proof and then fully validates forward from its checkpoint.
- `--genesis-sync` validates the chain from height zero.
- `--sync-from <height>` selects an explicit validated starting height.

## Monitoring

Set `--metrics host:port` to serve Prometheus metrics or `--metrics off` to disable them. See [monitoring.md](monitoring.md) for the metric names and scrape configuration.
