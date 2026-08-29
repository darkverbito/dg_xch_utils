// The trusted-peer tier. Trust keys on three inputs:
//   `is_localhost(host) || node_id in trusted_peers || host in trusted_cidrs`.
// Trust is evaluated fresh at each gate (never cached on the connection), so this policy is a
// pure function of the peer's cert-hash node id, its remote HOST (IP), and the config.
//
// The host is carried on `SocketPeer.host` and threaded into each gate. A peer with no resolved
// host (an outbound dial to a name we never resolved to an `IpAddr`) can still be trusted by
// node id — never by host.
//
// What trust gates:
//   1. `max_subscriptions(peer)`             — 200,000 untrusted → 2,000,000 trusted.
//   2. `max_subscribe_response_items(peer)`  — 100,000 untrusted →   500,000 trusted.
//   3. tx-queue admission                    — trusted peers' bundles get high-priority queue
//      placement; see `tx_queue::TxQueue`.
//
// With NO config, localhost is auto-trusted (`is_localhost(host)` short-circuits before the
// node-id map): a loopback wallet gets the trusted caps + tx priority out of the box.

use dg_xch_core::blockchain::sized_bytes::Bytes32;
use ipnet::IpNet;
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;
use tracing::warn;

// Stock full-node subscription caps.
const MAX_SUBSCRIBE_ITEMS: usize = 200_000;
const TRUSTED_MAX_SUBSCRIBE_ITEMS: usize = 2_000_000;
const MAX_SUBSCRIBE_RESPONSE_ITEMS: usize = 100_000;
const TRUSTED_MAX_SUBSCRIBE_RESPONSE_ITEMS: usize = 500_000;

/// The trusted-peer policy: the set of trusted peer node ids (cert hashes), the list of trusted
/// CIDR networks, and the untrusted/trusted cap pairs. Shared as an `Arc` between the
/// [`crate::wallet::WalletNotifier`] (subscription caps), the full-node api (response-item cap), and
/// the gossip transaction queue (priority tier). The default (empty node-id set, empty CIDR
/// list): every REMOTE peer untrusted, but a LOCALHOST peer auto-trusted.
#[derive(Debug, Clone)]
pub struct TrustPolicy {
    trusted: HashSet<Bytes32>,
    trusted_cidrs: Vec<IpNet>,
    max_subscribe_items: usize,
    trusted_max_subscribe_items: usize,
    max_subscribe_response_items: usize,
    trusted_max_subscribe_response_items: usize,
}

impl Default for TrustPolicy {
    /// The default policy: an empty trusted node-id set and empty CIDR list at the stock caps.
    /// Every remote peer resolves untrusted; a localhost peer resolves trusted.
    fn default() -> Self {
        Self {
            trusted: HashSet::new(),
            trusted_cidrs: Vec::new(),
            max_subscribe_items: MAX_SUBSCRIBE_ITEMS,
            trusted_max_subscribe_items: TRUSTED_MAX_SUBSCRIBE_ITEMS,
            max_subscribe_response_items: MAX_SUBSCRIBE_RESPONSE_ITEMS,
            trusted_max_subscribe_response_items: TRUSTED_MAX_SUBSCRIBE_RESPONSE_ITEMS,
        }
    }
}

impl TrustPolicy {
    /// Build a policy from a set of trusted node ids at the stock caps (no trusted CIDRs).
    #[must_use]
    pub fn new(trusted: HashSet<Bytes32>) -> Self {
        Self {
            trusted,
            ..Self::default()
        }
    }

    /// Build a policy from the runtime config's `trusted_peers` list of hex node-id (cert-hash)
    /// strings. A malformed hex entry is skipped (logged by the caller); an empty list yields the
    /// default (localhost-only trust).
    #[must_use]
    pub fn from_hex_ids(ids: &[String]) -> Self {
        let trusted = ids
            .iter()
            .filter_map(|s| Bytes32::from_str(s.trim()).ok())
            .collect();
        Self::new(trusted)
    }

    /// The production constructor — the two config inputs: `trusted_peers` (hex node-id /
    /// cert-hash strings) AND `trusted_cidrs` (CIDR strings). A malformed node-id is skipped
    /// silently; a malformed CIDR is logged and skipped. Empty lists yield the default
    /// (localhost auto-trusted, every other peer untrusted).
    #[must_use]
    pub fn from_config(trusted_peers: &[String], trusted_cidrs: &[String]) -> Self {
        let mut policy = Self::from_hex_ids(trusted_peers);
        policy.trusted_cidrs = Self::parse_cidrs(trusted_cidrs);
        policy
    }

    /// Parse `--trusted-cidr` strings into IPv4/IPv6 CIDR matchers. A malformed entry is logged
    /// and skipped — one bad config line does not sink the node. Both IPv4 (`10.0.0.0/8`) and
    /// IPv6 (`2001:db8::/32`) forms parse.
    fn parse_cidrs(cidrs: &[String]) -> Vec<IpNet> {
        cidrs
            .iter()
            .filter_map(|c| match c.trim().parse::<IpNet>() {
                Ok(net) => Some(net),
                Err(e) => {
                    warn!("skipping malformed --trusted-cidr {c:?}: {e}");
                    None
                }
            })
            .collect()
    }

    /// A policy with explicit caps — the test seam for driving the trusted/untrusted split at
    /// small scale (production uses [`TrustPolicy::from_config`] / [`TrustPolicy::default`]).
    #[must_use]
    pub fn with_caps(
        trusted: HashSet<Bytes32>,
        max_subscribe_items: usize,
        trusted_max_subscribe_items: usize,
        max_subscribe_response_items: usize,
        trusted_max_subscribe_response_items: usize,
    ) -> Self {
        Self {
            trusted,
            trusted_cidrs: Vec::new(),
            max_subscribe_items,
            trusted_max_subscribe_items,
            max_subscribe_response_items,
            trusted_max_subscribe_response_items,
        }
    }

    /// The loopback identities. `SocketPeer.host` is an already-resolved `IpAddr`, so the
    /// hostname form (`"localhost"`) never reaches this layer — only the two literal loopback
    /// IPs, 127.0.0.1 and ::1. Matched EXACTLY (not the whole 127.0.0.0/8 block): 127.0.0.2 is
    /// loopback but is not trusted.
    #[must_use]
    fn is_localhost(host: IpAddr) -> bool {
        host == IpAddr::V4(Ipv4Addr::LOCALHOST) || host == IpAddr::V6(Ipv6Addr::LOCALHOST)
    }

    /// Whether `host` falls inside any configured trusted CIDR. `IpNet::contains` masks host
    /// bits by the network prefix, so an IPv4 host is only ever tested against IPv4 networks and
    /// likewise for IPv6.
    #[must_use]
    fn host_in_trusted_cidrs(&self, host: IpAddr) -> bool {
        self.trusted_cidrs.iter().any(|net| net.contains(&host))
    }

    /// Host-only trust: localhost or a trusted-CIDR member — the gate inbound timelord
    /// acceptance uses. Node-id trust deliberately does NOT apply: these gates key on the source
    /// address, never on the cert identity.
    #[must_use]
    pub fn host_trusted(&self, host: Option<IpAddr>) -> bool {
        matches!(host, Some(ip) if Self::is_localhost(ip) || self.host_in_trusted_cidrs(ip))
    }

    /// Whether `peer` is trusted:
    /// `is_localhost(host) || node_id.hex() in trusted_peers || is_trusted_cidr(host, trusted_cidrs)`.
    /// `host` is the peer's remote IP; `None` (an outbound dial to an unresolved name) means only
    /// the node-id path can grant trust. With empty config only localhost is trusted.
    #[must_use]
    pub fn is_trusted(&self, peer: &Bytes32, host: Option<IpAddr>) -> bool {
        if let Some(ip) = host
            && (Self::is_localhost(ip) || self.host_in_trusted_cidrs(ip))
        {
            return true;
        }
        self.trusted.contains(peer)
    }

    /// The per-peer combined subscription cap: the trusted `trusted_max_subscribe_items`
    /// (2,000,000) for a trusted peer, else the untrusted `max_subscribe_items` (200,000).
    #[must_use]
    pub fn max_subscriptions(&self, peer: &Bytes32, host: Option<IpAddr>) -> usize {
        if self.is_trusted(peer, host) {
            self.trusted_max_subscribe_items
        } else {
            self.max_subscribe_items
        }
    }

    /// The initial-state response-item budget: the trusted `trusted_max_subscribe_response_items`
    /// (500,000) for a trusted peer, else the untrusted `max_subscribe_response_items` (100,000).
    #[must_use]
    pub fn max_subscribe_response_items(&self, peer: &Bytes32, host: Option<IpAddr>) -> usize {
        if self.is_trusted(peer, host) {
            self.trusted_max_subscribe_response_items
        } else {
            self.max_subscribe_response_items
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(byte: u8) -> Bytes32 {
        Bytes32::from([byte; 32])
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("test IP literal")
    }

    // Gate 4: is_trusted resolution — a node id in the configured set resolves trusted, a non-member
    // untrusted, and (with no host) an empty config leaves every peer untrusted.
    #[test]
    fn is_trusted_resolves_on_node_id_membership() {
        let policy = TrustPolicy::new(HashSet::from([id(0xaa)]));
        assert!(
            policy.is_trusted(&id(0xaa), None),
            "configured node id is trusted"
        );
        assert!(
            !policy.is_trusted(&id(0xbb), None),
            "non-member is untrusted"
        );
    }

    // Node-id trust is independent of host: trusted with no host AND with an arbitrary
    // non-loopback host, while a non-member stays untrusted at that same host.
    #[test]
    fn node_id_trust_is_independent_of_host() {
        let policy = TrustPolicy::new(HashSet::from([id(0xaa)]));
        assert!(policy.is_trusted(&id(0xaa), None));
        assert!(policy.is_trusted(&id(0xaa), Some(ip("8.8.8.8"))));
        assert!(!policy.is_trusted(&id(0xbb), Some(ip("8.8.8.8"))));
    }

    // Case 4: a non-localhost, non-CIDR, non-node-id peer is untrusted — the default is unchanged for
    // remote peers, and an absent host cannot be trusted.
    #[test]
    fn empty_config_leaves_remote_peers_untrusted() {
        let policy = TrustPolicy::default();
        assert!(!policy.is_trusted(&id(0xaa), None));
        assert!(!policy.is_trusted(&id(0x00), Some(ip("10.0.0.1"))));
        // 127.0.0.2 is is_loopback() but NOT in the literal localhost set — stays untrusted.
        assert!(!policy.is_trusted(&id(0x00), Some(ip("127.0.0.2"))));
        assert_eq!(
            policy.max_subscriptions(&id(0xaa), Some(ip("10.0.0.1"))),
            MAX_SUBSCRIBE_ITEMS
        );
        assert_eq!(
            policy.max_subscribe_response_items(&id(0xaa), Some(ip("10.0.0.1"))),
            MAX_SUBSCRIBE_RESPONSE_ITEMS
        );
    }

    // Case 1: a localhost peer (127.0.0.1 / ::1) is trusted even with empty trusted_peers/cidrs, and
    // that lifts BOTH caps to the trusted tier — is_localhost short-circuits before the node-id
    // map.
    #[test]
    fn localhost_is_trusted_with_empty_config() {
        let policy = TrustPolicy::default();
        assert!(policy.is_trusted(&id(0x00), Some(ip("127.0.0.1"))));
        assert!(policy.is_trusted(&id(0x00), Some(ip("::1"))));
        assert_eq!(
            policy.max_subscriptions(&id(0x00), Some(ip("127.0.0.1"))),
            TRUSTED_MAX_SUBSCRIBE_ITEMS
        );
        assert_eq!(
            policy.max_subscribe_response_items(&id(0x00), Some(ip("::1"))),
            TRUSTED_MAX_SUBSCRIBE_RESPONSE_ITEMS
        );
    }

    // Case 2: a peer whose host IP is inside a configured trusted_cidr is trusted (IPv4 and IPv6);
    // outside is untrusted; the caps lift for an in-CIDR peer.
    #[test]
    fn host_in_trusted_cidr_is_trusted() {
        let policy = TrustPolicy::from_config(&[], &["10.0.0.0/8".into(), "2001:db8::/32".into()]);
        assert!(policy.is_trusted(&id(0xbb), Some(ip("10.1.2.3"))));
        assert!(policy.is_trusted(&id(0xbb), Some(ip("2001:db8::dead"))));
        assert!(!policy.is_trusted(&id(0xbb), Some(ip("11.0.0.1"))));
        assert!(!policy.is_trusted(&id(0xbb), Some(ip("2001:dead::1"))));
        assert_eq!(
            policy.max_subscriptions(&id(0xbb), Some(ip("10.9.9.9"))),
            TRUSTED_MAX_SUBSCRIBE_ITEMS
        );
    }

    // Case 5: malformed --trusted-cidr entries are skipped non-fatally; the surviving valid CIDR
    // (here IPv4) still matches. IPv4 + IPv6 matching is covered by `host_in_trusted_cidr_is_trusted`.
    #[test]
    fn malformed_cidr_entries_skipped_non_fatally() {
        let policy = TrustPolicy::from_config(
            &[],
            &[
                "not-a-cidr".into(),
                "999.0.0.0/8".into(),
                "10.0.0.0/33".into(),
                "192.168.0.0/16".into(),
            ],
        );
        assert!(policy.is_trusted(&id(0x00), Some(ip("192.168.4.4"))));
        assert!(!policy.is_trusted(&id(0x00), Some(ip("172.16.0.1"))));
    }

    // Gate 1+2 cap selection: a trusted (node-id) peer gets the trusted caps, an untrusted peer the
    // untrusted caps — max_subscriptions / max_subscribe_response_items.
    #[test]
    fn trusted_peer_gets_trusted_caps_untrusted_gets_untrusted() {
        let policy = TrustPolicy::new(HashSet::from([id(0xaa)]));
        assert_eq!(
            policy.max_subscriptions(&id(0xaa), None),
            TRUSTED_MAX_SUBSCRIBE_ITEMS
        );
        assert_eq!(
            policy.max_subscriptions(&id(0xbb), None),
            MAX_SUBSCRIBE_ITEMS
        );
        assert_eq!(
            policy.max_subscribe_response_items(&id(0xaa), None),
            TRUSTED_MAX_SUBSCRIBE_RESPONSE_ITEMS
        );
        assert_eq!(
            policy.max_subscribe_response_items(&id(0xbb), None),
            MAX_SUBSCRIBE_RESPONSE_ITEMS
        );
    }

    // The config surface: hex node-id strings parse to the trusted set; a malformed entry is skipped,
    // not fatal.
    #[test]
    fn from_hex_ids_parses_valid_and_skips_malformed() {
        let good = "aa".repeat(32); // 64 hex chars = 32 bytes
        let policy = TrustPolicy::from_hex_ids(&[good, "not-hex".to_string()]);
        assert!(policy.is_trusted(&id(0xaa), None));
        assert!(!policy.is_trusted(&id(0xbb), None));
    }
}
