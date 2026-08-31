//! WebSocket endpoints on the portfu server: the live log stream (the Druid
//! Garden pattern — subscribe to the logger's bus, filter by level) and a
//! lightweight sync-status stream for dashboards.

use crate::service::ActiveNode;
use dg_logger::DruidGardenLogger;
use log::{Level, debug};
use portfu::prelude::{Message, Path, PortfuError, State, WebSocket, websocket};
use std::str::FromStr;

/// Stream log events at or above `{level}` as JSON, one message per event.
/// The logger's bus is synchronous, so a blocking thread bridges it into an
/// async channel; dropping the socket drops the receiver and the bridge ends
/// on its next send.
#[websocket("/ws/logs/{level}")]
pub async fn log_stream(
    socket: WebSocket,
    level: Path,
    logger: State<DruidGardenLogger>,
) -> Result<(), PortfuError> {
    let level = level.inner();
    let level = Level::from_str(level.as_str())
        .map_err(|e| PortfuError::Parsing(format!("{level} is not a valid log level: {e:?}")))?;
    let mut bus = logger.0.subscribe();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<dg_logger::LogEvent>(256);
    std::thread::spawn(move || {
        while let Ok(event) = bus.recv() {
            if tx.blocking_send(event).is_err() {
                break;
            }
        }
    });
    loop {
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else { break };
                if event.level <= level {
                    let json = serde_json::to_string(&event)
                        .map_err(|e| PortfuError::Internal(format!("serialize log event: {e}")))?;
                    if socket.send_text(json).await.is_err() {
                        break;
                    }
                }
            }
            msg = socket.next_message() => {
                match msg {
                    Ok(Some(Message::Ping(data))) => {
                        let _ = socket.send(Message::Pong(data)).await;
                    }
                    Ok(Some(Message::Close(_))) | Ok(None) | Err(_) => break,
                    Ok(Some(_)) => {}
                }
            }
        }
    }
    debug!("log stream closed");
    Ok(())
}

/// Push the sync status every two seconds: confirmed peak, claimed tip, and
/// the health verdict — enough for a live dashboard without polling /metrics.
#[websocket("/ws/status")]
pub async fn status_stream(
    socket: WebSocket,
    active: State<ActiveNode>,
) -> Result<(), PortfuError> {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if socket.send_text(active.0.status_json().await).await.is_err() {
                    break;
                }
            }
            msg = socket.next_message() => {
                match msg {
                    Ok(Some(Message::Ping(data))) => {
                        let _ = socket.send(Message::Pong(data)).await;
                    }
                    Ok(Some(Message::Close(_))) | Ok(None) | Err(_) => break,
                    Ok(Some(_)) => {}
                }
            }
        }
    }
    Ok(())
}
