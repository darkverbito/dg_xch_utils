use clap::Parser;
use full_node::{Config, Node};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

// jemalloc as the global allocator. glibc malloc grows one 64MB arena per contending thread
// (up to 8x cores) and never returns freed non-main-arena pages to the OS — under the
// per-window body-precompute thread churn the syncing nodes climbed ~GiB/hour of freed-but-
// held heap until the container OOMed. jemalloc bounds arenas and background-purges
// dirty pages back to the OS; fullnode_alloc_{allocated,resident}_bytes expose its view so
// allocator holdback and true retention stay distinguishable on the dashboards.
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

// dg_xch full-node daemon. Honors the documented CLI flags; env conventions FULLNODE_HOST / FULLNODE_PORT.
// No network-specific env vars — network identity enters only via ConsensusConstants + --network.
#[derive(Parser, Debug)]
#[command(name = "full-node", about = "dg_xch validating full node")]
struct Cli {
    /// Chia P2P listen address (peers reach us here).
    #[arg(long, default_value = "0.0.0.0:8444")]
    listen: String,
    /// Local RPC listen address.
    #[arg(long, default_value = "127.0.0.1:8555")]
    rpc: String,
    /// RPC client-auth posture: `local` (default) = no client certs, loopback-only (a routable
    /// --rpc bind is downgraded to loopback with a warning); `cni` = CNI-compatible mutual TLS
    /// against a per-install private CA (auto-generated under --ssl-dir if absent) for an
    /// authenticated network RPC. The world-public Chia CA is never a client-auth anchor in either.
    #[arg(long = "rpc-tls", default_value = "local")]
    rpc_tls: String,
    /// Directory holding the RPC private CA for `--rpc-tls cni` (`<ssl-dir>/ca/private_ca.{crt,key}`,
    /// generated once and persisted). Distribute the crt to RPC tooling; keep the key node-private.
    #[arg(long = "ssl-dir", default_value = "ssl")]
    ssl_dir: String,
    /// Seed introducer host:port for peer bootstrap.
    #[arg(long)]
    introducer: Option<String>,
    /// Manual peer host:port to dial directly, repeatable. Bypasses the introducer for these peers and
    /// re-dials them if dropped; point at trusted, fast full nodes to bootstrap without the public introducer.
    #[arg(long = "peer")]
    peer: Vec<String>,
    /// External WAN address advertised for peer gossip behind NAT.
    #[arg(long)]
    advertise: Option<String>,
    /// Storage backend URL: sqlite://<path> or postgres://…
    #[arg(long, default_value = "sqlite:///data/chain.db")]
    db: String,
    /// Network id selecting the consensus constants.
    #[arg(long, default_value = "mainnet")]
    network: String,
    /// Prometheus /metrics listen address, or `off` to disable. Default on.
    #[arg(long, default_value = "127.0.0.1:9100")]
    metrics: String,
    /// Debug: directory to capture real sync data into (weight proof + downloaded block ranges) for
    /// offline replay/profiling. Off unless set.
    #[arg(long)]
    capture_dir: Option<String>,
    /// Validate the historical chain block by block from height 0 (disables weight-proof fast sync).
    #[arg(long, default_value_t = false)]
    genesis_sync: bool,
    /// Anchor mid-chain at this height and validate forward from there (0 = off). Lets several
    /// nodes each fully validate a disjoint chain segment in parallel.
    #[arg(long, default_value_t = 0)]
    sync_from: u32,
    /// Enable the compact-VDF (bluebox) solicitation scan. OFF by default, like chia's
    /// `send_uncompact_interval: 0`. The serve + consume halves run unconditionally; this only
    /// turns on the background scan for bulky proofs (no effect without bluebox timelord peers).
    #[arg(long, default_value_t = false)]
    uncompact: bool,
    /// RAM budget (MiB) for the sync window readahead's resident block bodies. Unset = the shipped
    /// default (256 MiB, adaptive lookahead ≤ 8 windows, one in-flight fetch per peer) — no change for
    /// existing nodes. Set it on a large-RAM, fetch-starved node (cores idle waiting on blocks) to
    /// prefetch aggressively: the budget is a HARD resident ceiling that drives a deeper lookahead AND
    /// raises the aggregate in-flight fetch concurrency (spread across peers) so the CPU, not the
    /// network, becomes the bottleneck. OOM-safe — a huge budget still collapses depth at large blocks.
    #[arg(long)]
    prefetch_memory_mb: Option<u64>,
    /// Optional cap on the aggregate outstanding block-range requests (the concurrency count, distinct
    /// from --prefetch-memory-mb's byte budget). Unset = derive from the anti-flood ceiling. Spread
    /// across peers as ceil(N / peers) per connection.
    #[arg(long)]
    prefetch_max_inflight: Option<usize>,
    /// Cert-hash node id (64 hex chars) granted the TRUSTED tier, repeatable — chia's `trusted_peers`.
    /// A trusted peer gets the larger subscription (2,000,000) and response-item (500,000) caps and
    /// high-priority transaction-queue placement. Empty (default) = every peer untrusted.
    #[arg(long = "trusted-peer")]
    trusted_peer: Vec<String>,
    /// CIDR network (IPv4 or IPv6, e.g. 10.0.0.0/8) whose peers are granted the TRUSTED tier by
    /// remote IP, repeatable — chia's `trusted_cidrs`. A peer whose host falls in any of these
    /// networks gets the trusted caps + tx priority. Malformed entries are skipped. (Localhost is
    /// auto-trusted regardless.)
    #[arg(long = "trusted-cidr")]
    trusted_cidr: Vec<String>,
    /// Expose /debug/heap (jemalloc heap dump) on the metrics port. OFF by default — it can leak
    /// in-memory data and writes a file to disk. /metrics and /health are always served.
    #[arg(long = "debug-endpoints", default_value_t = false)]
    debug_endpoints: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        // Span-close events carry busy/idle durations — per-block (block.apply) and per-window
        // (sync.short) timings land in the logs with zero extra plumbing.
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .json()
        .init();

    let cli = Cli::parse();
    let mut config = Config::build(
        &cli.listen,
        &cli.rpc,
        cli.introducer.as_deref(),
        &cli.peer,
        cli.advertise.as_deref(),
        &cli.db,
        &cli.network,
        &cli.metrics,
        cli.capture_dir.as_deref(),
        cli.genesis_sync,
        cli.sync_from,
        cli.uncompact,
        cli.prefetch_memory_mb,
        cli.prefetch_max_inflight,
        &cli.trusted_peer,
        &cli.trusted_cidr,
    )
    .map_err(std::io::Error::other)?;

    // Finding F1 remediation: the RPC client-auth trust anchor is chosen here (default CNI
    // private-CA mTLS), never the world-public Chia CA.
    config.rpc_tls =
        full_node::RpcTlsMode::parse(&cli.rpc_tls, &cli.ssl_dir).map_err(std::io::Error::other)?;
    config.debug_endpoints = cli.debug_endpoints;

    match config.backend.clone() {
        full_node::config::Backend::Sqlite(_) => {
            let node = Arc::new(Node::boot(config).await?);
            node.run().await?;
        }
        #[cfg(feature = "postgres")]
        full_node::config::Backend::Postgres(url) => {
            let store = Arc::new(
                dg_xch_stores::PostgresStore::open(&url)
                    .await
                    .map_err(|e| std::io::Error::other(format!("open postgres: {e}")))?,
            );
            let node = Arc::new(Node::boot_with_store(config, store)?);
            node.run().await?;
        }
        #[cfg(not(feature = "postgres"))]
        full_node::config::Backend::Postgres(url) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!(
                    "postgres:// --db ({url}) requires a binary built with --features postgres"
                ),
            )
            .into());
        }
        #[cfg(feature = "mmap")]
        full_node::config::Backend::Mmap(dir) => {
            let store = Arc::new(
                dg_xch_stores::MmapStore::open(&dir)
                    .await
                    .map_err(|e| std::io::Error::other(format!("open mmap store: {e}")))?,
            );
            let node = Arc::new(Node::boot_with_store(config, store)?);
            node.run().await?;
        }
        #[cfg(not(feature = "mmap"))]
        full_node::config::Backend::Mmap(dir) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!(
                    "mmap:// --db ({}) requires a binary built with --features mmap",
                    dir.display()
                ),
            )
            .into());
        }
    }
    Ok(())
}
