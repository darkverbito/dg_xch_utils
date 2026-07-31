pub mod full_node_responses;
pub mod generic;
pub mod simulator_responses;
pub mod wallet_responses;

use serde::{Deserialize, Serialize};
use std::sync::Arc;

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
