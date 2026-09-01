pub mod heartbeat;
pub mod run;
pub mod schema;
pub mod state;
pub mod transaction;

pub use heartbeat::DaemonBeat;
pub use state::{JobState, overdue_since};
pub use transaction::Transaction;

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;

pub struct Store {
    conn: Connection,
}

/// Generous: retries are cheap and the window is normally sub-millisecond.
const OPEN_RETRIES: u32 = 10;

impl Store {
    /// `busy_timeout` doesn't cover the first migration. Two processes can
    /// both reach `CREATE TABLE` on a brand-new database file. One gets
    /// `SQLITE_BUSY` right away, not after the busy handler waits.
    ///
    /// Each retry uses a fresh connection. A failed statement can leave
    /// the old one in an open, unresumable transaction.
    pub fn open(db_path: &Path) -> Result<Store> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        let mut last_err = None;
        for attempt in 0..OPEN_RETRIES {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(20 * u64::from(attempt)));
            }
            let conn = Connection::open(db_path)
                .with_context(|| format!("opening {}", db_path.display()))?;
            // set before `migrate`. A contended migration then waits on
            // the lock instead of failing.
            conn.busy_timeout(std::time::Duration::from_secs(5))?;
            match schema::migrate(&conn) {
                Ok(()) => return Ok(Store { conn }),
                Err(e) if is_database_busy(&e) => last_err = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("failed to open store")))
            .with_context(|| format!("initializing schema for {}", db_path.display()))
    }

    pub fn open_in_memory() -> Result<Store> {
        let conn = Connection::open_in_memory()?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        schema::migrate(&conn)?;
        Ok(Store { conn })
    }

    pub fn schema_version(&self) -> Result<i64> {
        schema::version(&self.conn)
    }

    /// Increments only on writes from another connection, not this one's
    /// own.
    pub fn data_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .pragma_query_value(None, "data_version", |r| r.get(0))?)
    }

    pub fn drop_table_for_testing(&self, table: &str) -> Result<()> {
        self.conn
            .execute_batch(&format!("DROP TABLE {table}"))
            .map_err(Into::into)
    }

    pub fn count_rows_for_testing(&self, table: &str) -> Result<i64> {
        Ok(self
            .conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?)
    }
}

pub fn is_database_busy(e: &anyhow::Error) -> bool {
    e.downcast_ref::<rusqlite::Error>()
        .and_then(rusqlite::Error::sqlite_error_code)
        .is_some_and(|c| c == rusqlite::ErrorCode::DatabaseBusy)
}
