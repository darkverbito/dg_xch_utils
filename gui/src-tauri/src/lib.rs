use crate::threads::sync_wallets::sync_wallets;
use shared_types::Profile;
use std::collections::HashMap;
use std::io::Error;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use tauri::async_runtime::JoinHandle;
use tauri::{Manager, State};

pub mod commands;
pub mod secrets;
pub mod threads;
pub mod utils;

const SERVICE: &str = "garden.druid.gui";

type ThreadPool = Arc<Mutex<Vec<JoinHandle<Result<(), Error>>>>>;

pub struct ApplicationState {
    pub loaded_profiles: Arc<Mutex<HashMap<u64, Profile>>>,
    pub wallets: Arc<Mutex<HashMap<u64, Profile>>>,
    pub threads: ThreadPool,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let background_run_handle = Arc::new(AtomicBool::new(true));
    let state = ApplicationState {
        loaded_profiles: Arc::default(),
        wallets: Arc::default(),
        threads: Arc::default(),
    };
    tauri::Builder::default()
        .manage(state)
        .setup(|app| {
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }
            //Start Threads
            let state: State<ApplicationState> = app.state();
            match state.threads.lock() {
                Ok(mut threads) => {
                    let wallets = state.wallets.clone();
                    let loaded_profiles = state.loaded_profiles.clone();
                    threads.push(tauri::async_runtime::spawn(async move {
                        sync_wallets(loaded_profiles, wallets, background_run_handle).await
                    }));
                }
                Err(e) => {
                    panic!("State has Been Poisoned: {e:?}")
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::profiles::list_profiles,
            commands::profiles::create_profile,
            commands::profiles::image_lifehash,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
