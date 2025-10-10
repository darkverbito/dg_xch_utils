pub mod wallet_responses;
pub mod full_node_responses;
pub mod generic;
pub mod simulator_responses;

use std::sync::Arc;
use serde::{Deserialize, Serialize};

pub type UrlFunction = Arc<dyn Fn(&str, u16, &str) -> String + Send + Sync + 'static>;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EmptyResponse {
    pub success: bool,
}

// to phase out
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InitialFreezePeriodResp {
    pub initial_freeze_end_timestamp: u64,
    pub success: bool,
}