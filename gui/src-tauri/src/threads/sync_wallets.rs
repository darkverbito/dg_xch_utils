use shared_types::Profile;
use std::collections::HashMap;
use std::io::Error;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

pub async fn sync_wallets(
    _loaded_profiles: Arc<Mutex<HashMap<u64, Profile>>>,
    _wallets: Arc<Mutex<HashMap<u64, Profile>>>,
    _run_handle: Arc<AtomicBool>,
) -> Result<(), Error> {
    Ok(())
}
