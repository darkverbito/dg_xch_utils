use crate::{generate_rpc, ClientSSLConfig};
use crate::api::responses::wallet_responses::{
    LoginResp, SignedTransactionRecordResp, TransactionRecordResp, WalletBalanceResp,
    WalletInfoResp, WalletSyncResp,
};
use crate::rpc::{get_client, get_url};
use dg_xch_core::blockchain::announcement::Announcement;
use dg_xch_core::blockchain::coin::Coin;
use dg_xch_core::blockchain::transaction_record::TransactionRecord;
use dg_xch_core::blockchain::wallet_balance::WalletBalance;
use dg_xch_core::blockchain::wallet_info::WalletInfo;
use dg_xch_core::blockchain::wallet_sync::WalletSync;
use dg_xch_core::blockchain::wallet_type::AmountWithPuzzleHash;
use reqwest::Client;
use std::collections::HashMap;
use std::sync::Arc;
use crate::api::responses::UrlFunction;


pub struct WalletClient {
    client: Client,
    host: String,
    port: u16,
    additional_headers: Option<HashMap<String, String>>,
    url_function: UrlFunction,
}
impl WalletClient {
    #[must_use]
    pub fn new(
        host: &str,
        port: u16,
        timeout: u64,
        ssl_path: &Option<ClientSSLConfig>,
        additional_headers: Option<HashMap<String, String>>,
    ) -> Self {
        WalletClient {
            client: get_client(ssl_path, timeout).unwrap_or_default(),
            host: host.to_string(),
            port,
            additional_headers,
            url_function: Arc::new(get_url),
        }
    }
}

generate_rpc!(
    WalletAPI,
    WalletClient,
    [
        (log_in, [wallet_fingerprint: u32], LoginResp, u32),
        (log_in_and_skip, [wallet_fingerprint: u32], LoginResp, u32),
        (get_wallets, [wallet_fingerprint: u32], WalletInfoResp, Vec<WalletInfo>),
        (get_wallet_balance, [wallet_id: u32], WalletBalanceResp, Vec<WalletBalance>),
        (get_sync_status, [], WalletSyncResp, WalletSync),
        (send_transaction, [
            wallet_id: u32,
            amount: u64,
            address: String,
            fee: u64
        ], TransactionRecordResp, TransactionRecord),
        (send_transaction_multi, [
            wallet_id: u32,
            additions: Vec<AmountWithPuzzleHash>,
            fee: u64,
        ], TransactionRecordResp, TransactionRecord),
        (get_transaction, [
            wallet_id: u32,
            transaction_id: String,
        ], TransactionRecordResp, TransactionRecord),
        (create_signed_transaction, [
            wallet_id: u32,
            additions: Vec<Coin>,
            coins: Vec<Coin>,
            coin_announcements: Vec<Announcement>,
            puzzle_announcements: Vec<Announcement>,
            fee: u64,
        ], SignedTransactionRecordResp, TransactionRecord),
    ]
);