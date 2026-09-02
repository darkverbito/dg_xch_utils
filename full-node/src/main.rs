use clap::Parser;
use dg_logger::DruidGardenLoggerBuilder;
use dg_xch_p2p::P2pSettings;
use full_node::{Config, Node};
use log::Level;
use std::sync::Arc;
use std::time::Duration;

// jemalloc as the global allocator. glibc malloc grows one 64MB arena per contending thread
// (up to 8x cores) and never returns freed non-main-arena pages to the OS — under the
// per-window body-precompute thread churn the syncing nodes climbed ~GiB/hour of freed-but-
// held heap until the container OOMed. jemalloc bounds arenas and background-purges
// dirty pages back to the OS; fullnode_alloc_{allocated,resident}_bytes expose its view so
// allocator holdback and true retention stay distinguishable on the dashboards.
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[derive(Parser, Debug)]
#[command(name = "full-node", about = "dg_xch validating full node")]
struct Cli {
    #[arg(long, default_value = "0.0.0.0:8444")]
    listen: String,
    /// Local RPC listen address.
    #[arg(long, default_value = "127.0.0.1:8555")]
    rpc: String,
    #[arg(long = "rpc-tls", default_value = "local")]
    rpc_tls: String,
    /// Directory holding the RPC private CA for `--rpc-tls private-ca` (`<ssl-dir>/ca/private_ca.{crt,key}`,
    /// generated once and persisted). Distribute the crt to RPC tooling; keep the key node-private.
    #[arg(long = "ssl-dir", default_value = "ssl")]
    ssl_dir: String,
    #[arg(long)]
    introducer: Option<String>,
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
    /// Outbound connections to keep open. Unset = the built-in default.
    #[arg(long)]
    target_outbound: Option<usize>,
    /// Total peers (inbound + outbound) to accept. Unset = the built-in default.
    #[arg(long)]
    target_peer_count: Option<usize>,
    /// Maximum addresses retained for future outbound connections.
    #[arg(long, default_value_t = 1_000)]
    host_pool_capacity: usize,
    /// Refill the address pool when it drops below this count.
    #[arg(long, default_value_t = 5)]
    address_lower: usize,
    /// Stop requesting addresses when the pool reaches this count.
    #[arg(long, default_value_t = 10)]
    address_upper: usize,
    #[arg(long, default_value_t = 30)]
    connect_timeout_secs: u64,
    #[arg(long, default_value_t = 15)]
    handshake_timeout_secs: u64,
    #[arg(long, default_value_t = 1)]
    retry_timeout_secs: u64,
    #[arg(long, default_value_t = 120)]
    heartbeat_secs: u64,
    #[arg(long, default_value_t = 30)]
    pong_deadline_secs: u64,
    #[arg(long, default_value_t = 6_000)]
    recent_peer_threshold_secs: u64,
    /// Lower bound for randomized reconnect backoff, from 0 through 1.
    #[arg(long, default_value_t = 0.5)]
    jitter_floor: f64,
    #[arg(long = "trusted-peer")]
    trusted_peer: Vec<String>,
    /// CIDR network (IPv4 or IPv6, e.g. 10.0.0.0/8) whose peers are granted the TRUSTED tier by
    /// remote IP, repeatable. A peer whose host falls in any of these
    /// networks gets the trusted caps + tx priority. Malformed entries are skipped. (Localhost is
    /// auto-trusted regardless.)
    #[arg(long = "trusted-cidr")]
    trusted_cidr: Vec<String>,
    #[arg(long = "debug-endpoints", default_value_t = false)]
    debug_endpoints: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|v| v.parse::<Level>().ok())
        .unwrap_or(Level::Info);
    let logger = DruidGardenLoggerBuilder::new()
        .current_level(level)
        .init()
        .map_err(|e| format!("failed to initialize logger: {e}"))?;

    let cli = Cli::parse();
    let defaults = P2pSettings::default();
    let p2p = P2pSettings {
        target_outbound: cli.target_outbound.unwrap_or(defaults.target_outbound),
        target_peer_count: cli.target_peer_count.unwrap_or(defaults.target_peer_count),
        host_pool_capacity: cli.host_pool_capacity,
        address_lower: cli.address_lower,
        address_upper: cli.address_upper,
        connect_timeout: Duration::from_secs(cli.connect_timeout_secs),
        handshake_timeout: Duration::from_secs(cli.handshake_timeout_secs),
        retry_timeout: Duration::from_secs(cli.retry_timeout_secs),
        heartbeat: Duration::from_secs(cli.heartbeat_secs),
        pong_deadline: Duration::from_secs(cli.pong_deadline_secs),
        recent_peer_threshold: Duration::from_secs(cli.recent_peer_threshold_secs),
        jitter_floor: cli.jitter_floor,
    };
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
        p2p,
        &cli.trusted_peer,
        &cli.trusted_cidr,
    )
    .map_err(std::io::Error::other)?;

    // Select the RPC client-auth trust anchor after parsing the base configuration.
    config.rpc_tls =
        full_node::RpcTlsMode::parse(&cli.rpc_tls, &cli.ssl_dir).map_err(std::io::Error::other)?;
    config.debug_endpoints = cli.debug_endpoints;

    // Boot the concrete backend, bring up the protocol servers, and wrap both in
    // the type-erased handles the portfu constructs dispatch through.
    macro_rules! activate {
        ($variant:ident, $node:expr) => {{
            let node = $node;
            let services = node.start_services().await?;
            let sources = Arc::new(node.metrics_sources(&services));
            (
                full_node::service::ActiveNode::$variant(full_node::service::BackendHandle {
                    node,
                    sources,
                }),
                services,
            )
        }};
    }
    let metrics_bind = config.metrics;
    let (active, services) = match config.backend.clone() {
        full_node::config::Backend::Sqlite(_) => {
            activate!(Sqlite, Arc::new(Node::boot(config).await?))
        }
        #[cfg(feature = "postgres")]
        full_node::config::Backend::Postgres(url) => {
            let store = Arc::new(
                dg_xch_stores::PostgresStore::open(&url)
                    .await
                    .map_err(|e| std::io::Error::other(format!("open postgres: {e}")))?,
            );
            activate!(Postgres, Arc::new(Node::boot_with_store(config, store)?))
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
            activate!(Mmap, Arc::new(Node::boot_with_store(config, store)?))
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
    };

    // The portfu server hosts /metrics, /health, and the daemon's tasks and
    // intervals (annotation-registered). Its bind is the old --metrics address;
    // `--metrics off` keeps the process surface loopback-only.
    let (host, port) = match metrics_bind {
        Some(addr) => (addr.ip().to_string(), addr.port()),
        None => ("127.0.0.1".to_string(), 0),
    };
    log::info!(
        "portfu server hosting metrics/health/tasks backend={} bind={host}:{port}",
        active.backend_name()
    );
    let active = Arc::new(active);
    let services = Arc::new(services);
    // A second interrupt forces exit. The graceful path can sit inside a long network await
    // (a weight-proof race carries a 120s deadline, and a stalled sync loops through them), so
    // the first Ctrl-C starts the drain and a second one means NOW — the standard daemon
    // contract, without which a wedged sync reads as an unkillable process.
    tokio::spawn(async {
        let _ = tokio::signal::ctrl_c().await;
        let _ = tokio::signal::ctrl_c().await;
        log::warn!("second interrupt — forcing exit without drain");
        std::process::exit(130);
    });
    let server = portfu::prelude::ServerBuilder::new()
        .host(host)
        .port(port)
        .global_state::<full_node::service::ActiveNode>(active.clone())
        .global_state::<full_node::service::NodeServices>(services.clone())
        .global_state::<dg_logger::DruidGardenLogger>(logger)
        .build();
    let result = server.run().await;
    // Server exited (signal): the shutdown bridge has flipped the node's run
    // flag; stop the protocol servers and the supervisor.
    services.drain().await;
    result.map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(())
}
