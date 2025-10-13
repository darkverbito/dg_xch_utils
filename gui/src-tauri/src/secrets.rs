use crate::SERVICE;
use keyring::Entry;

#[derive(Copy, Clone)]
pub struct NonceArray(pub [u8; 24]);

pub fn set_secret(key: &str, value: &str) -> Result<(), String> {
    Entry::new(SERVICE, key)
        .and_then(|e| e.set_password(value))
        .map_err(|e| e.to_string())
}

pub fn get_secret(key: &str) -> Result<Option<String>, String> {
    let entry = Entry::new(SERVICE, key).map_err(|e| e.to_string())?;
    match entry.get_password() {
        Ok(v) => Ok(Some(v)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

pub fn delete_secret(key: &str) -> Result<(), String> {
    match Entry::new(SERVICE, key).and_then(|e| e.delete_credential()) {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}
