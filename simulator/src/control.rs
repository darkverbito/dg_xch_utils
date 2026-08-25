//! The plain-HTTP chain-control endpoint a wallet e2e harness drives, matching the chia 2.7.1
//! simulator's nginx-fronted control API. `POST /farm_block { address, guarantee_tx_block }` farms a
//! block whose reward pays `address`; `POST /get_blockchain_state` reports the peak. This is the
//! interface the `sage-sim-harness` external-peer mode funds and advances the chain through, distinct
//! from the mTLS peer protocol the wallet syncs over.

use crate::server::{SharedChain, farm_reward_blocks};
use dg_xch_keys::decode_puzzle_hash;
use dg_xch_stores::SqliteStore;
use dg_xch_stores::traits::BlockStore;
use full_node::Node;
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::net::TcpListener;

/// Start the plain-HTTP control server on `addr`. Returns its run flag; clearing it stops the loop.
pub(crate) fn spawn(
    addr: SocketAddr,
    chain: SharedChain,
    node: Arc<Node<SqliteStore>>,
) -> Arc<AtomicBool> {
    let run = Arc::new(AtomicBool::new(true));
    let run_c = run.clone();
    tokio::spawn(async move {
        let listener = match TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                log::error!("sim control server failed to bind {addr}: {e}");
                return;
            }
        };
        while run_c.load(Ordering::Relaxed) {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let chain = chain.clone();
            let node = node.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req| handle(req, chain.clone(), node.clone()));
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    run
}

async fn handle(
    req: Request<Incoming>,
    chain: SharedChain,
    node: Arc<Node<SqliteStore>>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let path = req.uri().path().to_string();
    let body = req
        .into_body()
        .collect()
        .await
        .map(|b| b.to_bytes())
        .unwrap_or_default();
    let (status, payload) = match path.as_str() {
        "/farm_block" => match farm_block(&body, &chain, &node).await {
            Ok(v) => (StatusCode::OK, v),
            Err(e) => (StatusCode::OK, json!({ "success": false, "error": e })),
        },
        "/get_blockchain_state" | "/height" => match blockchain_state(&node).await {
            Ok(v) => (StatusCode::OK, v),
            Err(e) => (StatusCode::OK, json!({ "success": false, "error": e })),
        },
        _ => (
            StatusCode::NOT_FOUND,
            json!({ "success": false, "error": format!("unknown endpoint {path}") }),
        ),
    };
    let mut response = Response::new(Full::new(Bytes::from(payload.to_string())));
    *response.status_mut() = status;
    response.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/json"),
    );
    Ok(response)
}

async fn farm_block(
    body: &[u8],
    chain: &SharedChain,
    node: &Node<SqliteStore>,
) -> Result<Value, String> {
    let req: Value = serde_json::from_slice(body).map_err(|e| format!("bad body: {e}"))?;
    let address = req
        .get("address")
        .and_then(Value::as_str)
        .ok_or("missing address")?;
    let ph = decode_puzzle_hash(address).map_err(|e| format!("bad address: {e}"))?;
    // A count of blocks to farm (chia's `blocks`, default 1). `guarantee_tx_block` is always honored:
    // the simulator only farms transaction blocks.
    let blocks = req
        .get("blocks")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(1);
    farm_reward_blocks(chain, node, ph, blocks)
        .await
        .map_err(|e| e.to_string())?;
    let height = peak_height(node).await;
    Ok(json!({ "success": true, "new_peak_height": height }))
}

async fn blockchain_state(node: &Node<SqliteStore>) -> Result<Value, String> {
    let height = peak_height(node).await;
    Ok(json!({
        "success": true,
        "blockchain_state": { "peak": { "height": height } },
        "height": height,
    }))
}

async fn peak_height(node: &Node<SqliteStore>) -> u32 {
    node.store
        .get_peak()
        .await
        .ok()
        .flatten()
        .map_or(0, |(_, h)| h)
}
