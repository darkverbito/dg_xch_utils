use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(TS, Clone, Deserialize, Serialize)]
#[ts(export)]
pub struct Profile {
    pub id: u64,
    pub name: String,
    pub image_hash: [u8; 32],
    pub description: String,
    pub key: Vec<u8>,
    pub derivations: u64,
}

#[derive(TS, Clone, Deserialize, Serialize)]
#[ts(export)]
pub struct ProfileList {
    pub profiles: Vec<Profile>,
}

#[derive(TS, Clone, Deserialize, Serialize)]
#[ts(export)]
pub struct RgbaImage {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}
