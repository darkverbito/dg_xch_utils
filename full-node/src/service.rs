//! The portfu process skeleton: one type-erased handle over the concrete store
//! instantiations so tasks and endpoints register once, plus the shared service
//! state (peer registry, run flags, supervisor) the tasks operate through.

use crate::daemon::Node;
use crate::daemon::OutboundPeers;
use crate::metrics::MetricsSources;
use dg_xch_core::protocols::PeerMap;
use dg_xch_p2p::sessions::Supervisor;
use dg_xch_stores::SqliteStore;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A concrete backend instantiation: the node plus its metrics/health sources.
pub struct BackendHandle<S> {
    pub node: Arc<Node<S>>,
    pub sources: Arc<MetricsSources<S>>,
}

/// The one process-wide node handle. `State<T>` is type-keyed and task
/// registration is monomorphic, so the generic `Node<S>` is carried behind this
/// enum and every portfu construct dispatches through it.
pub enum ActiveNode {
    Sqlite(BackendHandle<SqliteStore>),
    #[cfg(feature = "postgres")]
    Postgres(BackendHandle<dg_xch_stores::PostgresStore>),
    #[cfg(feature = "mmap")]
    Mmap(BackendHandle<dg_xch_stores::MmapStore>),
}

macro_rules! with_backend {
    ($self:expr, $h:ident => $body:expr) => {
        match $self {
            ActiveNode::Sqlite($h) => $body,
            #[cfg(feature = "postgres")]
            ActiveNode::Postgres($h) => $body,
            #[cfg(feature = "mmap")]
            ActiveNode::Mmap($h) => $body,
        }
    };
}

impl ActiveNode {
    #[must_use]
    pub fn backend_name(&self) -> &'static str {
        match self {
            ActiveNode::Sqlite(_) => "sqlite",
            #[cfg(feature = "postgres")]
            ActiveNode::Postgres(_) => "postgres",
            #[cfg(feature = "mmap")]
            ActiveNode::Mmap(_) => "mmap",
        }
    }

    #[must_use]
    pub fn debug_endpoints(&self) -> bool {
        with_backend!(self, h => h.node.config.debug_endpoints)
    }

    pub async fn metrics_text(&self) -> String {
        with_backend!(self, h => h.sources.metrics_text().await)
    }

    pub async fn health_check(&self) -> (&'static str, String) {
        with_backend!(self, h => h.sources.health_check().await)
    }

    /// A dashboard-sized status line: the sync numbers plus the health verdict.
    pub async fn status_json(&self) -> String {
        with_backend!(self, h => {
            let snap = h.sources.sample_liveness().await;
            let (health, _) = h.sources.health_check().await;
            format!(
                "{{\"backend\":\"{}\",\"peak\":{},\"claimed\":{},\"tip_lag\":{},\"healthy\":{}}}",
                self.backend_name(),
                snap.peak_height,
                snap.claimed_peak,
                snap.tip_lag,
                health.starts_with("200"),
            )
        })
    }

    pub fn set_run(&self, value: bool) {
        with_backend!(self, h => h.node.run.store(value, Ordering::Relaxed));
    }

    #[must_use]
    pub fn is_running(&self) -> bool {
        with_backend!(self, h => h.node.run.load(Ordering::Relaxed))
    }

    pub async fn run_sync_driver(&self, services: &NodeServices) {
        with_backend!(self, h => {
            crate::daemon::sync_driver(
                h.node.clone(),
                services.registry.clone(),
                services.inbound_peers.clone(),
            )
            .await;
        });
    }

    pub async fn run_tip_follower(&self, services: &NodeServices) {
        with_backend!(self, h => {
            crate::daemon::tip_follower(
                h.node.clone(),
                services.registry.clone(),
                services.inbound_peers.clone(),
            )
            .await;
        });
    }

    pub async fn reap_wallet_subscriptions(&self, services: &NodeServices) {
        with_backend!(self, h => {
            crate::daemon::reap_wallet_subscriptions_once(&h.node, &services.inbound_peers).await;
        });
    }
}

/// Everything the boot phase wires up before the portfu server starts: the
/// outbound supervisor and its peer registry, the inbound peer map, and the run
/// flags of the two protocol servers (chia P2P and RPC) that keep their own
/// specialized TLS listeners.
pub struct NodeServices {
    pub registry: Arc<dyn OutboundPeers>,
    pub peer_registry: Arc<dg_xch_p2p::PeerRegistry>,
    pub inbound_peers: PeerMap,
    pub peer_run: Arc<AtomicBool>,
    pub rpc_run: Arc<AtomicBool>,
    pub supervisor: tokio::sync::Mutex<Option<Supervisor>>,
}

impl NodeServices {
    /// Stop the protocol servers and the supervisor. Called once after the
    /// portfu server exits.
    pub async fn drain(&self) {
        self.peer_run.store(false, Ordering::Relaxed);
        self.rpc_run.store(false, Ordering::Relaxed);
        if let Some(mut supervisor) = self.supervisor.lock().await.take() {
            supervisor.stop().await;
        }
    }
}
