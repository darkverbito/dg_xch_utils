use std::time::Duration;

// Slot targets and caps reuse the full-node config defaults.
#[derive(Debug, Clone, Copy)]
pub struct P2pSettings {
    pub target_outbound: usize,
    pub target_peer_count: usize,
    pub host_pool_capacity: usize,
    pub address_lower: usize,
    pub address_upper: usize,
    pub connect_timeout: Duration,
    pub handshake_timeout: Duration,
    pub retry_timeout: Duration,
    pub heartbeat: Duration,
    pub pong_deadline: Duration,
    pub recent_peer_threshold: Duration,
    pub jitter_floor: f64,
}

impl Default for P2pSettings {
    fn default() -> Self {
        Self {
            target_outbound: 8,
            target_peer_count: 80,
            host_pool_capacity: 1000,
            address_lower: 5,
            address_upper: 10,
            connect_timeout: Duration::from_secs(30),
            handshake_timeout: Duration::from_secs(15),
            retry_timeout: Duration::from_secs(1),
            heartbeat: Duration::from_secs(120),
            pong_deadline: Duration::from_secs(30),
            recent_peer_threshold: Duration::from_secs(6000),
            jitter_floor: 0.5,
        }
    }
}

impl P2pSettings {
    // Full-jitter backoff: retry_timeout scaled to [jitter_floor, 1.0].
    #[must_use]
    pub fn jittered_backoff(&self, attempt: u32) -> Duration {
        let capped = attempt.min(6);
        let base = self.retry_timeout.saturating_mul(1u32 << capped);
        let frac = self.jitter_floor + (1.0 - self.jitter_floor) * rand::random::<f64>();
        base.mul_f64(frac)
    }
}
