use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

// Which storage backend the `--db` URL selects. SQLite is the embedded Pi-floor backend (the only one built
// today); Postgres is the industrial scale-out backend, whose sqlx implementation is a dg_xch_stores concern
// not yet landed — a `postgres://` URL is accepted here but rejected at open with a clear error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Backend {
    Sqlite(PathBuf),
    Postgres(String),
    Mmap(PathBuf),
}

impl Backend {
    /// Parse a `--db` value: `sqlite://<path>` / `sqlite:<path>` → SQLite, `postgres://…` /
    /// `postgresql://…` → Postgres, a bare path → SQLite.
    #[must_use]
    pub fn parse(db: &str) -> Self {
        if let Some(rest) = db.strip_prefix("mmap://") {
            Backend::Mmap(PathBuf::from(rest))
        } else if let Some(rest) = db.strip_prefix("sqlite://") {
            Backend::Sqlite(PathBuf::from(rest))
        } else if let Some(rest) = db.strip_prefix("sqlite:") {
            Backend::Sqlite(PathBuf::from(rest))
        } else if db.starts_with("postgres://") || db.starts_with("postgresql://") {
            Backend::Postgres(db.to_string())
        } else {
            Backend::Sqlite(PathBuf::from(db))
        }
    }
}

// The daemon's runtime configuration, honoring the documented CLI flags: --listen (P2P peer server), --rpc
// (local RPC), --introducer (seed bootstrap), --advertise (WAN address for gossip behind NAT), --db (backend).
#[derive(Clone, Debug)]
pub struct Config {
    pub listen: SocketAddr,
    pub rpc: SocketAddr,
    pub introducer: Option<(String, u16)>,
    // Manual peers dialed directly at startup (host:port each), in addition to any introducer-seeded
    // addresses. Persistent: a dropped manual peer is reclaimed to the address book and re-dialed. A node
    // pointed only at trusted, fast full nodes here needs no introducer at all.
    pub manual_peers: Vec<(String, u16)>,
    pub advertise: Option<SocketAddr>,
    pub backend: Backend,
    pub network_id: String,
    // Prometheus /metrics listen address. `Some` (default on); `None` when `--metrics off` disabled it.
    pub metrics: Option<SocketAddr>,
    // Debug: directory to capture real sync data into for offline replay — the fetched weight proof
    // (`weight_proof_<tip_height>.bin`) and every downloaded block range (`blocks_<start>_<end>.bin`).
    // Lets the whole fast-sync pipeline be validated + profiled offline with no live peer.
    pub capture_dir: Option<PathBuf>,
    // `--genesis-sync`: validate the historical chain block by block from height 0. Disables the
    // weight-proof fast sync entirely — the long-sync acceptance path and the discovery
    // harness for old-regime divergences.
    pub genesis_sync: bool,
    // `--sync-from <height>`: anchor mid-chain and validate forward from there. 0 = off.
    // Composes the weight-proof summaries (whole-chain epoch schedule) with a headers-first pass
    // over a fetched span just below the anchor. Mutually exclusive with fast sync; independent
    // nodes can each take a disjoint chain segment.
    pub sync_from: u32,
    // `--uncompact`: enable the compact-VDF solicitation scan (Phase 1.5). OFF by default, mirroring
    // chia's own `send_uncompact_interval: 0` default (chia/util/initial-config.yaml:403). With the
    // flag on, the scan hands bulky proofs to connected bluebox TIMELORDS via
    // RequestCompactProofOfTime (chia broadcast_uncompact_blocks) and consumes their
    // RespondCompactProofOfTime through the same validate/swap/re-gossip path as compact-VDF gossip.
    // On a network-infused node with no timelord peer the scan runs and sends
    // nothing — the code path exists and fires the moment a bluebox connects.
    pub uncompact: bool,
    // `--prefetch-memory-mb <N>`: RAM budget (MiB) for the window readahead's resident block bodies.
    // `None` = the shipped default (256 MiB, adaptive depth ≤ 8, one window per peer) — no change
    // for existing deployments. `Some(N)` opts a large-RAM, fetch-starved node into aggressive
    // prefetch: the budget becomes a HARD resident ceiling that drives a deeper lookahead K (allowed
    // past 8) AND raises the aggregate in-flight fetch concurrency (spread across peers) so the buffer
    // refills as fast as the validator drains it. OOM-safe: at huge block sizes the byte budget still
    // collapses depth toward one window.
    pub prefetch_memory_mb: Option<u64>,
    // `--prefetch-max-inflight <N>`: optional cap on the AGGREGATE outstanding block-range requests
    // (the concurrency knob, a COUNT — distinct from the byte budget above). `None` = derive from the
    // anti-flood ceiling (`peers × per-peer cap`). Spread across peers as `ceil(N / peers)` per
    // connection, never flooding one. Only takes effect alongside `--prefetch-memory-mb` (or on its
    // own, on the default budget).
    pub prefetch_max_inflight: Option<usize>,
    /// Outbound connections the node keeps open. Unset = the p2p default.
    pub target_outbound: Option<usize>,
    /// Total peers (inbound + outbound) the node accepts. Unset = the p2p default.
    pub target_peer_count: Option<usize>,
    // `--trusted-peer <node-id-hex>` (repeatable): the cert-hash node ids granted the trusted tier —
    // chia's `trusted_peers` config map, keyed on `node_id.hex()`. A trusted peer gets the larger
    // subscription / response-item caps and high-priority transaction-queue placement.
    // Empty (the default) → no peer trusted by node id.
    pub trusted_peers: Vec<String>,
    // `--trusted-cidr <cidr>` (repeatable): CIDR networks whose peers are granted the trusted tier by
    // remote IP — chia's `trusted_cidrs`. A peer whose host falls in any of these networks gets the
    // trusted caps + tx priority, exactly like a configured node id. Parsed once at boot; malformed
    // entries are skipped non-fatally. Note: with no config at all, localhost is STILL auto-trusted
    // (chia `is_trusted_peer`); these CIDRs extend that to a wider trusted subnet.
    pub trusted_cidrs: Vec<String>,
}

impl Config {
    /// Build a config from the parsed CLI strings.
    ///
    /// # Errors
    /// Returns an error string if `--listen`, `--rpc`, `--introducer`, `--peer`, or `--advertise` fail to
    /// parse.
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        listen: &str,
        rpc: &str,
        introducer: Option<&str>,
        peers: &[String],
        advertise: Option<&str>,
        db: &str,
        network: &str,
        metrics: &str,
        capture_dir: Option<&str>,
        genesis_sync: bool,
        sync_from: u32,
        uncompact: bool,
        prefetch_memory_mb: Option<u64>,
        prefetch_max_inflight: Option<usize>,
        target_outbound: Option<usize>,
        target_peer_count: Option<usize>,
        trusted_peers: &[String],
        trusted_cidrs: &[String],
    ) -> Result<Self, String> {
        let listen = SocketAddr::from_str(listen).map_err(|e| format!("bad --listen: {e}"))?;
        let rpc = SocketAddr::from_str(rpc).map_err(|e| format!("bad --rpc: {e}"))?;
        let introducer = introducer.map(parse_host_port).transpose()?;
        let manual_peers = peers
            .iter()
            .map(|p| parse_host_port(p))
            .collect::<Result<Vec<_>, _>>()?;
        let advertise = advertise
            .map(|a| SocketAddr::from_str(a).map_err(|e| format!("bad --advertise: {e}")))
            .transpose()?;
        let metrics = parse_metrics(metrics)?;
        Ok(Self {
            listen,
            rpc,
            introducer,
            manual_peers,
            advertise,
            backend: Backend::parse(db),
            network_id: network.to_string(),
            metrics,
            capture_dir: capture_dir.map(PathBuf::from),
            genesis_sync,
            sync_from,
            uncompact,
            prefetch_memory_mb,
            prefetch_max_inflight,
            target_outbound,
            target_peer_count,
            trusted_peers: trusted_peers.to_vec(),
            trusted_cidrs: trusted_cidrs.to_vec(),
        })
    }
}

// `--metrics`: an address enables the endpoint (default on), `off`/`none`/empty disables it.
fn parse_metrics(s: &str) -> Result<Option<SocketAddr>, String> {
    match s.trim() {
        "off" | "none" | "" => Ok(None),
        addr => Ok(Some(
            SocketAddr::from_str(addr).map_err(|e| format!("bad --metrics: {e}"))?,
        )),
    }
}

fn parse_host_port(s: &str) -> Result<(String, u16), String> {
    let (host, port) = s
        .rsplit_once(':')
        .ok_or_else(|| format!("expected host:port, got {s}"))?;
    let port = port
        .parse::<u16>()
        .map_err(|e| format!("bad host:port port in {s}: {e}"))?;
    Ok((host.to_string(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(peers: &[&str]) -> Result<Config, String> {
        let owned: Vec<String> = peers.iter().map(|s| (*s).to_string()).collect();
        Config::build(
            "0.0.0.0:8444",
            "0.0.0.0:8555",
            None,
            &owned,
            None,
            "sqlite:///data/chain.db",
            "mainnet",
            "off",
            None,
            false,
            0,
            false,
            None,
            None,
            None,
            None,
            &[],
            &[],
        )
    }

    #[test]
    fn trusted_peers_default_empty_and_pass_through() {
        assert!(cfg(&[]).unwrap().trusted_peers.is_empty());
        assert!(cfg(&[]).unwrap().trusted_cidrs.is_empty());
        let c = Config::build(
            "0.0.0.0:8444",
            "0.0.0.0:8555",
            None,
            &[],
            None,
            "sqlite:///data/chain.db",
            "mainnet",
            "off",
            None,
            false,
            0,
            false,
            None,
            None,
            None,
            None,
            &["aa".repeat(32)],
            &["10.0.0.0/8".to_string()],
        )
        .unwrap();
        assert_eq!(c.trusted_peers, vec!["aa".repeat(32)]);
        assert_eq!(c.trusted_cidrs, vec!["10.0.0.0/8".to_string()]);
    }

    #[test]
    fn no_peer_flags_yield_empty_manual_peers() {
        assert!(cfg(&[]).unwrap().manual_peers.is_empty());
    }

    #[test]
    fn repeated_peer_flags_all_parse_in_order() {
        // A DNS service name and a bare IP must both parse; order is preserved.
        let c = cfg(&["chia-node-0.peers.example:8444", "10.101.159.8:8444"]).unwrap();
        assert_eq!(
            c.manual_peers,
            vec![
                ("chia-node-0.peers.example".to_string(), 8444),
                ("10.101.159.8".to_string(), 8444),
            ]
        );
    }

    #[test]
    fn a_peer_without_a_port_is_rejected() {
        assert!(cfg(&["chia-node-0.peers"]).is_err());
    }
}
