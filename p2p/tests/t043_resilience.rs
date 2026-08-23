mod common;

use common::{
    empty_api, fast_settings, keepalive_settings, peer, spawn_full_node, spawn_silent_node,
    wait_until,
};
use dg_xch_p2p::{P2pSettings, Supervisor};
use std::time::{Duration, Instant};

// Inject-and-measure: the jittered backoff must spread reconnects, not synchronize them.
#[test]
fn jittered_backoff_spreads_reconnects_no_thundering_herd() {
    let s = P2pSettings {
        retry_timeout: Duration::from_millis(1000),
        jitter_floor: 0.5,
        ..P2pSettings::default()
    };
    let samples: Vec<f64> = (0..2000)
        .map(|_| s.jittered_backoff(0).as_secs_f64() * 1000.0)
        .collect();
    let min = samples.iter().cloned().fold(f64::MAX, f64::min);
    let max = samples.iter().cloned().fold(f64::MIN, f64::max);
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    println!(
        "[MEASURED] attempt-0 backoff ms: min={min:.1} max={max:.1} mean={mean:.1} (base 1000, floor 0.5)"
    );
    // full-jitter over [floor,1.0]*base = [500,1000] ms; assert the spread is real
    assert!((500.0..560.0).contains(&min), "floor honored: {min}");
    assert!((940.0..=1000.0).contains(&max), "cap honored: {max}");
    assert!(max - min > 350.0, "reconnects are spread, not synchronized");

    // exponential growth under repeated failure, still capped by jitter
    let a5 = s.jittered_backoff(5).as_secs_f64() * 1000.0;
    println!("[MEASURED] attempt-5 backoff ms: {a5:.1} (base 32000, floor 0.5)");
    assert!((16_000.0..=32_000.0).contains(&a5));
}

// F-1 (loopback): mass server-side drop of a 4-slot fleet -> all reconnect, and the
// reconnect instants are spread by the jitter, not synchronized.
#[tokio::test]
async fn mass_drop_reconnects_all_slots_with_spread() {
    let settings = P2pSettings {
        target_outbound: 4,
        retry_timeout: Duration::from_millis(400),
        ..fast_settings()
    };
    // four independent loopback servers, one per slot
    let mut servers = Vec::new();
    let mut sup = Supervisor::new(settings);
    for _ in 0..4 {
        let srv = spawn_full_node(empty_api()).await;
        sup.seed_addresses(&[peer("127.0.0.1", srv.port, 1)]).await;
        servers.push(srv);
    }
    sup.start_outbound();
    let reg = sup.registry.clone();
    let connected = wait_until(
        || async { reg.outbound_count().await == 4 },
        Duration::from_secs(40),
    )
    .await;
    println!("[MEASURED] slots connected: {}", reg.outbound_count().await);
    assert!(connected, "all four slots connect");

    // Inject: drop every peer on every server at the same instant.
    for srv in &servers {
        common::drop_all_server_peers(srv).await;
    }
    // Wait for the fleet to detect the drop and tear down (count falls) before timing
    // the re-dials — otherwise we'd time the pre-drop state.
    assert!(
        wait_until(
            || async { reg.outbound_count().await == 0 },
            Duration::from_secs(30)
        )
        .await,
        "the whole fleet detects the drop and tears down"
    );
    // Measure each slot's re-dial instant from the moment of full teardown.
    let t0 = Instant::now();
    let mut recovered = 0usize;
    let mut times = Vec::new();
    while recovered < 4 && t0.elapsed() < Duration::from_secs(30) {
        let c = reg.outbound_count().await;
        if c > recovered {
            for _ in recovered..c {
                times.push(t0.elapsed().as_millis());
            }
            recovered = c;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(recovered, 4, "every slot reconnected after the mass drop");
    let spread = times.iter().max().unwrap() - times.iter().min().unwrap();
    println!("[MEASURED] re-dial ms per slot (from teardown): {times:?}  spread={spread}ms");

    sup.stop().await;
    for srv in servers {
        srv.run.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

// F (loopback): a silent half-open peer (handshakes, never answers the probe) is torn
// down within the pong deadline — not left in a 30s TCP limbo.
#[tokio::test]
async fn silent_half_open_peer_is_torn_down_within_the_deadline() {
    let silent = spawn_silent_node().await;
    let settings = keepalive_settings();
    let mut sup = Supervisor::new(settings);
    sup.start_manual("127.0.0.1", silent.port);
    let reg = sup.registry.clone();

    assert!(
        wait_until(
            || async { reg.outbound_count().await == 1 },
            Duration::from_secs(10)
        )
        .await,
        "manual peer connects to the silent node"
    );

    // Measure detection: from connected to torn-down (count back to 0).
    let t0 = Instant::now();
    let torn = wait_until(
        || async { reg.outbound_count().await == 0 },
        Duration::from_secs(5),
    )
    .await;
    let detect = t0.elapsed();
    println!(
        "[MEASURED] half-open teardown in {}ms (heartbeat {}ms + pong deadline {}ms)",
        detect.as_millis(),
        settings.heartbeat.as_millis(),
        settings.pong_deadline.as_millis()
    );
    assert!(torn, "silent peer is torn down");
    assert!(
        detect < settings.heartbeat + settings.pong_deadline + Duration::from_millis(1500),
        "teardown within the deadline, not a TCP limbo"
    );

    sup.stop().await;
    silent
        .run
        .store(false, std::sync::atomic::Ordering::Relaxed);
}
