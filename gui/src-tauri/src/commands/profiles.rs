use chacha20poly1305::aead::Aead;
use chacha20poly1305::aead::generic_array::GenericArray;
use chacha20poly1305::consts::U32;
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use dg_xch_keys::{key_from_mnemonic_str, random_key};
use log::info;
use petname::Generator;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::SeedableRng;
use sha2::{Digest, Sha256};
use shared_types::{Profile, ProfileList, RgbaImage};
use std::fs;
use std::fs::File;
use std::io::Write;
use tauri::Manager;

use crate::utils::lifehash;

#[tauri::command]
pub fn list_profiles(app: tauri::AppHandle) -> Result<ProfileList, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let config_path = dir.join("profiles.json");
    info!("Loading profiles from {:?}", config_path);
    if config_path.exists() {
        serde_json::from_reader(fs::File::open(config_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())
    } else {
        Ok(ProfileList { profiles: vec![] })
    }
}

#[tauri::command]
pub fn image_lifehash(hash: [u8; 32]) -> Result<RgbaImage, String> {
    Ok(lifehash(hash.as_slice())?.to_owned())
}

#[tauri::command]
pub fn create_profile(
    app: tauri::AppHandle,
    mnemonic: Option<String>,
    password: String,
) -> Result<Profile, String> {
    let secret_key = match mnemonic {
        Some(a) => key_from_mnemonic_str(&a).map_err(|e| e.to_string())?,
        None => random_key().map_err(|e| e.to_string())?,
    };
    let key_hash: [u8; 32] = Sha256::digest(secret_key.to_bytes()).into();
    let pet_name = petname::Petnames::medium()
        .generate(&mut ChaCha20Rng::from_seed(key_hash), 3, "-")
        .ok_or("Failed to Generate Petname".to_string())?;
    let encryption_key: [u8; 32] = Sha256::digest(&password).into();
    let nonce: [u8; 24] = encryption_key[0..24].try_into().unwrap();
    let key: GenericArray<u8, U32> = GenericArray::<u8, U32>::from(encryption_key);
    let chacha_key = XChaCha20Poly1305::new(&key);
    let chacha_nonce = XNonce::from(nonce);
    let encrypted_key = chacha_key
        .encrypt(&chacha_nonce, secret_key.to_bytes().as_slice())
        .map_err(|e| e.to_string())?;
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let config_path = dir.join("profiles.json");
    info!("Loading profiles from {:?}", config_path);
    Ok(if !config_path.exists() {
        let mut file = File::create(&config_path).map_err(|e| e.to_string())?;
        let profile = Profile {
            id: 0,
            name: pet_name,
            image_hash: key_hash,
            description: "A Chia Profile!".to_string(),
            key: encrypted_key,
            derivations: 10,
        };
        file.write(
            serde_json::to_string_pretty(&ProfileList {
                profiles: vec![profile.clone()],
            })
            .map_err(|e| e.to_string())?
            .as_bytes(),
        )
        .map_err(|e| e.to_string())?;
        profile
    } else {
        let mut profiles = list_profiles(app).map_err(|e| e.to_string())?;
        let new_id = profiles.profiles.iter().map(|v| v.id).max().unwrap_or(0);
        let profile = Profile {
            id: new_id,
            name: pet_name,
            image_hash: key_hash,
            description: "A Chia Profile!".to_string(),
            key: encrypted_key,
            derivations: 10,
        };
        profiles.profiles.push(profile.clone());
        let mut file = File::create(&config_path).map_err(|e| e.to_string())?;
        file.write(
            serde_json::to_string_pretty(&profiles)
                .map_err(|e| e.to_string())?
                .as_bytes(),
        )
        .map_err(|e| e.to_string())?;
        profile
    })
}
