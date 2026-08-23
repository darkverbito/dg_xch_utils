// The introducer seed must RETRY while the node is peer-starved.
//
// Production finding: the introducer seed ran exactly once at daemon boot; a
// boot-time failure (DNS not ready yet — "introducer seed failed") left the address book empty and
// the node peer-poor forever. chia never has this failure mode: FullNodeDiscovery._connect_to_peers
// (chia/server/node_discovery.py:256-292) re-queries the introducer on a doubling backoff
// (introducer_backoff 1s → ×2 → 300s cap) whenever peers are needed and the address pool cannot
// supply them, resetting the backoff once addresses flow again.
//
// The supervisor's introducer session mirrors that: retry while the outbound peer count is below
// target, backoff doubling from `retry_timeout` to the 300s cap, quiet once the target is met.

mod common;

use common::{fast_settings, free_port, peer, spawn_introducer_on, wait_until};
use dg_xch_p2p::Supervisor;
use std::sync::atomic::Ordering;
use std::time::Duration;

// RED against the one-shot boot seed: the introducer is unreachable when the supervisor starts
// (the DNS-not-ready analog), then comes up on the same address. A one-shot seed never looks
// again; the retry session must find it and fill the book.
#[tokio::test]
async fn boot_time_introducer_failure_recovers_when_the_introducer_appears() {
    let port = free_port(); // nothing listens here yet — the boot-time seed will fail
    let settings = fast_settings();
    let mut sup = Supervisor::new(settings);
    sup.start_introducer("127.0.0.1", port);

    // The boot attempt fails (connection refused); the book stays empty.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        sup.book.lock().await.is_empty(),
        "no introducer yet — the book must still be empty"
    );

    // The introducer becomes reachable on the SAME endpoint (DNS/service now ready).
    let (server, _queries) = spawn_introducer_on(
        port,
        vec![peer("1.1.1.1", 8444, 42), peer("2.2.2.2", 8444, 42)],
    )
    .await;

    let book = sup.book.clone();
    assert!(
        wait_until(
            || async { book.lock().await.len() == 2 },
            Duration::from_secs(15)
        )
        .await,
        "the introducer session must retry past the boot failure and seed the book \
         once the introducer is reachable (the one-shot seed never retried)"
    );

    sup.stop().await;
    server.run.store(false, Ordering::Relaxed);
}

// While the book can still supply a dial candidate the introducer is left alone — chia's
// `size == 0 or retry_introducers` gate (node_discovery.py:263): a non-empty address pool feeds
// the outbound slots first; only exhaustion re-opens the introducer. (Deliberately no
// `start_outbound` here: a real dial on the test host is the known t041 flake — the gate is
// what's under test, and it must hold with zero live outbound peers.)
#[tokio::test]
async fn introducer_is_quiet_while_the_book_can_supply_candidates() {
    let intro_port = free_port();
    let (intro, queries) = spawn_introducer_on(intro_port, vec![]).await;

    let mut sup = Supervisor::new(fast_settings());
    // The book holds a candidate (routability is the outbound slots' problem, not the seed's).
    sup.seed_addresses(&[peer("203.0.113.1", 8444, 1)]).await;
    sup.start_introducer("127.0.0.1", intro_port);

    // Several retry windows: below target (0 < 2) but the book is non-empty → no queries.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        queries.load(Ordering::Relaxed),
        0,
        "no introducer queries while the address book holds a candidate"
    );

    sup.stop().await;
    intro.run.store(false, Ordering::Relaxed);
}

// At (or above) the outbound target the introducer is never queried even with an empty book —
// chia resets its introducer backoff and stops querying once peers are no longer needed
// (node_discovery.py:385 `retry_introducers = False` when `extra_peers_needed == 0`).
#[tokio::test]
async fn introducer_is_quiet_once_the_outbound_target_is_met() {
    let intro_port = free_port();
    let (intro, queries) = spawn_introducer_on(intro_port, vec![]).await;

    // target_outbound = 0: the (empty) live outbound set already meets the target.
    let settings = dg_xch_p2p::P2pSettings {
        target_outbound: 0,
        ..fast_settings()
    };
    let mut sup = Supervisor::new(settings);
    sup.start_introducer("127.0.0.1", intro_port);

    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        queries.load(Ordering::Relaxed),
        0,
        "no introducer queries while the outbound target is met"
    );

    sup.stop().await;
    intro.run.store(false, Ordering::Relaxed);
}
