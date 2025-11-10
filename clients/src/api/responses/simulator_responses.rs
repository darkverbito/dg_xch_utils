use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AutoFarmResp {
    pub auto_farm_enabled: bool,
    pub success: bool,
}
