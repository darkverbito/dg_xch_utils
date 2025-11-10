use crate::secrets::NonceArray;
use crate::utils::{decrypt, encrypt, lifehash, load_config_file, petname, save_config_file};
use dg_xch_keys::{key_from_mnemonic_str, random_key};
use sha2::{Digest, Sha256};
use shared_types::{Profile, ProfileList, ProfileState, RgbaImage};
use tauri::Manager;

static PROFILES_FILENAME: &str = "profiles.json";

#[tauri::command]
pub fn list_profiles(app: tauri::AppHandle) -> Result<ProfileList, String> {
    if let Some(list) = load_config_file(app, PROFILES_FILENAME)? {
        Ok(list)
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
    let nonce: NonceArray = app.try_state().map(|v| *v).unwrap_or(NonceArray([0; 24]));
    let secret_key = match mnemonic {
        Some(a) => key_from_mnemonic_str(&a).map_err(|e| e.to_string())?,
        None => random_key().map_err(|e| e.to_string())?,
    };
    let key_hash: [u8; 32] = Sha256::digest(secret_key.to_bytes()).into();
    let pet_name = petname(&key_hash)?;
    let encrypted_key = encrypt(
        secret_key.to_bytes().as_slice(),
        password.as_bytes(),
        nonce.0,
    )?;
    let current_profiles: ProfileList = list_profiles(app.clone())?;
    let new_id = current_profiles
        .profiles
        .iter()
        .map(|v| v.id)
        .max()
        .unwrap_or(0);
    let profile = Profile {
        id: new_id,
        state: ProfileState::Encrypted,
        name: pet_name,
        image_hash: key_hash,
        description: "A Chia Profile!".to_string(),
        key: encrypted_key,
        derivations: 10,
    };
    save_config_file(app, PROFILES_FILENAME, &profile)?;
    Ok(profile)
}

#[tauri::command]
pub fn load_profile(
    app: tauri::AppHandle,
    profile_id: u64,
    password: String,
) -> Result<Profile, String> {
    let nonce: NonceArray = app.try_state().map(|v| *v).unwrap_or(NonceArray([0; 24]));
    let current_profiles: ProfileList = list_profiles(app.clone())?;
    let mut profile_to_load = match current_profiles
        .profiles
        .iter()
        .find(|v| v.id == profile_id)
    {
        Some(profile) => Ok(profile.clone()),
        None => Err("Profile not found".to_string()),
    }?;
    profile_to_load.key = decrypt(&profile_to_load.key, password.as_bytes(), nonce.0)?;
    Ok(profile_to_load)
}

#[tauri::command]
pub fn update_profile(
    app: tauri::AppHandle,
    profile: Profile,
    password: String,
) -> Result<Profile, String> {
    let nonce: NonceArray = app.try_state().map(|v| *v).unwrap_or(NonceArray([0; 24]));
    let mut current_profiles: ProfileList = list_profiles(app.clone())?;
    let decrypted = {
        let profile_to_update = current_profiles
            .profiles
            .iter_mut()
            .find(|v| v.id == profile.id)
            .ok_or("Profile not found".to_string())?;
        profile_to_update.name = profile.name;
        profile_to_update.description = profile.description;
        profile_to_update.derivations = profile.derivations;
        decrypt(&profile_to_update.key, password.as_bytes(), nonce.0)?
    };
    save_config_file(app, PROFILES_FILENAME, &current_profiles)?;
    let profile_to_update = current_profiles
        .profiles
        .iter_mut()
        .find(|v| v.id == profile.id)
        .ok_or("Profile not found".to_string())?;
    profile_to_update.key = decrypted;
    profile_to_update.state = ProfileState::Decrypted;
    Ok(profile_to_update.clone())
}

#[tauri::command]
pub fn delete_profile(
    app: tauri::AppHandle,
    profile_id: u64,
    password: String,
) -> Result<(), String> {
    let nonce: NonceArray = app.try_state().map(|v| *v).unwrap_or(NonceArray([0; 24]));
    let mut current_profiles: ProfileList = list_profiles(app.clone())?;
    let profile_to_delete = match current_profiles
        .profiles
        .iter()
        .find(|v| v.id == profile_id)
    {
        Some(profile) => Ok(profile),
        None => Err("Profile not found".to_string()),
    }?;
    decrypt(&profile_to_delete.key, password.as_bytes(), nonce.0)?;
    current_profiles.profiles.retain(|v| v.id != profile_id);
    save_config_file(app, PROFILES_FILENAME, &current_profiles)?;
    Ok(())
}
