mod arc;
pub mod error;
#[cfg(feature = "mmap")]
pub mod mmap;
#[cfg(feature = "postgres")]
pub mod postgres;
pub mod sqlite;
pub mod telemetry;
pub mod traits;
pub mod types;

pub use error::StoreError;
#[cfg(feature = "mmap")]
pub use mmap::MmapStore;
#[cfg(feature = "postgres")]
pub use postgres::PostgresStore;
pub use sqlite::SqliteStore;
pub use telemetry::{DURATION_BUCKETS_SECS, HistogramSnapshot, StoreTelemetry};
pub use traits::{BlockStore, CoinStore};
pub use types::{BatchHandle, BlockStatus, Savepoint};

// Drop `--` comment lines from a migration file so a ';' inside a comment can never cut a
// statement in half when the deferred index build splits the file into single statements.
pub(crate) fn strip_sql_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    for line in sql.lines() {
        if !line.trim_start().starts_with("--") {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}
