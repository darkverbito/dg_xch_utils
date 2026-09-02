//! The daemon's long-running halves as portfu tasks, plus the periodic work as
//! intervals. Registration is by annotation (inventory), so the server picks
//! these up without a call site in main; every fn dispatches through the
//! type-erased [`ActiveNode`](crate::service::ActiveNode).

use crate::service::{ActiveNode, NodeServices};
use log::info;
use portfu::prelude::{PortfuError, Server, State, interval, task};
use std::sync::atomic::Ordering;

/// The batch/bulk sync driver — the node's main loop.
#[task]
pub async fn sync_driver(
    active: State<ActiveNode>,
    services: State<NodeServices>,
) -> Result<(), PortfuError> {
    info!("sync driver task starting");
    active.0.run_sync_driver(&services.0).await;
    info!("sync driver task stopped");
    Ok(())
}

/// The event-driven near-tip follower (the short-sync band).
#[task]
pub async fn tip_follower(
    active: State<ActiveNode>,
    services: State<NodeServices>,
) -> Result<(), PortfuError> {
    info!("tip follower task starting");
    active.0.run_tip_follower(&services.0).await;
    info!("tip follower task stopped");
    Ok(())
}

/// Wallet-subscription disconnect hygiene: reconcile the coin-state
/// subscription registry against the live inbound peer set. One bounded pass
/// per tick (O(subscribers)).
#[interval(30_000)]
pub async fn wallet_subscription_reaper(
    active: State<ActiveNode>,
    services: State<NodeServices>,
) -> Result<(), PortfuError> {
    active.0.reap_wallet_subscriptions(&services.0).await;
    Ok(())
}

#[interval(500)]
pub async fn shutdown_bridge(
    active: State<ActiveNode>,
    server: State<Server>,
) -> Result<(), PortfuError> {
    if !server.0.run.load(Ordering::Relaxed) && active.0.is_running() {
        info!("server shutdown observed; draining the node");
        active.0.set_run(false);
    }
    Ok(())
}
