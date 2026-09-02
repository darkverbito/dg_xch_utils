use crate::service::ActiveNode;
use portfu::prelude::http::StatusCode;
use portfu::prelude::{PortfuError, Response, State, get};

#[get("/metrics")]
pub async fn metrics(active: State<ActiveNode>) -> Result<String, PortfuError> {
    Ok(active.0.metrics_text().await)
}

#[get("/health")]
pub async fn health(active: State<ActiveNode>) -> Result<Response, PortfuError> {
    let (status, body) = active.0.health_check().await;
    let code = if status.starts_with("200") {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    Ok(Response::from_status_and_message(code, body).content_type("text/plain; charset=utf-8"))
}

/// One profile of either kind at a time — a second request is refused, not queued.
#[cfg(feature = "profiling")]
static PROFILING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(feature = "profiling")]
#[get("/debug/flamegraph")]
pub async fn debug_flamegraph(active: State<ActiveNode>) -> Result<Response, PortfuError> {
    use crate::metrics::{FLAMEGRAPH_SECONDS, FLAMEGRAPH_TIMEOUT, profiling};
    use std::sync::atomic::Ordering;
    if !active.0.debug_endpoints() {
        return Ok(Response::from_status_and_message(
            StatusCode::NOT_FOUND,
            "/debug/flamegraph is disabled; start the node with --debug-endpoints to enable it",
        ));
    }
    if PROFILING.swap(true, Ordering::SeqCst) {
        return Ok(Response::from_status_and_message(
            StatusCode::SERVICE_UNAVAILABLE,
            "a debug profile is already running",
        ));
    }
    log::info!("flamegraph profiling started (~{FLAMEGRAPH_SECONDS}s)");
    let out = tokio::time::timeout(
        FLAMEGRAPH_TIMEOUT,
        profiling::sample_flamegraph(FLAMEGRAPH_SECONDS),
    )
    .await;
    PROFILING.store(false, Ordering::SeqCst);
    match out {
        Ok(Ok(svg)) => Ok(
            Response::from_status_and_message(StatusCode::OK, svg).content_type("image/svg+xml")
        ),
        Ok(Err(e)) => Ok(Response::from_status_and_message(
            StatusCode::INTERNAL_SERVER_ERROR,
            e,
        )),
        Err(_) => Ok(Response::from_status_and_message(
            StatusCode::GATEWAY_TIMEOUT,
            "flamegraph timed out",
        )),
    }
}

#[cfg(feature = "profiling")]
#[get("/debug/heap")]
pub async fn debug_heap(active: State<ActiveNode>) -> Result<Response, PortfuError> {
    use crate::metrics::{HEAP_DUMP_TIMEOUT, profiling};
    use std::sync::atomic::Ordering;
    if !active.0.debug_endpoints() {
        return Ok(Response::from_status_and_message(
            StatusCode::NOT_FOUND,
            "/debug/heap is disabled; start the node with --debug-endpoints to enable it",
        ));
    }
    if PROFILING.swap(true, Ordering::SeqCst) {
        return Ok(Response::from_status_and_message(
            StatusCode::SERVICE_UNAVAILABLE,
            "a debug profile is already running",
        ));
    }
    log::info!("heap-profile dump requested");
    let out = tokio::time::timeout(HEAP_DUMP_TIMEOUT, profiling::dump_heap_profile()).await;
    PROFILING.store(false, Ordering::SeqCst);
    match out {
        Ok(Ok(prof)) => {
            log::info!("heap profile dumped bytes={}", prof.len());
            Ok(Response::from_status_and_message(StatusCode::OK, prof)
                .content_type("application/octet-stream"))
        }
        Ok(Err(e)) => Ok(Response::from_status_and_message(
            StatusCode::INTERNAL_SERVER_ERROR,
            e,
        )),
        Err(_) => Ok(Response::from_status_and_message(
            StatusCode::GATEWAY_TIMEOUT,
            "heap dump timed out",
        )),
    }
}
