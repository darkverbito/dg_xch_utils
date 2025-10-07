use serde::{Deserialize, Serialize};
use dg_xch_core::blockchain::transaction_record::TransactionRecord;
use dg_xch_core::blockchain::wallet_balance::WalletBalance;
use dg_xch_core::blockchain::wallet_info::WalletInfo;
use dg_xch_core::blockchain::wallet_sync::WalletSync;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoginResp {
    pub fingerprint: u32,
    pub success: bool,
}
impl From<LoginResp> for u32 {
    fn from(a: LoginResp) -> Self {
        a.fingerprint
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SignedTransactionRecordResp {
    pub signed_tx: TransactionRecord,
    pub success: bool,
}
impl From<SignedTransactionRecordResp> for TransactionRecord {
    fn from(a: SignedTransactionRecordResp) -> Self {
        a.signed_tx
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TransactionRecordResp {
    pub transaction: TransactionRecord,
    pub success: bool,
}
impl From<TransactionRecordResp> for TransactionRecord {
    fn from(a: TransactionRecordResp) -> Self {
        a.transaction
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WalletBalanceResp {
    pub wallets: Vec<WalletBalance>,
    pub success: bool,
}
impl From<WalletBalanceResp> for Vec<WalletBalance> {
    fn from(a: WalletBalanceResp) -> Self {
        a.wallets
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WalletInfoResp {
    pub wallets: Vec<WalletInfo>,
    pub success: bool,
}

impl From<WalletInfoResp> for Vec<WalletInfo> {
    fn from(a: WalletInfoResp) -> Self {
        a.wallets
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WalletSyncResp {
    pub genesis_initialized: bool,
    pub synced: bool,
    pub syncing: bool,
    pub success: bool,
}
impl From<WalletSyncResp> for WalletSync {
    fn from(a: WalletSyncResp) -> Self {
        WalletSync {
            genesis_initialized: a.genesis_initialized,
            synced: a.synced,
            syncing: a.syncing,
        }
    }
}
