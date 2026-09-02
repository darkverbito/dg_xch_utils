use crate::address_manager::Endpoint;
use crate::config::P2pSettings;
use dg_xch_clients::ClientSSLConfig;
use dg_xch_clients::websocket::{WsClient, WsClientConfig};
use dg_xch_core::constants::{CHIA_CA_CRT, CHIA_CA_KEY};
use dg_xch_core::protocols::{ChiaMessageHandler, NodeType};
use dg_xch_serialize::ChiaProtocolVersion;
use std::collections::{HashMap, HashSet};
use std::io::Error;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio::sync::RwLock;
use uuid::Uuid;

#[derive(Debug, PartialEq, Eq)]
pub enum AdmitError {
    InboundCapReached,
    DuplicateEndpoint,
    SelfConnection,
}

// One live outbound channel = one sync-reservation slot. The
// connection is reachable for the sync layer to issue RequestBlocks against.
pub struct OutboundPeer {
    pub endpoint: Endpoint,
    pub client: WsClient,
    pub run: Arc<AtomicBool>,
}
impl OutboundPeer {
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.client.is_closed()
    }
    pub fn stop(&self) {
        self.run.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

// Shared connection bookkeeping: the inbound cap, duplicate-endpoint reserve, and
// self-connection detection (NAT hairpin), plus the live outbound set for the sync seam.
pub struct PeerRegistry {
    inner: RwLock<Inner>,
    pub settings: P2pSettings,
    pub self_nonce: u64,
}
#[derive(Default)]
struct Inner {
    outbound: Vec<Arc<OutboundPeer>>,
    inbound: HashSet<Endpoint>,
    endpoints: HashSet<Endpoint>,
    selfs: HashSet<Endpoint>,
}

impl PeerRegistry {
    #[must_use]
    pub fn new(settings: P2pSettings) -> Self {
        Self {
            inner: RwLock::new(Inner::default()),
            settings,
            self_nonce: rand::random(),
        }
    }

    pub async fn add_self(&self, ep: Endpoint) {
        self.inner.write().await.selfs.insert(ep);
    }

    // Admit only while total < target_peer_count, and reject a duplicate endpoint or a dial
    // that resolves to our own authority.
    pub async fn admit_inbound(&self, ep: &Endpoint) -> Result<(), AdmitError> {
        let mut g = self.inner.write().await;
        if g.selfs.contains(ep) {
            return Err(AdmitError::SelfConnection);
        }
        if g.endpoints.contains(ep) {
            return Err(AdmitError::DuplicateEndpoint);
        }
        if g.inbound.len() + g.outbound.len() >= self.settings.target_peer_count {
            return Err(AdmitError::InboundCapReached);
        }
        g.inbound.insert(ep.clone());
        g.endpoints.insert(ep.clone());
        Ok(())
    }

    pub async fn release_inbound(&self, ep: &Endpoint) {
        let mut g = self.inner.write().await;
        g.inbound.remove(ep);
        g.endpoints.remove(ep);
    }

    pub async fn reserve_outbound(&self, ep: &Endpoint) -> Result<(), AdmitError> {
        let mut g = self.inner.write().await;
        if g.selfs.contains(ep) {
            return Err(AdmitError::SelfConnection);
        }
        if g.endpoints.contains(ep) {
            return Err(AdmitError::DuplicateEndpoint);
        }
        g.endpoints.insert(ep.clone());
        Ok(())
    }

    pub async fn register_outbound(&self, peer: Arc<OutboundPeer>) {
        self.inner.write().await.outbound.push(peer);
    }

    pub async fn release_outbound(&self, ep: &Endpoint) {
        let mut g = self.inner.write().await;
        g.outbound.retain(|p| &p.endpoint != ep);
        g.endpoints.remove(ep);
    }

    // The sync seam: iterate the live outbound channels (reservation slots).
    pub async fn outbound_peers(&self) -> Vec<Arc<OutboundPeer>> {
        self.inner.read().await.outbound.clone()
    }

    // Evict a misbehaving/slow peer: stop its channel. The owning slot observes the stop,
    // releases the reservation, and re-dials — eviction never revives the dead channel.
    pub async fn evict(&self, ep: &Endpoint) -> bool {
        if let Some(p) = self
            .inner
            .read()
            .await
            .outbound
            .iter()
            .find(|p| &p.endpoint == ep)
        {
            p.stop();
            return true;
        }
        false
    }

    pub async fn outbound_count(&self) -> usize {
        self.inner.read().await.outbound.len()
    }

    pub async fn inbound_count(&self) -> usize {
        self.inner.read().await.inbound.len()
    }
}

pub fn empty_handlers() -> Arc<RwLock<HashMap<Uuid, Arc<ChiaMessageHandler>>>> {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Builds a FRESH per-connection handler map on each dial. A fresh map per connection is required: the
/// oneshot request machinery subscribes/unsubscribes temporary handlers on the same map, so sharing one map
/// across peers would cross-deliver responses. The daemon supplies `full_node_handlers_client` here so
/// outbound peers dispatch NewPeak/RequestBlock/gossip; `None` falls back to `empty_handlers`.
pub type HandlerFactory = Arc<dyn Fn() -> HashMap<Uuid, Arc<ChiaMessageHandler>> + Send + Sync>;

// Build the per-connection handler map for a dial: the factory's fresh map, or an empty one.
pub(crate) fn dial_handlers(
    factory: Option<&HandlerFactory>,
) -> Arc<RwLock<HashMap<Uuid, Arc<ChiaMessageHandler>>>> {
    match factory {
        Some(f) => Arc::new(RwLock::new(f())),
        None => empty_handlers(),
    }
}

pub async fn dial(
    host: &str,
    port: u16,
    handlers: Arc<RwLock<HashMap<Uuid, Arc<ChiaMessageHandler>>>>,
    run: Arc<AtomicBool>,
    settings: &P2pSettings,
) -> Result<WsClient, Error> {
    let config = Arc::new(WsClientConfig {
        host: host.to_string(),
        port,
        network_id: "mainnet".to_string(),
        ssl_info: None::<ClientSSLConfig>,
        software_version: None,
        protocol_version: ChiaProtocolVersion::default(),
        additional_headers: None,
        // Outbound full-node link: police what the peer sends us, so a peer we dial to sync from
        // cannot flood us and our own solicited RespondBlocks bursts are accounted at the read
        // loop.
        rate_limited: true,
    });
    let timeout = settings.connect_timeout.as_secs();
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        runtime.block_on(WsClient::with_ca(
            config,
            NodeType::FullNode,
            handlers,
            run,
            CHIA_CA_CRT.as_bytes(),
            CHIA_CA_KEY.as_bytes(),
            timeout,
        ))
    })
    .await
    .map_err(|error| Error::other(format!("connection task failed: {error}")))?
}

impl dg_xch_core::errors::ErrorCode for AdmitError {
    fn band(&self) -> dg_xch_core::errors::ErrorBand {
        dg_xch_core::errors::ErrorBand::Peer
    }

    fn variant(&self) -> u16 {
        match self {
            AdmitError::InboundCapReached => 1,
            AdmitError::DuplicateEndpoint => 2,
            AdmitError::SelfConnection => 3,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(p: u16) -> Endpoint {
        ("10.0.0.1".to_string(), p)
    }

    #[tokio::test]
    async fn inbound_cap_is_enforced() {
        let reg = PeerRegistry::new(P2pSettings {
            target_peer_count: 2,
            ..P2pSettings::default()
        });
        assert!(reg.admit_inbound(&ep(1)).await.is_ok());
        assert!(reg.admit_inbound(&ep(2)).await.is_ok());
        assert_eq!(
            reg.admit_inbound(&ep(3)).await.unwrap_err(),
            AdmitError::InboundCapReached
        );
        reg.release_inbound(&ep(1)).await;
        assert!(reg.admit_inbound(&ep(3)).await.is_ok());
    }

    #[tokio::test]
    async fn inbound_flood_plateaus_flat_at_the_cap() {
        let reg = PeerRegistry::new(P2pSettings {
            target_peer_count: 80,
            ..P2pSettings::default()
        });
        let mut accepted = 0usize;
        let mut rejected = 0usize;
        for i in 0..10_000u32 {
            let o = i.to_le_bytes();
            let e = (format!("{}.{}.{}.{}", o[0], o[1], o[2], o[3]), 8444);
            match reg.admit_inbound(&e).await {
                Ok(()) => accepted += 1,
                Err(AdmitError::InboundCapReached) => rejected += 1,
                Err(_) => {}
            }
        }
        // the tracked set never grows past the cap: bounded memory under flood (flat RSS)
        assert_eq!(accepted, 80);
        assert_eq!(reg.inbound_count().await, 80);
        assert_eq!(rejected, 10_000 - 80);
    }

    #[tokio::test]
    async fn duplicate_endpoint_is_rejected() {
        let reg = PeerRegistry::new(P2pSettings::default());
        assert!(reg.admit_inbound(&ep(1)).await.is_ok());
        assert_eq!(
            reg.admit_inbound(&ep(1)).await.unwrap_err(),
            AdmitError::DuplicateEndpoint
        );
    }

    #[tokio::test]
    async fn self_authority_is_rejected_both_directions() {
        let reg = PeerRegistry::new(P2pSettings::default());
        reg.add_self(ep(8444)).await;
        assert_eq!(
            reg.admit_inbound(&ep(8444)).await.unwrap_err(),
            AdmitError::SelfConnection
        );
        assert_eq!(
            reg.reserve_outbound(&ep(8444)).await.unwrap_err(),
            AdmitError::SelfConnection
        );
    }
}
